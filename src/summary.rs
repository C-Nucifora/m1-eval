// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-function read/write summary, derived from a script's CST.
//!
//! The dependency-cone runner (M8 Task 26) needs to know, for each function,
//! which project channels it *writes* and which it *reads*, so it can build a
//! writer map (`channel -> function`) and order functions upstream of a target.
//!
//! `m1-typecheck`'s `schedule.rs` derives equivalent sets internally, but they are
//! not exposed across its public API, so we derive our own here directly from the
//! CST — exactly the canonical paths the evaluator itself reads and writes:
//!
//! - the left-hand side of an `AssignmentStatement` is a **write**;
//! - a compound assignment (`+=`, `*=`, …) reads its target first, so a compound
//!   target is **both** a read and a write;
//! - every other identifier/member reference on a value-producing position is a
//!   **read**.
//!
//! Only *project symbols* (channels/parameters/constants/tables) land in the
//! sets. Function-local variables, builtin library objects (`Calculate`,
//! `Filter`, …), and the `In`/`Out` signature anchors are excluded — a `local`
//! is not a cross-function dependency, and a builtin is not a project channel.
//! Names are canonicalised through [`crate::ident::classify`] so `Speed`,
//! `This.Speed`, and `Root.Demo.Speed` all collapse to one path.
//!
//! Identifiers may contain spaces; we only ever split paths on `.`.

use crate::ident::{Target, classify};
use crate::value::Value;
use m1_core::{Field, Kind, Node};
use m1_typecheck::Project;
use m1_typecheck::parsed::ParsedScript;
use std::collections::{BTreeSet, HashMap};
use std::ops::{Deref, DerefMut};

/// The canonical read/write sets of one function's body.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IoSets {
    /// Canonical paths of project symbols this function assigns to.
    pub writes: BTreeSet<String>,
    /// Canonical paths of project symbols this function reads.
    pub reads: BTreeSet<String>,
}

/// The checked form used by schedule planning.
///
/// `IoSets` is a public, externally constructible two-field struct. Keep the
/// scheduler-only call graph and static-analysis failures in this crate-private
/// wrapper so downstream struct literals keep compiling.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct CheckedIoSets {
    pub(crate) sets: IoSets,
    /// Canonical user functions or methods called by this function.
    ///
    /// The scheduler uses these edges only to fold helper-function I/O into a
    /// periodic root. Builtin and project-object methods never appear here.
    pub(crate) calls: BTreeSet<String>,
    /// Static expansion failures that would make the sets incomplete. Schedule
    /// planning rejects these instead of using an under-approximated graph.
    analysis_errors: BTreeSet<String>,
}

impl CheckedIoSets {
    pub(crate) fn analysis_errors(&self) -> impl Iterator<Item = &str> {
        self.analysis_errors.iter().map(String::as_str)
    }
}

impl Deref for CheckedIoSets {
    type Target = IoSets;

    fn deref(&self) -> &Self::Target {
        &self.sets
    }
}

impl DerefMut for CheckedIoSets {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.sets
    }
}

/// Collect the read/write sets of `script`'s body.
///
/// `group` is the enclosing group's canonical path (for group-relative name
/// resolution); the function symbol the script backs is looked up from the
/// project by the script's file name, so `In.*` references canonicalise too.
pub fn io_sets(script: &ParsedScript, project: &Project, group: Option<&str>) -> IoSets {
    checked_io_sets(script, project, group).sets
}

pub(crate) fn checked_io_sets(
    script: &ParsedScript,
    project: &Project,
    group: Option<&str>,
) -> CheckedIoSets {
    let fn_symbol = project.function_symbol_for_script(&script.name);
    let mut walker = Walker {
        project,
        group,
        fn_symbol: fn_symbol.as_deref(),
        // Local variable names in scope; a declared local shadows project lookup,
        // so we track them to exclude from the dependency sets.
        locals: HashMap::new(),
        expand: Vec::new(),
        sets: CheckedIoSets::default(),
    };
    walker.walk(&script.cst.root());
    walker.sets
}

/// Carries the resolution context while walking one function body.
struct Walker<'a> {
    project: &'a Project,
    group: Option<&'a str>,
    fn_symbol: Option<&'a str>,
    locals: HashMap<String, Value>,
    /// Active compile-time `expand` bindings, outermost first.
    expand: Vec<(String, i32, i32)>,
    sets: CheckedIoSets,
}

