// SPDX-License-Identifier: GPL-3.0-or-later
//! The `--coverage` analysis: what each project script *uses* versus what the
//! engine *supports*.
//!
//! Before a run, a user wants to know which parts of their project the evaluator
//! implements, which it will only stub (Tier-3 IO, externally
//! driven), and which it cannot handle at all (and would fail loud on). This
//! module walks every script's CST and answers that, statically:
//!
//! - every call is resolved through the same project-aware capability model as
//!   runtime dispatch — supported, assumed, stubbed, or unsupported;
//! - every statement/expression construct `Kind` is classified against the set
//!   the evaluator implements.
//!
//! The result is a [`CoverageReport`] of de-duplicated, sorted entries — pure
//! data, no `m1-core`/`m1-typecheck` types — that the CLI prints and the `Engine`
//! facade returns.

use crate::builtins::{BuiltinSupport, CapabilityScope, classify_bare_call, classify_member_call};
use crate::loader::Loaded;
use m1_core::{Field, Kind, Node};
use m1_typecheck::Project;
use m1_typecheck::parsed::ParsedScript;
use std::collections::{BTreeSet, HashMap};

/// One thing a script uses, with where it was found. `name` is a `Object.Method`
/// for a builtin call or a construct kind for a language construct.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CoverageItem {
    /// What is used: `"Calculate.Max"`, `"Integral.Normal"`, `"IfStatement"`, …
    pub name: String,
    /// Whether it is a builtin call or a language construct.
    pub kind: ItemKind,
}

/// Whether a coverage item is a builtin call or a language construct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ItemKind {
    /// An `Object.Method(...)` builtin call.
    Builtin,
    /// A language statement/expression construct.
    Construct,
}

/// The coverage analysis result: which used items are supported, stubbed, or
/// unsupported. Each list is de-duplicated and sorted for a deterministic report.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CoverageReport {
    /// Items dispatched through a direct evaluator implementation.
    pub supported: Vec<CoverageItem>,
    /// Items dispatched through an explicit deterministic offline model.
    pub assumed: Vec<CoverageItem>,
    /// Items handled as documented/scenario-fed stubs (Tier-3 IO).
    pub stubbed: Vec<CoverageItem>,
    /// Items the engine does not handle (would fail loud at runtime).
    pub unsupported: Vec<CoverageItem>,
    /// The whole-project execution schedule: one `(function symbol, rate)` entry
    /// per script-backed function. `Some(hz)` is the function's periodic rate (it
    /// runs that many times per second in whole-project mode); `None` flags a
    /// function with no resolvable periodic trigger. Startup functions are
    /// identified separately in [`CoverageReport::startup`]; other `None` entries
    /// are not run by the whole-project scheduler. Sorted `(rate descending,
    /// function symbol)` for a deterministic report; empty when the analysis had
    /// no [`Project`] to resolve rates from.
    pub schedule: Vec<(String, Option<f64>)>,
    /// Functions the whole-project runner executes exactly once before the
    /// periodic loop. These also appear in `schedule` with no periodic rate.
    pub startup: Vec<String>,
}

impl CoverageReport {
    /// Analyse every script in `scripts`, producing a combined report. No project
    /// context: a member-expression callee is classified on its `(object, method)`
    /// spelling alone, so a user-function call whose method name collides with an
    /// IO-stub method (e.g. `Update`) cannot be distinguished from the stub. Use
    /// [`CoverageReport::analyse_in`] for project-accurate receiver and
    /// user-function coverage.
    pub fn analyse(scripts: &[ParsedScript]) -> CoverageReport {
        Self::analyse_with_startup(scripts, None, &[])
    }

    /// Analyse every script with optional project context. When a [`Project`] is
    /// given, a `CallExpression` whose member-expression callee resolves to a user
    /// `Function`/`Method` symbol is reported **Supported** (it is evaluated inline
    /// — P15-D), disambiguating it from a same-named IO-stub method (`Service
    /// Bits.Update` vs `Slip Control.Update`). Runtime dispatch and coverage both
    /// use the same receiver-aware capability classifier.
    pub fn analyse_in(scripts: &[ParsedScript], project: Option<&Project>) -> CoverageReport {
        Self::analyse_with_startup(scripts, project, &[])
    }

