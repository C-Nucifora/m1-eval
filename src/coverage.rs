// SPDX-License-Identifier: GPL-3.0-or-later
//! The `--coverage` analysis: what each project script *uses* versus what the
//! engine *supports*.
//!
//! Before a run, a user wants to know which parts of their project the evaluator
//! will compute faithfully, which it will only stub (Tier-3 IO, externally
//! driven), and which it cannot handle at all (and would fail loud on). This
//! module walks every script's CST and answers that, statically:
//!
//! - every `Object.Method(...)` builtin call is classified against the dispatch
//!   table via [`crate::builtins::classify_builtin`] — supported, stubbed, or
//!   unsupported;
//! - every statement/expression construct `Kind` is classified against the set
//!   the evaluator implements.
//!
//! The result is a [`CoverageReport`] of de-duplicated, sorted entries — pure
//! data, no `m1-core`/`m1-typecheck` types — that the CLI prints and the `Engine`
//! facade returns.

use crate::builtins::{BuiltinSupport, classify_builtin};
use crate::ident::{Target, classify};
use m1_core::{Field, Kind, Node};
use m1_typecheck::Project;
use m1_typecheck::parsed::ParsedScript;
use m1_typecheck::symbols::SymbolKind;
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
    /// Items the engine evaluates faithfully.
    pub supported: Vec<CoverageItem>,
    /// Items handled as documented/scenario-fed stubs (Tier-3 IO).
    pub stubbed: Vec<CoverageItem>,
    /// Items the engine does not handle (would fail loud at runtime).
    pub unsupported: Vec<CoverageItem>,
    /// The whole-project execution schedule: one `(function symbol, rate)` entry
    /// per script-backed function. `Some(hz)` is the function's periodic rate (it
    /// runs that many times per second in whole-project mode); `None` flags a
    /// function with no resolvable periodic trigger (`On Startup`, an untriggered
    /// or `$(…)`-parameterised trigger) — it is **not** run by the whole-project
    /// scheduler. Sorted `(rate descending, function symbol)` for a deterministic
    /// report; empty when the analysis had no [`Project`] to resolve rates from.
    pub schedule: Vec<(String, Option<f64>)>,
}

impl CoverageReport {
    /// Analyse every script in `scripts`, producing a combined report. No project
    /// context: a member-expression callee is classified on its `(object, method)`
    /// spelling alone, so a user-function call whose method name collides with an
    /// IO-stub method (e.g. `Update`) cannot be distinguished from the stub. Use
    /// [`CoverageReport::analyse_in`] for project-accurate user-function coverage.
    pub fn analyse(scripts: &[ParsedScript]) -> CoverageReport {
        Self::analyse_in(scripts, None)
    }

    /// Analyse every script with optional project context. When a [`Project`] is
    /// given, a `CallExpression` whose member-expression callee resolves to a user
    /// `Function`/`Method` symbol is reported **Supported** (it is evaluated inline
    /// — P15-D), disambiguating it from a same-named IO-stub method (`Service
    /// Bits.Update` vs `Slip Control.Update`). This mirrors `eval_call`, which
    /// tries `userfn::call` before library/IO dispatch.
    pub fn analyse_in(scripts: &[ParsedScript], project: Option<&Project>) -> CoverageReport {
        let mut supported = BTreeSet::new();
        let mut stubbed = BTreeSet::new();
        let mut unsupported = BTreeSet::new();
        for script in scripts {
            // The script's enclosing group, for resolving group-relative callees.
            let group = project.and_then(|p| p.group_for_script(&script.name));
            let cx = WalkCtx {
                project,
                group: group.as_deref(),
            };
            walk(
                &script.cst.root(),
                &cx,
                &mut supported,
                &mut stubbed,
                &mut unsupported,
            );
        }
        // A construct/builtin classified supported by one script must not also be
        // reported unsupported because a *different* occurrence (e.g. a bad-shape
        // call) hit the fallback; the sets above are already by (name, kind), so
        // dedup is automatic. We only ensure the buckets are disjoint by
        // precedence: supported > stubbed > unsupported.
        stubbed.retain(|i| !supported.contains(i));
        unsupported.retain(|i| !supported.contains(i) && !stubbed.contains(i));
        CoverageReport {
            supported: supported.into_iter().collect(),
            stubbed: stubbed.into_iter().collect(),
            unsupported: unsupported.into_iter().collect(),
            schedule: build_schedule(scripts, project),
        }
    }

    /// A human-readable, deterministic summary for the CLI. One section per
    /// bucket, each line `kind: name`. Empty buckets are still labelled so the
    /// output shape is stable.
    pub fn render(&self) -> String {
        let mut out = String::new();
        render_section(&mut out, "Supported", &self.supported);
        render_section(&mut out, "Stubbed", &self.stubbed);
        render_section(&mut out, "Unsupported", &self.unsupported);
        render_schedule(&mut out, &self.schedule);
        out
    }
}

