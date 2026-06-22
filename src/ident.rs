// SPDX-License-Identifier: GPL-3.0-or-later
//! Identifier text → canonical symbol path.
//!
//! M1 scripts reference channels/parameters by *unqualified* or *group-relative*
//! names (`Speed`, `Parent.Sibling`, `This.Output`), and the same text can also
//! name a function-local variable or a builtin library object (`Calculate`).
//! Before the evaluator can read or write a value it must know *which* of these a
//! name denotes and, for project symbols, its single canonical path.
//!
//! This module wraps `m1_typecheck::resolve::resolve`, which already implements
//! the M1 scope order (local → library → absolute → group-relative → `Parent`
//! walk) and the `Root.`-prefix canonicalisation. We translate its `Resolution`
//! into a small [`Target`] the rest of the crate can match on without depending
//! on `m1-typecheck` types.
//!
//! Identifiers may contain spaces; `resolve` and this wrapper only ever split on
//! `.` for path segments, never on whitespace.

use crate::value::Value;
use m1_typecheck::Project;
use m1_typecheck::ValueType;
use m1_typecheck::resolve::{Resolution, Scope, resolve};
use std::collections::HashMap;

/// What an identifier (or dotted path) denotes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A project channel/parameter/constant/table/function, by its single
    /// canonical symbol-table path (e.g. `"Root.Demo.Speed"`).
    Symbol(String),
    /// A function-local variable, by its name.
    Local(String),
    /// A builtin library object (e.g. `Calculate`, `CanComms`) — the call path
    /// (Task 11) decides what to do with the method. `object` is the library
    /// object name.
    Builtin { object: String },
    /// Nothing in the project, locals, or the builtin library matches. The
    /// evaluator fails loud on a read/write of an `Unresolved` target rather than
    /// guessing.
    Unresolved,
}

/// The leading dot-segment of a path (`"Calculate"` of `"Calculate.Max"`). Never
/// splits on whitespace — only on `.`.
fn root_segment(path: &str) -> &str {
    match path.find('.') {
        Some(i) => &path[..i],
        None => path,
    }
}

/// Classify an identifier/path against the project, the enclosing group, the
/// backing function symbol, and the current function locals.
///
/// `group` is the canonical path of the enclosing group (so group-relative names
/// resolve); `fn_symbol` is the canonical path of the `Function`/`Method` symbol
/// the script backs (so `In.<Param>` resolves against its signature). `locals`
/// carries the names currently in scope — their runtime [`Value`]s are irrelevant
/// to *resolution*, so they are passed to `resolve` as `ValueType::Unknown`; the
/// only thing that matters is whether the name is a known local.
pub fn classify(
    name: &str,
    group: Option<&str>,
    fn_symbol: Option<&str>,
    project: &Project,
    locals: &HashMap<String, Value>,
) -> Target {
    let scope = Scope {
        locals: locals
            .keys()
            .map(|k| (k.clone(), ValueType::Unknown))
            .collect(),
        group: group.map(str::to_string),
        project: Some(project),
        fn_symbol: fn_symbol.map(str::to_string),
    };

    match resolve(name, &scope) {
        Resolution::Symbol(sym) => Target::Symbol(sym.path.clone()),
        Resolution::Local(_) => Target::Local(name.to_string()),
        Resolution::BuiltinObject(obj) => Target::Builtin {
            object: obj.to_string(),
        },
        // A builtin function/method call (`Calculate.Max`). Its object is the
        // leading segment; the call path validates and dispatches the method.
        Resolution::BuiltinFn(_) => Target::Builtin {
            object: root_segment(name).to_string(),
        },
        // `Opaque` covers `In`/`Out`/`Parent`/`This`/`Library`/`Root` anchors and
        // accessor calls on existing symbols — not a project miss, but also not a
        // value this resolver can hand back as a canonical path on its own. The
        // member-expression / call paths (M4) handle these anchors before calling
        // `classify`, so reaching here means "no canonical symbol": treat as
        // unresolved so a stray read fails loud rather than silently succeeding.
        Resolution::Opaque | Resolution::Unresolved => Target::Unresolved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn mini_project() -> Project {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini");
        let loaded = crate::loader::load(&dir.join("Project.m1prj"), None)
            .expect("mini fixture loads");
        loaded.project
    }

    #[test]
    fn group_relative_name_canonicalizes() {
        let project = mini_project();
        let locals = HashMap::new();
        // `Speed` referenced from inside group `Root.Demo` is `Root.Demo.Speed`.
        let t = classify("Speed", Some("Root.Demo"), None, &project, &locals);
        assert_eq!(t, Target::Symbol("Root.Demo.Speed".to_string()));
    }

    #[test]
    fn parent_reference_walks_up_the_group_tree() {
        let project = mini_project();
        let locals = HashMap::new();
        // From group `Root.Demo`, `Parent.Sibling` is `Root.Sibling`.
        let t = classify("Parent.Sibling", Some("Root.Demo"), None, &project, &locals);
        assert_eq!(t, Target::Symbol("Root.Sibling".to_string()));
    }

    #[test]
    fn absolute_path_resolves() {
        let project = mini_project();
        let locals = HashMap::new();
        let t = classify("Root.Demo.Output", None, None, &project, &locals);
        assert_eq!(t, Target::Symbol("Root.Demo.Output".to_string()));
    }

    #[test]
    fn local_variable_is_local() {
        let project = mini_project();
        let mut locals = HashMap::new();
        locals.insert("scaled".to_string(), Value::Float(0.0));
        // A bare local name shadows project lookup per the M1 scope order.
        let t = classify("scaled", Some("Root.Demo"), None, &project, &locals);
        assert_eq!(t, Target::Local("scaled".to_string()));
    }

    #[test]
    fn builtin_object_is_builtin() {
        let project = mini_project();
        let locals = HashMap::new();
        let t = classify("Calculate", Some("Root.Demo"), None, &project, &locals);
        assert_eq!(
            t,
            Target::Builtin {
                object: "Calculate".to_string()
            }
        );
    }

    #[test]
    fn builtin_function_carries_its_object() {
        let project = mini_project();
        let locals = HashMap::new();
        let t = classify("Calculate.Max", Some("Root.Demo"), None, &project, &locals);
        assert_eq!(
            t,
            Target::Builtin {
                object: "Calculate".to_string()
            }
        );
    }

    #[test]
    fn unknown_name_is_unresolved() {
        let project = mini_project();
        let locals = HashMap::new();
        // A bare, group-scoped name that matches no symbol is a genuine miss.
        let t = classify("NoSuchChannel", Some("Root.Demo"), None, &project, &locals);
        assert_eq!(t, Target::Unresolved);
    }
}