    /// Analyse a complete loaded project, including the loader's exact startup
    /// trigger set. This is the path used by [`crate::Engine::coverage`].
    pub fn analyse_loaded(loaded: &Loaded) -> CoverageReport {
        Self::analyse_with_startup(
            &loaded.scripts,
            Some(&loaded.project),
            &loaded.startup_fn_symbols,
        )
    }

    fn analyse_with_startup(
        scripts: &[ParsedScript],
        project: Option<&Project>,
        startup_fn_symbols: &[String],
    ) -> CoverageReport {
        let mut supported = BTreeSet::new();
        let mut assumed = BTreeSet::new();
        let mut stubbed = BTreeSet::new();
        let mut unsupported = BTreeSet::new();
        for script in scripts {
            // The script's enclosing group, for resolving group-relative callees.
            let group = project.and_then(|p| p.group_for_script(&script.name));
            let fn_symbol = project.and_then(|p| p.function_symbol_for_script(&script.name));
            let cx = WalkCtx {
                project,
                group: group.as_deref(),
                fn_symbol: fn_symbol.as_deref(),
                scripts,
            };
            let mut locals = HashMap::new();
            walk(
                &script.cst.root(),
                &cx,
                &mut locals,
                &mut supported,
                &mut assumed,
                &mut stubbed,
                &mut unsupported,
            );
        }
        // The public item identity is its displayed `(name, kind)`, so occurrences
        // in different scripts collapse into one line. Keep the weakest capability
        // seen for that identity. A supported occurrence must never hide another
        // occurrence that will be assumed, stubbed, or rejected at runtime.
        stubbed.retain(|i| !unsupported.contains(i));
        assumed.retain(|i| !unsupported.contains(i) && !stubbed.contains(i));
        supported
            .retain(|i| !unsupported.contains(i) && !stubbed.contains(i) && !assumed.contains(i));
        CoverageReport {
            supported: supported.into_iter().collect(),
            assumed: assumed.into_iter().collect(),
            stubbed: stubbed.into_iter().collect(),
            unsupported: unsupported.into_iter().collect(),
            schedule: build_schedule(scripts, project),
            startup: build_startup(scripts, project, startup_fn_symbols),
        }
    }

    /// A human-readable, deterministic summary for the CLI. One section per
    /// bucket, each line `kind: name`. Empty buckets are still labelled so the
    /// output shape is stable.
    pub fn render(&self) -> String {
        let mut out = String::new();
        render_section(&mut out, "Supported", &self.supported);
        render_section(&mut out, "Assumed", &self.assumed);
        render_section(&mut out, "Stubbed", &self.stubbed);
        render_section(&mut out, "Unsupported", &self.unsupported);
        render_schedule(&mut out, &self.schedule, &self.startup);
        out
    }
}

/// Append the `Schedule:` section: one line per function with its periodic rate,
/// startup execution, or unscheduled status. An empty schedule still prints the
/// label so the output shape is stable.
fn render_schedule(out: &mut String, schedule: &[(String, Option<f64>)], startup: &[String]) {
    out.push_str("Schedule:\n");
    if schedule.is_empty() {
        out.push_str("  (none)\n");
        return;
    }
    for (function, rate) in schedule {
        match rate {
            Some(hz) => out.push_str(&format!("  {function} @ {hz} Hz\n")),
            None if startup.binary_search(function).is_ok() => {
                out.push_str(&format!("  {function} (startup, runs once)\n"));
            }
            None => out.push_str(&format!("  {function} (unscheduled)\n")),
        }
    }
}