impl Walker<'_> {
    /// Walk a node, dispatching assignments and method calls specially and
    /// recursing elsewhere.
    fn walk(&mut self, node: &Node) {
        match node.kind() {
            Kind::ExpandStatement => self.walk_expand(node),
            Kind::LocalDeclaration => self.walk_local_decl(node),
            Kind::AssignmentStatement => self.walk_assignment(node),
            Kind::CallExpression => self.walk_call(node),
            _ => {
                for child in node.named_children() {
                    self.walk(&child);
                }
            }
        }
    }

    /// Walk an `expand` body with its literal integer range available for M1's
    /// compile-time `$(VAR)` text substitution. Unsupported bounds leave the
    /// template unresolved instead of inventing concrete paths.
    fn walk_expand(&mut self, node: &Node) {
        let binding = match expand_binding(node) {
            Ok(binding) => Some(binding),
            Err(reason) => {
                self.sets.analysis_errors.insert(format!(
                    "expand at byte {} cannot be resolved: {reason}",
                    node.byte_range().start
                ));
                None
            }
        };
        let binding = match binding {
            Some(binding)
                if self
                    .expand
                    .iter()
                    .any(|(variable, _, _)| variable == &binding.0) =>
            {
                self.sets.analysis_errors.insert(format!(
                    "expand at byte {} shadows loop variable `{}`",
                    node.byte_range().start,
                    binding.0
                ));
                None
            }
            other => other,
        };
        let saved_local = binding
            .as_ref()
            .and_then(|(variable, _, _)| self.locals.get(variable).cloned());
        if let Some(binding) = binding.clone() {
            self.locals
                .insert(binding.0.clone(), Value::m1_integer(binding.1));
            self.expand.push(binding);
        }
        if let Some(body) = node
            .children()
            .into_iter()
            .find(|child| child.kind() == Kind::Block)
        {
            self.walk(&body);
        }
        if let Some((variable, _, _)) = &binding {
            self.expand.pop();
            match saved_local {
                Some(value) => {
                    self.locals.insert(variable.clone(), value);
                }
                None => {
                    self.locals.remove(variable);
                }
            }
        }
    }

    /// A method call `<receiver>.<method>(args)` in statement position. The
    /// arguments are reads; the callee's channel receiver (if any) is accounted by
    /// [`Walker::account_call_callee`].
    fn walk_call(&mut self, node: &Node) {
        if let Some(args) = node.child_by_field(Field::Arguments) {
            self.walk_reads(&args);
        }
        self.account_user_call(node);
        self.account_call_callee(node);
    }

    /// Record a resolved user-function call. Keeping this next to the channel
    /// receiver accounting makes statement-position and expression-position
    /// calls follow the same resolution rules as runtime dispatch.
    fn account_user_call(&mut self, call_node: &Node) {
        let Some(callee) = call_node.child_by_field(Field::Function) else {
            return;
        };
        let raw = match callee.kind() {
            Kind::Identifier => callee.text().to_string(),
            Kind::MemberExpression => match crate::expr::flatten_member(&callee) {
                Ok(path) => path,
                Err(_) => return,
            },
            _ => return,
        };
        for variant in self.substituted(&raw) {
            let rewritten = crate::expr::rewrite_this(&variant, self.group);
            let path = rewritten.as_deref().unwrap_or(&variant);
            let Target::Symbol(canonical) =
                classify(path, self.group, self.fn_symbol, self.project, &self.locals)
            else {
                continue;
            };
            if self
                .project
                .symbols()
                .get(&canonical)
                .is_some_and(|symbol| {
                    matches!(
                        symbol.kind,
                        m1_typecheck::symbols::SymbolKind::Function
                            | m1_typecheck::symbols::SymbolKind::Method
                    )
                })
            {
                self.sets.calls.insert(canonical);
            }
        }
    }

    /// Account the *receiver* of a method call's callee. Mirrors `m1-typecheck`
    /// schedule.rs: `Value.Set*(…)` is an imperative **write** only when its
    /// receiver resolves to a firmware-writable channel. Other value methods
    /// are **reads**, except a supported `GetUnscheduled`, whose contract
    /// explicitly omits the scheduling edge. A library/object callee
    /// (`Calculate.Max`) has no project-value receiver and is ignored. The
    /// arguments are handled by the caller, not here.
    fn account_call_callee(&mut self, call_node: &Node) {
        let Some(callee) = call_node.child_by_field(Field::Function) else {
            return;
        };
        if callee.kind() != Kind::MemberExpression {
            return;
        }
        let (Some(receiver), Some(method)) = (
            callee.child_by_field(Field::Object),
            callee.child_by_field(Field::Property),
        ) else {
            return;
        };
        for path in self.canonical_symbols(&receiver) {
            if method.text() == "GetUnscheduled"
                && crate::builtins::object::unscheduled_value_path(&path, self.project).is_some()
            {
                // Only channels and generated table values own this method. Its
                // entire purpose is to suppress the receiver's scheduling edge.
                continue;
            }
            if method.text().starts_with("Set") {
                if let Some(value_path) =
                    crate::builtins::object::writable_value_path(&path, self.project)
                {
                    self.sets.writes.insert(value_path);
                }
                continue;
            }
            if matches!(method.text(), "Lookup" | "Get")
                && self
                    .project
                    .symbols()
                    .get(&path)
                    .is_some_and(|symbol| symbol.kind == m1_typecheck::symbols::SymbolKind::Table)
            {
                // Table lookup/indexing reads calibration axes and body cells,
                // not the generated runtime `.Value` channel.
                continue;
            }

            // Parameters are readable even though they are calibration-owned
            // and therefore excluded from `writable_value_path`.
            let value_path = match method.text() {
                "AsInteger" | "AsString" => {
                    crate::builtins::enum_conv::enum_value_path(&path, self.project).or_else(|| {
                        crate::builtins::object::numeric_value_path(&path, self.project)
                    })
                }
                _ => crate::builtins::object::numeric_value_path(&path, self.project),
            };
            if let Some(value_path) = value_path {
                self.sets.reads.insert(value_path);
            }
        }
    }

    /// A `local`/`static local` declaration introduces a local name (shadowing
    /// project symbols) and reads its initialiser, if any.
    fn walk_local_decl(&mut self, node: &Node) {
        if let Some(name) = node.child_by_field(Field::Name) {
            // Register the local so later references to it are not mistaken for a
            // project channel read.
            for name in self.substituted(name.text()) {
                self.locals.insert(name, Value::Bool(false));
            }
        }
        if let Some(init) = node.child_by_field(Field::Value) {
            self.walk_reads(&init);
        }
    }

    /// An assignment: the target is a write (and also a read for a compound
    /// assignment), and the value expression is read.
    fn walk_assignment(&mut self, node: &Node) {
        let target = node.child_by_field(Field::Target);
        let value = node.child_by_field(Field::Value);
        let op = node.child_by_field(Field::Operator);
        let compound = op
            .map(|o| m1_core::is_compound_assign(o.kind()))
            .unwrap_or(false);

        if let Some(target) = &target {
            // Resolve the target path to a canonical symbol; locals are not deps.
            for path in self.canonical_symbols(target) {
                self.sets.writes.insert(path.clone());
                if compound {
                    // A compound assignment reads the target before writing it.
                    self.sets.reads.insert(path);
                }
            }
        }
        if let Some(value) = &value {
            self.walk_reads(value);
        }
    }

    /// Walk an expression position, recording each project-symbol reference as a
    /// read. Member expressions are flattened to a path and resolved as a unit;
    /// other nodes recurse so nested calls/operands are covered.
    fn walk_reads(&mut self, node: &Node) {
        match node.kind() {
            Kind::Identifier => {
                for path in self.canonical_symbols(node) {
                    self.sets.reads.insert(path);
                }
            }
            Kind::MemberExpression => {
                // A member chain like `A.B.C` is one reference. If its head is a
                // builtin object (e.g. `Calculate.PI`) or it does not resolve to a
                // project symbol, `canonical_symbol` returns None and we skip it.
                for path in self.canonical_symbols(node) {
                    self.sets.reads.insert(path);
                }
                // Do not recurse into the member's segments — they are not
                // independent references. (A `MemberExpression` used as a call
                // *callee* is handled by the CallExpression arm below, which only
                // walks the argument list.)
            }
            Kind::CallExpression => {
                // The callee may be a library/table object (`Calculate.Max`,
                // `Map.Lookup`) — not a channel read — or a method on a channel
                // receiver (`Chan.AsInteger()` reads `Chan`, `Chan.Set(…)` writes
                // it). `account_call_callee` handles the channel-receiver case; the
                // arguments are always reads.
                if let Some(args) = node.child_by_field(Field::Arguments) {
                    self.walk_reads(&args);
                }
                self.account_user_call(node);
                self.account_call_callee(node);
            }
            _ => {
                for child in node.named_children() {
                    self.walk_reads(&child);
                }
            }
        }
    }

    /// Canonicalise an identifier/member node to a project-symbol path, or `None`
    /// when it is a local, a builtin object, or unresolved (none of which is a
    /// cross-function channel dependency).
    fn canonical_symbols(&mut self, node: &Node) -> Vec<String> {
        let raw = match node.kind() {
            Kind::Identifier => node.text().to_string(),
            Kind::MemberExpression => match crate::expr::flatten_member(node) {
                Ok(path) => path,
                Err(_) => return Vec::new(),
            },
            _ => return Vec::new(),
        };
        self.substituted(&raw)
            .into_iter()
            .filter_map(|variant| {
                // Expand a `This` anchor before resolution, exactly as runtime.
                let rewritten = crate::expr::rewrite_this(&variant, self.group);
                let path = rewritten.as_deref().unwrap_or(&variant);
                match classify(path, self.group, self.fn_symbol, self.project, &self.locals) {
                    Target::Symbol(path) => Some(path),
                    Target::Local(_) | Target::Builtin { .. } | Target::Unresolved => None,
                }
            })
            .collect()
    }

    fn substituted(&mut self, text: &str) -> Vec<String> {
        match substituted(text, &self.expand) {
            Ok(variants) => variants,
            Err(reason) => {
                self.sets.analysis_errors.insert(reason);
                Vec::new()
            }
        }
    }
}

