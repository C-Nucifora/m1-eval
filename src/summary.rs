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
use m1_typecheck::symbols::SymbolKind;
use std::collections::{BTreeSet, HashMap};

/// The canonical read/write sets of one function's body.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IoSets {
    /// Canonical paths of project symbols this function assigns to.
    pub writes: BTreeSet<String>,
    /// Canonical paths of project symbols this function reads.
    pub reads: BTreeSet<String>,
}

/// Collect the read/write sets of `script`'s body.
///
/// `group` is the enclosing group's canonical path (for group-relative name
/// resolution); the function symbol the script backs is looked up from the
/// project by the script's file name, so `In.*` references canonicalise too.
pub fn io_sets(script: &ParsedScript, project: &Project, group: Option<&str>) -> IoSets {
    let fn_symbol = project.function_symbol_for_script(&script.name);
    let mut walker = Walker {
        project,
        group,
        fn_symbol: fn_symbol.as_deref(),
        // Local variable names in scope; a declared local shadows project lookup,
        // so we track them to exclude from the dependency sets.
        locals: HashMap::new(),
        sets: IoSets::default(),
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
    sets: IoSets,
}

impl Walker<'_> {
    /// Walk a node, dispatching assignments and method calls specially and
    /// recursing elsewhere.
    fn walk(&mut self, node: &Node) {
        match node.kind() {
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

    /// A method call `<receiver>.<method>(args)` in statement position. The
    /// arguments are reads; the callee's channel receiver (if any) is accounted by
    /// [`Walker::account_call_callee`].
    fn walk_call(&mut self, node: &Node) {
        if let Some(args) = node.child_by_field(Field::Arguments) {
            self.walk_reads(&args);
        }
        self.account_call_callee(node);
    }

    /// Account the *receiver* of a method call's callee. Mirrors `m1-typecheck`
    /// schedule.rs: when the receiver resolves to a project channel/parameter,
    /// `Chan.Set*(…)` is the imperative setter — a **write** of that channel — and
    /// any other method (`AsInteger`/`Lookup`/`Get…`/…) is a **read**. A
    /// library/object callee (`Calculate.Max`) has no channel receiver and is
    /// ignored. The arguments are handled by the caller, not here.
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
        // The receiver must resolve to a project channel/parameter to count.
        let Some(path) = self.canonical_symbol(&receiver) else {
            return;
        };
        let writable = self
            .project
            .symbols()
            .get(&path)
            .map(|s| matches!(s.kind, SymbolKind::Channel | SymbolKind::Parameter))
            .unwrap_or(false);
        if !writable {
            return;
        }
        if method.text().starts_with("Set") {
            self.sets.writes.insert(path);
        } else {
            self.sets.reads.insert(path);
        }
    }

    /// A `local`/`static local` declaration introduces a local name (shadowing
    /// project symbols) and reads its initialiser, if any.
    fn walk_local_decl(&mut self, node: &Node) {
        if let Some(name) = node.child_by_field(Field::Name) {
            // Register the local so later references to it are not mistaken for a
            // project channel read.
            self.locals
                .insert(name.text().to_string(), Value::Bool(false));
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
            if let Some(path) = self.canonical_symbol(target) {
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
                if let Some(path) = self.canonical_symbol(node) {
                    self.sets.reads.insert(path);
                }
            }
            Kind::MemberExpression => {
                // A member chain like `A.B.C` is one reference. If its head is a
                // builtin object (e.g. `Calculate.PI`) or it does not resolve to a
                // project symbol, `canonical_symbol` returns None and we skip it.
                if let Some(path) = self.canonical_symbol(node) {
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
    fn canonical_symbol(&self, node: &Node) -> Option<String> {
        let raw = match node.kind() {
            Kind::Identifier => node.text().to_string(),
            Kind::MemberExpression => crate::expr::flatten_member(node).ok()?,
            _ => return None,
        };
        // Expand a `This` anchor to the enclosing group before resolution, exactly
        // as the evaluator does.
        let rewritten = crate::expr::rewrite_this(&raw, self.group);
        let path = rewritten.as_deref().unwrap_or(&raw);
        match classify(path, self.group, self.fn_symbol, self.project, &self.locals) {
            Target::Symbol(p) => Some(p),
            // Locals, builtins, and unresolved anchors are not project deps.
            Target::Local(_) | Target::Builtin { .. } | Target::Unresolved => None,
        }
    }
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
}