/// Derive the whole-project execution schedule: one `(function symbol, rate)`
/// entry per script-backed function. Mirrors `runner::enumerate_scheduled`'s rate
/// derivation (`function_symbol_for_script` → `symbols().get(..).call_rate_hz`)
/// but keeps the **unscheduled** (`None`) functions too, so the report can flag
/// them. Sorted `(rate descending, function symbol)` for determinism; empty when
/// no [`Project`] is available to resolve rates.
fn build_schedule(
    scripts: &[ParsedScript],
    project: Option<&Project>,
) -> Vec<(String, Option<f64>)> {
    let Some(project) = project else {
        return Vec::new();
    };
    let mut schedule: Vec<(String, Option<f64>)> = scripts
        .iter()
        .filter_map(|script| {
            // A script without a backing function symbol is not a scheduled
            // function (e.g. a non-function script) — skip it entirely.
            let fn_symbol = project.function_symbol_for_script(&script.name)?;
            let rate_hz = project
                .symbols()
                .get(&fn_symbol)
                .and_then(|s| s.call_rate_hz);
            Some((fn_symbol, rate_hz))
        })
        .collect();
    // Fastest-first, ties broken by the function symbol path. A `None` rate sorts
    // last (unscheduled functions after every periodic one).
    schedule.sort_by(|a, b| {
        rate_sort_key(b.1)
            .partial_cmp(&rate_sort_key(a.1))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    schedule
}

/// Keep the loader's exact `On Startup` set, restricted to functions that have
/// a discovered backing script. Sorted for deterministic rendering and binary
/// lookup in [`render_schedule`].
fn build_startup(
    scripts: &[ParsedScript],
    project: Option<&Project>,
    startup_fn_symbols: &[String],
) -> Vec<String> {
    let Some(project) = project else {
        return Vec::new();
    };
    let script_functions: BTreeSet<String> = scripts
        .iter()
        .filter_map(|script| project.function_symbol_for_script(&script.name))
        .collect();
    let mut startup: Vec<String> = startup_fn_symbols
        .iter()
        .filter(|function| script_functions.contains(*function))
        .cloned()
        .collect();
    startup.sort();
    startup.dedup();
    startup
}

/// Sort key that places a periodic rate by its Hz (descending when compared
/// reversed) and an unscheduled `None` last (treated as the lowest rate).
fn rate_sort_key(rate: Option<f64>) -> f64 {
    rate.unwrap_or(f64::NEG_INFINITY)
}

/// Append one labelled section of items to `out`.
fn render_section(out: &mut String, label: &str, items: &[CoverageItem]) {
    out.push_str(label);
    out.push_str(":\n");
    if items.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for item in items {
            let tag = match item.kind {
                ItemKind::Builtin => "builtin",
                ItemKind::Construct => "construct",
            };
            out.push_str(&format!("  {tag} {}\n", item.name));
        }
    }
}

/// Language construct kinds the evaluator implements (statements + control flow).
/// Kept in sync with `stmt::exec`'s match arms; an unlisted statement-level kind
/// is reported unsupported.
const SUPPORTED_CONSTRUCTS: &[Kind] = &[
    Kind::AssignmentStatement,
    Kind::ExpressionStatement,
    Kind::LocalDeclaration,
    Kind::IfStatement,
    Kind::WhenStatement,
    Kind::ExpandStatement,
    Kind::Block,
    Kind::EmptyStatement,
];

/// Statement-ish kinds we report on for coverage. Pure expression nodes
/// (`BinaryExpression`, `Number`, …) and structural nodes (`SourceFile`,
/// `ArgumentList`, field punctuation) are not interesting to the report, so we
/// only classify the control/statement constructs a user would recognise.
fn is_reportable_construct(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::AssignmentStatement
            | Kind::ExpressionStatement
            | Kind::LocalDeclaration
            | Kind::IfStatement
            | Kind::WhenStatement
            | Kind::ExpandStatement
    )
}