fn expand_binding(node: &Node) -> Result<(String, i32, i32), String> {
    let variable = node
        .child_by_field(Field::Variable)
        .ok_or_else(|| "missing loop variable".to_string())?
        .text()
        .trim()
        .to_string();
    let start_text = node
        .child_by_field(Field::Start)
        .ok_or_else(|| "missing start bound".to_string())?
        .text()
        .trim()
        .to_string();
    let end_text = node
        .child_by_field(Field::End)
        .ok_or_else(|| "missing end bound".to_string())?
        .text()
        .trim()
        .to_string();
    let start: i32 = start_text
        .parse()
        .map_err(|_| format!("start bound `{start_text}` is not a literal integer"))?;
    let end: i32 = end_text
        .parse()
        .map_err(|_| format!("end bound `{end_text}` is not a literal integer"))?;
    if start.abs_diff(end) >= 256 {
        return Err(format!(
            "range {start} to {end} exceeds the 256-value analysis limit"
        ));
    }
    if start > end {
        return Err(format!(
            "descending range {start} to {end} is not expanded by schedule analysis"
        ));
    }
    Ok((variable, start, end))
}

fn substituted(text: &str, bindings: &[(String, i32, i32)]) -> Result<Vec<String>, String> {
    if !text.contains("$(") {
        return Ok(vec![text.to_string()]);
    }
    let mut variants = vec![text.to_string()];
    // Process the innermost literal binding first. Shadowing is rejected by the
    // walker before it reaches this substitution step.
    for (variable, start, end) in bindings.iter().rev() {
        let needle = format!("$({variable})");
        if !variants.iter().any(|variant| variant.contains(&needle)) {
            continue;
        }
        let mut next = Vec::new();
        for variant in &variants {
            let values: Box<dyn Iterator<Item = i32>> = if start <= end {
                Box::new(*start..=*end)
            } else {
                Box::new((*end..=*start).rev())
            };
            for value in values {
                next.push(variant.replace(&needle, &value.to_string()));
            }
        }
        variants = next;
        if variants.len() > 4096 {
            return Err(format!(
                "expand substitution for `{text}` exceeds 4096 variants"
            ));
        }
    }
    if variants.iter().any(|variant| variant.contains("$(")) {
        return Err(format!(
            "expand substitution for `{text}` has an unresolved template"
        ));
    }
    Ok(variants)
}