/// Append the `Schedule:` section: one line per function with its rate, or a flag
/// that it is unscheduled (and so never runs in whole-project mode). An empty
/// schedule still prints the label so the output shape is stable.
fn render_schedule(out: &mut String, schedule: &[(String, Option<f64>)]) {
    out.push_str("Schedule:\n");
    if schedule.is_empty() {
        out.push_str("  (none)\n");
        return;
    }
    for (function, rate) in schedule {
        match rate {
            Some(hz) => out.push_str(&format!("  {function} @ {hz} Hz\n")),
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

/// The per-script context the coverage walk carries: the optional project model
/// and the script's enclosing group, used to resolve a call's callee to a user
/// function (so an inline user-function call is reported Supported, not stubbed).
struct WalkCtx<'a> {
    project: Option<&'a Project>,
    group: Option<&'a str>,
}

/// Recursively walk a node, bucketing builtin calls and reportable constructs.
fn walk(
    node: &Node,
    cx: &WalkCtx,
    supported: &mut BTreeSet<CoverageItem>,
    stubbed: &mut BTreeSet<CoverageItem>,
    unsupported: &mut BTreeSet<CoverageItem>,
) {
    // Calls: an inline user-function/method call (member-expr or bare-identifier
    // callee resolving to a project `Function`/`Method`) is Supported (P15-D);
    // otherwise classify the `Object.Method` against the builtin dispatch table.
    if node.kind() == Kind::CallExpression {
        if let Some(name) = user_function_callee(node, cx) {
            // A user function the engine evaluates inline.
            supported.insert(CoverageItem {
                name,
                kind: ItemKind::Builtin,
            });
        } else if let Some((object, method)) = call_object_method(node) {
            let item = CoverageItem {
                name: format!("{object}.{method}"),
                kind: ItemKind::Builtin,
            };
            match classify_builtin(&object, &method) {
                BuiltinSupport::Supported => supported.insert(item),
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

    for child in node.named_children() {
        walk(&child, cx, supported, stubbed, unsupported);
    }
}

/// The flattened callee path of a `CallExpression` when it resolves (against the
/// walk's project + group) to a user `Function`/`Method` symbol — i.e. a call the
/// engine evaluates inline (P15-D). `None` when there is no project context, the
/// callee does not resolve to a user function, or the callee shape is unexpected.
///
/// Both call forms are recognised, mirroring `eval_call`: a member-expression
/// callee (`Slip Control.Update`) and a bare-identifier callee (`Update`).
fn user_function_callee(node: &Node, cx: &WalkCtx) -> Option<String> {
    let project = cx.project?;
    let callee = node.child_by_field(Field::Function)?;
    let path = match callee.kind() {
        Kind::MemberExpression => crate::expr::flatten_member(&callee).ok()?,
        Kind::Identifier => callee.text().to_string(),
        _ => return None,
    };
    let no_locals = HashMap::new();
    let Target::Symbol(canon) = classify(&path, cx.group, None, project, &no_locals) else {
        return None;
    };
    let is_user_fn = project
        .symbols()
        .get(&canon)
        .map(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
        .unwrap_or(false);
    is_user_fn.then_some(path)
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
    fn integral_and_lookup_are_supported_cancomms_is_stubbed() {
        let src = r#"
local i = Integral.Normal(Speed, 0.0, 100.0, false, 0.0);
local t = Demo.Map.Lookup(Speed, Load);
local c = CanComms.GetFloat(1, 2);
Output = i;
"#;
        let scripts = scripts_from(src);
        let report = CoverageReport::analyse(&scripts);

        let names: Vec<&str> = report.supported.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"Integral.Normal"), "{names:?}");
        assert!(names.contains(&"Demo.Map.Lookup"), "{names:?}");

        let stub_names: Vec<&str> = report.stubbed.iter().map(|i| i.name.as_str()).collect();
        assert!(stub_names.contains(&"CanComms.GetFloat"), "{stub_names:?}");
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
        assert!(text.contains("Stubbed:"));
        assert!(text.contains("Unsupported:"));
        // Stubbed has nothing here.
        assert!(text.contains("(none)"));
    }

    #[test]
    fn schedule_reports_rated_functions_and_flags_unscheduled() {
        // Task 17: with a project, the report carries a per-function schedule —
        // each function's `call_rate_hz` (or `None` for an On-Startup function
        // that never runs in whole-project mode). The multirate fixture has four
        // periodic functions (two at 100 Hz, two at 50 Hz) and one startup.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/multirate");
        let loaded =
            crate::loader::load(&dir.join("Project.m1prj"), None).expect("multirate fixture loads");
        let report = CoverageReport::analyse_in(&loaded.scripts, Some(&loaded.project));

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
        // The On-Startup function is reported with rate `None` (unscheduled).
        assert_eq!(by_fn.get("Root.MR.Init"), Some(&None));
    }

    #[test]
    fn render_includes_schedule_section_flagging_unscheduled() {
        // The `Schedule:` section lists each function with its rate; an
        // unscheduled (`None`) function is flagged so the user sees it will not
        // run in whole-project mode.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/multirate");
        let loaded =
            crate::loader::load(&dir.join("Project.m1prj"), None).expect("multirate fixture loads");
        let report = CoverageReport::analyse_in(&loaded.scripts, Some(&loaded.project));
        let text = report.render();

        assert!(text.contains("Schedule:"), "no Schedule section: {text}");
        assert!(
            text.contains("Root.MR.Fast Writer") && text.contains("100"),
            "rated function not rendered: {text}"
        );
        assert!(
            text.contains("Root.MR.Init") && text.contains("unscheduled"),
            "startup function not flagged unscheduled: {text}"
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