/// The per-script context the coverage walk carries, used to resolve each call's
/// receiver and any script-backed user function exactly as runtime dispatch does.
struct WalkCtx<'a> {
    project: Option<&'a Project>,
    group: Option<&'a str>,
    fn_symbol: Option<&'a str>,
    scripts: &'a [ParsedScript],
}

/// Recursively walk a node, bucketing builtin calls and reportable constructs.
fn walk(
    node: &Node,
    cx: &WalkCtx,
    locals: &mut HashMap<String, crate::value::Value>,
    supported: &mut BTreeSet<CoverageItem>,
    assumed: &mut BTreeSet<CoverageItem>,
    stubbed: &mut BTreeSet<CoverageItem>,
    unsupported: &mut BTreeSet<CoverageItem>,
) {
    // Calls use exactly the same project-aware capability model as runtime.
    if node.kind() == Kind::CallExpression
        && let Some(callee) = node.child_by_field(Field::Function)
    {
        let scope = CapabilityScope {
            project: cx.project,
            group: cx.group,
            fn_symbol: cx.fn_symbol,
            locals: Some(locals),
            scripts: cx.scripts,
        };
        let classified = match callee.kind() {
            Kind::MemberExpression => call_object_method(node).map(|(object, method)| {
                let name = format!("{object}.{method}");
                let support = classify_member_call(&object, &method, &scope).support;
                (name, support)
            }),
            Kind::Identifier => {
                let name = callee.text().to_string();
                let support = classify_bare_call(&name, &scope).support;
                Some((name, support))
            }
            _ => None,
        };
        if let Some((name, support)) = classified {
            let item = CoverageItem {
                name,
                kind: ItemKind::Builtin,
            };
            match support {
                BuiltinSupport::Direct => supported.insert(item),
                BuiltinSupport::Modeled => assumed.insert(item),
                BuiltinSupport::Stubbed => stubbed.insert(item),
                BuiltinSupport::Unsupported => unsupported.insert(item),
            };
        }
    }

    // Reportable language constructs.
    if is_reportable_construct(node.kind()) {
        let item = CoverageItem {
            name: node.kind_str().to_string(),
            kind: ItemKind::Construct,
        };
        if SUPPORTED_CONSTRUCTS.contains(&node.kind()) {
            supported.insert(item);
        } else {
            unsupported.insert(item);
        }
    }

    // Runtime evaluates a local declaration's initializer before adding the
    // local to the active frame. Mirror that source-order behavior so subsequent
    // bare and member calls see the same shadowing that dispatch does.
    if node.kind() == Kind::LocalDeclaration {
        for child in node.named_children() {
            walk(&child, cx, locals, supported, assumed, stubbed, unsupported);
        }
        let is_static = node
            .children()
            .iter()
            .any(|child| child.kind() == Kind::Static);
        if !is_static && let Some(name) = node.child_by_field(Field::Name) {
            locals.insert(
                name.text().trim().to_string(),
                crate::value::Value::Bool(false),
            );
        }
        return;
    }

    for child in node.named_children() {
        walk(&child, cx, locals, supported, assumed, stubbed, unsupported);
    }
}