#[cfg(test)]
mod tests {
    use super::*;
    use m1_typecheck::parsed::parse_all;
    use std::path::Path;

    fn mini_project() -> Project {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini");
        crate::loader::load(
            &dir.join("Project.m1prj"),
            Some(&dir.join("parameters.m1cfg")),
        )
        .expect("mini fixture loads")
        .project
    }

    fn enum_project() -> Project {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/enums");
        crate::loader::load(&dir.join("Project.m1prj"), None)
            .expect("enum fixture loads")
            .project
    }

    /// Parse a synthetic script body under the `Demo.Update.m1scr` name so it
    /// canonicalises against the fixture's `Root.Demo` group.
    fn script_from(src: &str) -> ParsedScript {
        let pairs = vec![("Demo.Update.m1scr".to_string(), src.to_string())];
        parse_all(&pairs).into_iter().next().unwrap()
    }

    #[test]
    fn assignment_target_is_a_write_and_rhs_idents_are_reads() {
        let project = mini_project();
        let script = script_from("Output = Speed * Gain;\n");
        let sets = io_sets(&script, &project, Some("Root.Demo"));

        assert!(sets.writes.contains("Root.Demo.Output"), "{sets:?}");
        assert!(sets.reads.contains("Root.Demo.Speed"), "{sets:?}");
        assert!(sets.reads.contains("Root.Demo.Gain"), "{sets:?}");
        // The write target is not also a read here (plain assignment).
        assert!(!sets.reads.contains("Root.Demo.Output"), "{sets:?}");
    }

