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
use m1_core::{Field, Kind, Node};
use m1_typecheck::parsed::ParsedScript;
use std::collections::BTreeSet;

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
}

impl CoverageReport {
    /// Analyse every script in `scripts`, producing a combined report.
    pub fn analyse(scripts: &[ParsedScript]) -> CoverageReport {
        let mut supported = BTreeSet::new();
        let mut stubbed = BTreeSet::new();
        let mut unsupported = BTreeSet::new();
        for script in scripts {
            walk(
                &script.cst.root(),
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
        out
    }
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

/// Recursively walk a node, bucketing builtin calls and reportable constructs.
fn walk(
    node: &Node,
    supported: &mut BTreeSet<CoverageItem>,
    stubbed: &mut BTreeSet<CoverageItem>,
    unsupported: &mut BTreeSet<CoverageItem>,
) {
    // Builtin calls: classify the `Object.Method`.
    if node.kind() == Kind::CallExpression
        && let Some((object, method)) = call_object_method(node)
    {
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
        walk(&child, supported, stubbed, unsupported);
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
        let names: Vec<&str> = report
            .unsupported
            .iter()
            .map(|i| i.name.as_str())
            .collect();
        assert!(names.contains(&"Calculate.NoSuchMethod"), "{names:?}");
    }

    #[test]
    fn statement_constructs_are_reported_supported() {
        let src = "local x = 1;\nif (Speed > 0.0)\n{\n\tOutput = 1.0;\n}\nelse\n{\n\tOutput = 0.0;\n}\n";
        let scripts = scripts_from(src);
        let report = CoverageReport::analyse(&scripts);
        let constructs: Vec<&str> = report
            .supported
            .iter()
            .filter(|i| i.kind == ItemKind::Construct)
            .map(|i| i.name.as_str())
            .collect();
        // The if-statement and assignment constructs are recognised + supported.
        assert!(constructs.iter().any(|c| c.contains("if")), "{constructs:?}");
        assert!(
            constructs.iter().any(|c| c.contains("assignment") || c.contains("Assignment")),
            "{constructs:?}"
        );
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
}