/// Extract `(object, method)` from a `CallExpression` whose callee is a member
/// expression `Object.Method`. Mirrors `expr::eval_call`: the object is the
/// callee's `Object` field text (flattened for a nested member), the method its
/// `Property` field text. A bare-identifier callee (user-function call) yields
/// `None` — it is not a builtin and out of Phase-1 scope.
fn call_object_method(node: &Node) -> Option<(String, String)> {
    let callee = node.child_by_field(Field::Function)?;
    if callee.kind() != Kind::MemberExpression {
        return None;
    }
    let object_node = callee.child_by_field(Field::Object)?;
    let method_node = callee.child_by_field(Field::Property)?;
    let object = match object_node.kind() {
        Kind::MemberExpression => crate::expr::flatten_member(&object_node).ok()?,
        _ => object_node.text().to_string(),
    };
    Some((object, method_node.text().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use m1_typecheck::parsed::parse_all;

    fn scripts_from(src: &str) -> Vec<ParsedScript> {
        parse_all(&[("Demo.Update.m1scr".to_string(), src.to_string())])
    }

    #[test]
    fn assumptions_stubs_and_unresolved_receivers_are_distinct() {
        let src = r#"
local i = Integral.Normal(Speed, 0.0, 100.0, false, 0.0);
local t = Demo.Map.Lookup(Speed, Load);
local c = CanComms.GetFloat(1, 2);
Output = i;
"#;
        let scripts = scripts_from(src);
        let report = CoverageReport::analyse(&scripts);

        let assumed: Vec<&str> = report.assumed.iter().map(|i| i.name.as_str()).collect();
        assert!(assumed.contains(&"Integral.Normal"), "{assumed:?}");

        let stub_names: Vec<&str> = report.stubbed.iter().map(|i| i.name.as_str()).collect();
        assert!(stub_names.contains(&"CanComms.GetFloat"), "{stub_names:?}");

        // Without a project, coverage cannot prove that Demo.Map is a table.
        let unsupported: Vec<&str> = report.unsupported.iter().map(|i| i.name.as_str()).collect();
        assert!(unsupported.contains(&"Demo.Map.Lookup"), "{unsupported:?}");
    }

    #[test]
    fn table_lookup_and_arbitrary_set_use_the_resolved_receiver_kind() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini");
        let loaded = crate::loader::load(
            &dir.join("Project.m1prj"),
            Some(&dir.join("parameters.m1cfg")),
        )
        .expect("mini fixture loads");
        let scripts =
            scripts_from("local x = Map.Lookup(Speed, Speed);\nMap.Set(1);\nOutput = x;\n");
        let report = CoverageReport::analyse_in(&scripts, Some(&loaded.project));

        let assumed: Vec<&str> = report.assumed.iter().map(|i| i.name.as_str()).collect();
        assert!(assumed.contains(&"Map.Lookup"), "{assumed:?}");
        let unsupported: Vec<&str> = report.unsupported.iter().map(|i| i.name.as_str()).collect();
        assert!(unsupported.contains(&"Map.Set"), "{unsupported:?}");
    }

    #[test]
    fn set_is_supported_for_channels_and_stubbed_for_output_objects() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/enums");
        let loaded =
            crate::loader::load(&dir.join("Project.m1prj"), None).expect("enums fixture loads");
        let scripts = scripts_from("Precharge State.Set(1);\nFan Output.Set(1);\n");
        let report = CoverageReport::analyse_in(&scripts, Some(&loaded.project));

        assert!(
            report
                .supported
                .iter()
                .any(|item| item.name == "Precharge State.Set")
        );
        assert!(
            report
                .stubbed
                .iter()
                .any(|item| item.name == "Fan Output.Set")
        );
    }

    #[test]
    fn implemented_debounce_filter_is_reported_as_an_assumption() {
        let report =
            CoverageReport::analyse(&scripts_from("Output = Debounce.Filter(true, 0.1);\n"));
        let assumed: Vec<&str> = report.assumed.iter().map(|i| i.name.as_str()).collect();
        assert!(assumed.contains(&"Debounce.Filter"), "{assumed:?}");
        assert!(
            !report
                .unsupported
                .iter()
                .any(|item| item.name == "Debounce.Filter")
        );
    }

    #[test]
    fn local_named_calculate_does_not_change_builtin_coverage() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini");
        let loaded =
            crate::loader::load(&dir.join("Project.m1prj"), None).expect("mini fixture loads");
        let scripts = scripts_from("local Calculate = 0;\nOutput = Calculate.Max(1, 2);\n");
        let report = CoverageReport::analyse_in(&scripts, Some(&loaded.project));

        assert!(
            report
                .supported
                .iter()
                .any(|item| item.name == "Calculate.Max"),
            "{report:?}"
        );
        assert!(
            !report
                .unsupported
                .iter()
                .any(|item| item.name == "Calculate.Max"),
            "{report:?}"
        );
    }

    #[test]
    fn library_qualified_calls_use_the_same_capability_buckets() {
        let report = CoverageReport::analyse(&scripts_from(
            "Library.Calculate.Max(1, 2);\n\
             Library.Debounce.Filter(true, 0.1);\n\
             Library.Math.fabs(-1.0);\n\
             Library.CanComms.GetFloat(1, 2);\n",
        ));

        assert!(
            report
                .supported
                .iter()
                .any(|item| item.name == "Library.Calculate.Max"),
            "{report:?}"
        );
        for name in ["Library.Debounce.Filter", "Library.Math.fabs"] {
            assert!(
                report.assumed.iter().any(|item| item.name == name),
                "{name}: {report:?}"
            );
        }
        assert!(
            report
                .stubbed
                .iter()
                .any(|item| item.name == "Library.CanComms.GetFloat"),
            "{report:?}"
        );
    }

    #[test]
    fn duplicate_display_name_keeps_the_weakest_script_capability() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/userfn");
        let loaded =
            crate::loader::load(&dir.join("Project.m1prj"), None).expect("userfn fixture loads");
        let scripts = parse_all(&[
            (
                "Caller.Update.m1scr".to_string(),
                "Output.Set(1);\n".to_string(),
            ),
            (
                "Helper.Compute.m1scr".to_string(),
                "Output.Set(1);\n".to_string(),
            ),
        ]);
        let report = CoverageReport::analyse_in(&scripts, Some(&loaded.project));

        assert!(
            report.stubbed.iter().any(|item| item.name == "Output.Set"),
            "the unresolved Helper occurrence must remain visible: {report:?}"
        );
        assert!(
            !report
                .supported
                .iter()
                .any(|item| item.name == "Output.Set"),
            "the supported Caller occurrence must not hide the stub: {report:?}"
        );
    }

    #[test]
    fn duplicate_display_name_keeps_unsupported_over_supported() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini");
        let loaded = crate::loader::load(
            &dir.join("Project.m1prj"),
            Some(&dir.join("parameters.m1cfg")),
        )
        .expect("mini fixture loads");
        let scripts = parse_all(&[
            ("Demo.Update.m1scr".to_string(), "Map.Get(0);\n".to_string()),
            (
                "Detached.Update.m1scr".to_string(),
                "Map.Get(0);\n".to_string(),
            ),
        ]);
        let report = CoverageReport::analyse_in(&scripts, Some(&loaded.project));

        assert!(
            report.unsupported.iter().any(|item| item.name == "Map.Get"),
            "the unresolved occurrence must remain visible: {report:?}"
        );
        assert!(
            !report.supported.iter().any(|item| item.name == "Map.Get"),
            "the resolved table occurrence must not hide the failure: {report:?}"
        );
    }

    #[test]
    fn unimplemented_builtin_is_unsupported() {
        // `Calculate.NoSuchMethod` is not in the dispatch table.
        let src = "Output = Calculate.NoSuchMethod(1);\n";
        let scripts = scripts_from(src);
        let report = CoverageReport::analyse(&scripts);
        let names: Vec<&str> = report.unsupported.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"Calculate.NoSuchMethod"), "{names:?}");
    }

    #[test]
    fn statement_constructs_are_reported_supported() {
        let src =
            "local x = 1;\nif (Speed > 0.0)\n{\n\tOutput = 1.0;\n}\nelse\n{\n\tOutput = 0.0;\n}\n";
        let scripts = scripts_from(src);
        let report = CoverageReport::analyse(&scripts);
        let constructs: Vec<&str> = report
            .supported
            .iter()
            .filter(|i| i.kind == ItemKind::Construct)
            .map(|i| i.name.as_str())
            .collect();
        // The if-statement and assignment constructs are recognised + supported.
        assert!(
            constructs.iter().any(|c| c.contains("if")),
            "{constructs:?}"
        );
        assert!(
            constructs
                .iter()
                .any(|c| c.contains("assignment") || c.contains("Assignment")),
            "{constructs:?}"
        );
    }

    #[test]
    fn user_function_call_is_supported_not_stubbed() {
        // A member-expression callee that resolves to a user `Function`/`Method`
        // symbol is evaluated inline (P15-D) — Supported — even though its method
        // name (`Update`) collides with the `Service Bits.Update` GroupCompound IO
        // stub. The coverage walk must resolve the callee against the project to
        // disambiguate, mirroring `eval_call` (which tries `userfn::call` first).
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/userfn");
        let loaded =
            crate::loader::load(&dir.join("Project.m1prj"), None).expect("userfn fixture loads");
        let report = CoverageReport::analyse_in(&loaded.scripts, Some(&loaded.project));

        let supported: Vec<&str> = report.supported.iter().map(|i| i.name.as_str()).collect();
        assert!(
            supported.contains(&"Helper.Compute"),
            "Helper.Compute should be Supported, got supported={supported:?}"
        );
        // It must NOT appear in the unsupported or stubbed buckets.
        let unsupported: Vec<&str> = report.unsupported.iter().map(|i| i.name.as_str()).collect();
        let stubbed: Vec<&str> = report.stubbed.iter().map(|i| i.name.as_str()).collect();
        assert!(
            !unsupported.contains(&"Helper.Compute"),
            "unsupported={unsupported:?}"
        );
        assert!(!stubbed.contains(&"Helper.Compute"), "stubbed={stubbed:?}");
    }

    #[test]
    fn local_shadowing_keeps_coverage_aligned_with_runtime_resolution() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/userfn");
        let loaded =
            crate::loader::load(&dir.join("Project.m1prj"), None).expect("userfn fixture loads");
        let mut sources: Vec<(String, String)> = loaded
            .scripts
            .iter()
            .filter(|script| script.name != "Caller.Update.m1scr")
            .map(|script| (script.name.clone(), script.cst.root().text().to_string()))
            .collect();
        sources.push((
            "Caller.Update.m1scr".to_string(),
            "local Kick = 0;\nKick();\nlocal Output = 0;\nOutput.Set(1);\n".to_string(),
        ));
        let scripts = parse_all(&sources);
        let report = CoverageReport::analyse_in(&scripts, Some(&loaded.project));

        for name in ["Kick", "Output.Set"] {
            assert!(
                report.unsupported.iter().any(|item| item.name == name),
                "a local-shadowed call must match runtime's fail-loud route: {name}: {report:?}"
            );
            assert!(
                !report.supported.iter().any(|item| item.name == name),
                "a local-shadowed call cannot remain supported: {name}: {report:?}"
            );
        }

        let mut env = crate::env::Env::new();
        env.set_local("Kick", crate::value::Value::Int(0));
        let mut state = crate::env::StateStore::new();
        let mut ctx = crate::expr::EvalCtx {
            project: &loaded.project,
            calib: &loaded.calib,
            env: &mut env,
            state: &mut state,
            group: Some("Root.Caller"),
            fn_symbol: Some("Root.Caller.Update"),
            script_name: "Caller.Update.m1scr",
            dt: 0.01,
            scripts: &scripts,
            depth: 0,
            trace: None,
        };
        assert!(matches!(
            crate::builtins::dispatch_bare(
                "Kick",
                &[],
                crate::env::CallSite::new("Caller.Update.m1scr", 0),
                &mut ctx
            ),
            Err(crate::error::EvalError::UnsupportedConstruct { .. })
        ));
    }

    #[test]
    fn this_anchored_user_function_call_is_supported() {
        // `This.Sub.Checkup();` in Caller.Kick.m1scr names the FuncUserParam
        // Root.Caller.Sub.Checkup through the `This` anchor. The walk must
        // expand `This` against the script's group before classification, or the
        // call misreports as an unsupported builtin (the AV-M1 Checkup case).
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/userfn");
        let loaded =
            crate::loader::load(&dir.join("Project.m1prj"), None).expect("userfn fixture loads");
        let report = CoverageReport::analyse_in(&loaded.scripts, Some(&loaded.project));

        let supported: Vec<&str> = report.supported.iter().map(|i| i.name.as_str()).collect();
        assert!(
            supported.contains(&"This.Sub.Checkup"),
            "This.Sub.Checkup should be Supported, got supported={supported:?}"
        );
        let unsupported: Vec<&str> = report.unsupported.iter().map(|i| i.name.as_str()).collect();
        assert!(
            !unsupported.contains(&"This.Sub.Checkup"),
            "unsupported={unsupported:?}"
        );
    }

    #[test]
    fn group_compound_update_without_project_context_is_still_stubbed() {
        // Without a project (the project-free `analyse`), a `<obj>.Update` call
        // keeps its method-name classification (Stubbed) — the conservative
        // default when the callee cannot be resolved as a user function.
        let src = "Service Bits.Update();\n";
        let report = CoverageReport::analyse(&scripts_from(src));
        let stubbed: Vec<&str> = report.stubbed.iter().map(|i| i.name.as_str()).collect();
        assert!(stubbed.contains(&"Service Bits.Update"), "{stubbed:?}");
    }

    #[test]
    fn render_is_deterministic_and_labels_every_bucket() {
        let src = "Output = Integral.Normal(Speed, 0.0, 1.0, false, 0.0);\n";
        let report = CoverageReport::analyse(&scripts_from(src));
        let text = report.render();
        assert!(text.contains("Supported:"));
        assert!(text.contains("Assumed:"));
        assert!(text.contains("Stubbed:"));
        assert!(text.contains("Unsupported:"));
        // Stubbed has nothing here.
        assert!(text.contains("(none)"));
    }

    #[test]
    fn schedule_reports_periodic_and_executed_startup_functions() {
        // The multirate fixture has four periodic functions and one startup
        // function that the whole-project runner executes once before tick zero.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/multirate");
        let loaded =
            crate::loader::load(&dir.join("Project.m1prj"), None).expect("multirate fixture loads");
        let report = CoverageReport::analyse_loaded(&loaded);

        // Look the schedule up by function symbol path.
        let by_fn: std::collections::HashMap<&str, Option<f64>> = report
            .schedule
            .iter()
            .map(|(f, hz)| (f.as_str(), *hz))
            .collect();

        assert_eq!(by_fn.get("Root.MR.Fast Writer"), Some(&Some(100.0)));
        assert_eq!(by_fn.get("Root.MR.Fast Reader"), Some(&Some(100.0)));
        assert_eq!(by_fn.get("Root.MR.Slow Writer"), Some(&Some(50.0)));
        assert_eq!(by_fn.get("Root.MR.Slow Integrator"), Some(&Some(50.0)));
        // Startup has no periodic rate, but its separate set says it executes.
        assert_eq!(by_fn.get("Root.MR.Init"), Some(&None));
        assert_eq!(report.startup, vec!["Root.MR.Init"]);
    }

    #[test]
    fn render_reports_startup_as_running_once() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/multirate");
        let loaded =
            crate::loader::load(&dir.join("Project.m1prj"), None).expect("multirate fixture loads");
        let report = CoverageReport::analyse_loaded(&loaded);
        let text = report.render();

        assert!(text.contains("Schedule:"), "no Schedule section: {text}");
        assert!(
            text.contains("Root.MR.Fast Writer") && text.contains("100"),
            "rated function not rendered: {text}"
        );
        assert!(
            text.contains("Root.MR.Init") && text.contains("startup, runs once"),
            "startup execution not rendered: {text}"
        );
    }

    #[test]
    fn schedule_is_empty_without_project_context() {
        // The project-free `analyse` cannot resolve function rates (no `Project`),
        // so the schedule is empty — but `render` still labels the section.
        let report = CoverageReport::analyse(&scripts_from("Output = Speed;\n"));
        assert!(report.schedule.is_empty());
        assert!(report.render().contains("Schedule:"));
    }
}