    #[test]
    fn compound_assignment_target_is_both_read_and_write() {
        let project = mini_project();
        let script = script_from("Output += Speed;\n");
        let sets = io_sets(&script, &project, Some("Root.Demo"));

        assert!(sets.writes.contains("Root.Demo.Output"));
        assert!(sets.reads.contains("Root.Demo.Output"));
        assert!(sets.reads.contains("Root.Demo.Speed"));
    }

    #[test]
    fn locals_are_not_dependencies() {
        let project = mini_project();
        // `scaled` is a local; only Speed/Gain (reads) and Output (write) are deps.
        let script = script_from("local scaled = Speed * Gain;\nOutput = scaled;\n");
        let sets = io_sets(&script, &project, Some("Root.Demo"));

        assert!(sets.writes.contains("Root.Demo.Output"));
        assert!(sets.reads.contains("Root.Demo.Speed"));
        assert!(sets.reads.contains("Root.Demo.Gain"));
        // `scaled` must not appear as a channel.
        assert!(!sets.reads.iter().any(|r| r.contains("scaled")));
        assert!(!sets.writes.iter().any(|w| w.contains("scaled")));
    }

    #[test]
    fn builtin_callee_is_not_a_read_but_args_are() {
        let project = mini_project();
        let script = script_from("Output = Calculate.Max(Speed, Gain);\n");
        let sets = io_sets(&script, &project, Some("Root.Demo"));

        // The Calculate object/method is not a channel.
        assert!(!sets.reads.iter().any(|r| r.starts_with("Calculate")));
        // But the call arguments are reads.
        assert!(sets.reads.contains("Root.Demo.Speed"));
        assert!(sets.reads.contains("Root.Demo.Gain"));
        assert!(sets.writes.contains("Root.Demo.Output"));
    }

    #[test]
    fn channel_set_call_is_a_write_not_a_read() {
        let project = mini_project();
        // `Chan.Set(value)` is the imperative setter — a *write* of the channel,
        // matching `m1-typecheck` schedule.rs and the evaluator's `.Set` route. The
        // argument is still a read.
        let script = script_from("Output.Set(Speed);\n");
        let sets = io_sets(&script, &project, Some("Root.Demo"));

        assert!(sets.writes.contains("Root.Demo.Output"), "{sets:?}");
        // The receiver is a write, NOT mis-counted as a read.
        assert!(!sets.reads.contains("Root.Demo.Output"), "{sets:?}");
        // The argument is a read.
        assert!(sets.reads.contains("Root.Demo.Speed"), "{sets:?}");
    }

    #[test]
    fn non_set_method_call_on_channel_is_a_read() {
        let project = mini_project();
        // A non-`Set` method (`Output.AsInteger()`) reads its receiver — only the
        // imperative setter family writes. The receiver therefore appears as a read.
        let script = script_from("local x = Output.AsInteger();\n");
        let sets = io_sets(&script, &project, Some("Root.Demo"));

        assert!(sets.reads.contains("Root.Demo.Output"), "{sets:?}");
        assert!(!sets.writes.contains("Root.Demo.Output"), "{sets:?}");
    }

    #[test]
    fn get_unscheduled_omits_only_its_receiver_dependency() {
        let project = mini_project();
        let script =
            script_from("local x = Output.GetUnscheduled();\nlocal y = Speed.Validate(Gain);\n");
        let sets = io_sets(&script, &project, Some("Root.Demo"));

        assert!(
            !sets.reads.contains("Root.Demo.Output"),
            "GetUnscheduled must not create a scheduler edge: {sets:?}"
        );
        assert!(
            sets.reads.contains("Root.Demo.Speed"),
            "other object methods still read their receiver: {sets:?}"
        );
        assert!(
            sets.reads.contains("Root.Demo.Gain"),
            "call arguments remain ordinary reads: {sets:?}"
        );
    }

    #[test]
    fn parameter_value_method_is_a_read_but_set_is_not_a_write() {
        let project = mini_project();
        let script = script_from("local valid = Gain.Validate(Speed);\nGain.Set(Output);\n");
        let sets = io_sets(&script, &project, Some("Root.Demo"));

        assert!(
            sets.reads.contains("Root.Demo.Gain"),
            "parameter validation still reads its receiver: {sets:?}"
        );
        assert!(
            !sets.writes.contains("Root.Demo.Gain"),
            "parameters remain calibration-owned: {sets:?}"
        );
        assert!(sets.reads.contains("Root.Demo.Speed"), "{sets:?}");
        assert!(sets.reads.contains("Root.Demo.Output"), "{sets:?}");
    }

    #[test]
    fn parameter_backed_compound_set_is_not_a_scheduler_write() {
        let project = mini_project();
        let script = script_from("Calibration Compound.Set(Output);\n");
        let sets = io_sets(&script, &project, Some("Root.Demo"));

        assert!(
            !sets
                .writes
                .contains("Root.Demo.Calibration Compound.Calibration"),
            "the declared Parameter default is calibration-owned: {sets:?}"
        );
        assert!(
            !sets.writes.contains("Root.Demo.Calibration Compound.Value"),
            "the generated sibling must not bypass the declared default: {sets:?}"
        );
        assert!(sets.reads.contains("Root.Demo.Output"), "{sets:?}");
    }

    #[test]
    fn enum_conversions_read_channel_and_compound_receivers() {
        let project = enum_project();
        let script = script_from(
            "local direct = Mode.AsString();\nlocal compound = Compound.AsInteger();\n",
        );
        let sets = io_sets(&script, &project, Some("Root.Demo"));

        assert!(sets.reads.contains("Root.Demo.Mode"), "{sets:?}");
        assert!(sets.reads.contains("Root.Demo.Compound.Value"), "{sets:?}");
    }

    #[test]
    fn table_lookup_and_get_do_not_read_the_generated_value_channel() {
        let project = mini_project();
        let script = script_from("local x = Map.Lookup(Speed);\nlocal y = Map.Get(0);\n");
        let sets = io_sets(&script, &project, Some("Root.Demo"));

        assert!(sets.reads.contains("Root.Demo.Speed"), "{sets:?}");
        assert!(
            !sets.reads.contains("Root.Demo.Map.Value"),
            "calibration-only table calls must not create a runtime channel edge: {sets:?}"
        );
    }

    #[test]
    fn value_compound_set_writes_its_declared_default_channel() {
        let project = mini_project();
        let script = script_from("Sensor Compound.Set(2.0);\n");
        let sets = io_sets(&script, &project, Some("Root.Demo"));

        assert!(
            sets.writes.contains("Root.Demo.Sensor Compound.Sensor"),
            "the scheduler must see the same concrete write as runtime: {sets:?}"
        );
        assert!(!sets.writes.contains("Root.Demo.Sensor Compound"));
    }
}
