// SPDX-License-Identifier: GPL-3.0-or-later
//! Enum `.AsInteger` conversion (P15-B).
//!
//! `<source>.AsInteger()` converts an enum to its declared integer. The integer
//! is the value stored in the enum type's `members` list — the `ContainerOrder`
//! for project-local enums, the documented `EnumMember.value` for builtin /
//! firmware enums — never the ordinal index of the member.
//!
//! Two source forms appear in real scripts, both resolved through the same
//! `EnumType.members` lookup ([`crate::value::Value::as_enum_int`]):
//!
//! 1. **Enum-type-qualified member literal** (a compile-time constant), e.g.
//!    `Drive State.Idle.AsInteger()`, `AMK Inverter Boot State.System Ready
//!    .AsInteger()`. Here the object path is `<EnumTypeName>.<Member>`: the enum
//!    type resolves by name and the member's declared integer is returned.
//! 2. **Value-holding source** (a runtime read), e.g. `Control.Drive State
//!    .AsInteger()` (a value-compound whose enum value lives on its `.Value`
//!    child), `Boot State.AsInteger()` (an enum-typed channel). Here the object
//!    resolves to a `Channel` or a `Group` value-compound; its current
//!    [`Value::Enum`] is read from the env and converted.
//!
//! Enum type names *and* member names contain spaces (`AMK Inverter Boot State`,
//! `System Ready`), so the object path is only ever split on the **rightmost**
//! `.` — never on whitespace.

use crate::error::EvalError;
use crate::expr::EvalCtx;
use crate::ident::{Target, classify};
use crate::value::Value;
use m1_typecheck::Project;
use m1_typecheck::symbols::SymbolKind;
use std::collections::{HashMap, HashSet};

/// Convert `<object>.AsInteger()` to its declared enum integer.
///
/// Returns:
/// - `Ok(Some(Value::M1(Integer(n))))` when `object` is a recognised enum source (either
///   form) and the conversion succeeds;
/// - `Ok(None)` when `object` is neither an enum-type-qualified member literal
///   nor a value-holding enum source — so the caller can fall through to other
///   dispatch (e.g. a Timer object method);
/// - a fail-loud [`EvalError`] when `object` *is* an enum source but the
///   conversion cannot proceed (an unknown member, an unset enum channel, or a
///   non-enum runtime value) — never a guessed integer.
pub fn as_integer(object: &str, ctx: &mut EvalCtx) -> Result<Option<Value>, EvalError> {
    let Some(value) = enum_value(object, ctx)? else {
        return Ok(None);
    };
    let integer =
        i32::try_from(value.as_enum_int(ctx.project)?).map_err(|_| EvalError::TypeError {
            detail: format!("enum value from {object:?} is outside the M1 Integer range"),
        })?;
    Ok(Some(Value::m1_integer(integer)))
}

/// Convert `<object>.AsString()` to the enum member's exact display name.
pub fn as_string(object: &str, ctx: &mut EvalCtx) -> Result<Option<Value>, EvalError> {
    let Some(value) = enum_value(object, ctx)? else {
        return Ok(None);
    };
    // Match AsInteger's fail-loud membership contract. `Value::Enum` is public,
    // so a caller can seed a structurally valid value whose member does not
    // belong to its declared enum; never echo that corrupt member as a string.
    value.as_enum_int(ctx.project)?;
    let Value::Enum { member, .. } = value else {
        unreachable!("enum_value only returns Value::Enum");
    };
    Ok(Some(Value::Str(member)))
}

/// Resolve either supported enum source form to its current enum value.
fn enum_value(object: &str, ctx: &mut EvalCtx) -> Result<Option<Value>, EvalError> {
    // Form 1: an enum-type-qualified member literal `<EnumTypeName>.<Member>`.
    // Split on the rightmost `.` only — both the type name and the member may
    // contain spaces.
    if let Some((prefix, leaf)) = object.rsplit_once('.')
        && let Some(id) = ctx.project.symbols().enum_by_name(prefix)
    {
        if ctx.project.symbols().enum_has_member(id, leaf) {
            return Ok(Some(Value::Enum {
                id,
                member: leaf.to_string(),
            }));
        }
        // The prefix *is* an enum type but the leaf is not one of its members:
        // a fail-loud error rather than a silent miss — the author wrote an
        // enum-literal `.AsInteger` against a non-member.
        return Err(EvalError::TypeError {
            detail: format!("{leaf:?} is not a member of enum {prefix:?}"),
        });
    }

    // Form 2: a value-holding source. Classify the object against the project.
    let target = classify(
        object,
        ctx.group,
        ctx.fn_symbol,
        ctx.project,
        &ctx.env.locals,
    );
    let Target::Symbol(canon) = target else {
        // Not an enum literal and not a resolvable project symbol: let the caller
        // fall through to other dispatch.
        return Ok(None);
    };
    let Some(value_path) = enum_value_path(&canon, ctx.project) else {
        return Ok(None);
    };
    read_enum_at(&value_path, ctx).map(Some)
}

/// Resolve an enum-valued project object to the symbol `read_symbol` consumes.
/// Constants and direct typed objects keep their own path. A value compound
/// follows its declared default, then its generated `.Value` child.
pub(crate) fn enum_value_path(canon: &str, project: &Project) -> Option<String> {
    enum_value_path_inner(canon, project, &mut HashSet::new())
}

fn enum_value_path_inner(
    canon: &str,
    project: &Project,
    seen: &mut HashSet<String>,
) -> Option<String> {
    if !seen.insert(canon.to_string()) {
        return None;
    }
    let symbol = project.symbols().get(canon)?;
    if symbol.value_type.is_enum()
        && matches!(
            symbol.kind,
            SymbolKind::Channel
                | SymbolKind::Parameter
                | SymbolKind::Constant
                | SymbolKind::Object
                | SymbolKind::Reference
                | SymbolKind::Other
        )
    {
        return Some(canon.to_string());
    }

    if symbol.kind == SymbolKind::Group
        && let Some(default) = symbol.default_value.as_deref()
    {
        let locals = HashMap::new();
        if let Target::Symbol(path) = classify(default, Some(canon), None, project, &locals)
            && let Some(value_path) = enum_value_path_inner(&path, project, seen)
        {
            return Some(value_path);
        }
    }

    if matches!(
        symbol.kind,
        SymbolKind::Group | SymbolKind::Table | SymbolKind::Object
    ) {
        let value_path = format!("{canon}.Value");
        return project
            .symbols()
            .get(&value_path)
            .and_then(|_| enum_value_path_inner(&value_path, project, seen));
    }
    None
}

/// Read the current value at a canonical channel path and convert it to its enum
/// integer. Reads through [`crate::expr::read_symbol`] so the value resolves with
/// the same semantics as a plain read: a written/seeded value, else (in
/// whole-project mode) the channel's externally-driven enum startup default, else
/// — in single-function/cone mode — a fail-loud [`EvalError::MissingInput`]. A
/// non-enum value is a `TypeError` (never a guessed integer).
fn read_enum_at(canon: &str, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    let value = crate::expr::read_symbol(canon, ctx)?;
    if matches!(value, Value::Enum { .. }) {
        Ok(value)
    } else {
        Err(EvalError::TypeError {
            detail: format!("value at {canon:?} is not an enum value"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calib::Calibration;
    use crate::env::{Env, StateStore};
    use m1_typecheck::Project;
    use std::path::Path;

    /// A harness owning the stores so a fresh `EvalCtx` can be built per call,
    /// over the synthetic enums fixture (`Drive State` = {Idle:0, Precharging:2},
    /// channel `Root.Demo.Mode`, value-compound `Root.Demo.Compound`).
    struct Harness {
        project: Project,
        calib: Calibration,
        env: Env,
        state: StateStore,
    }

    impl Harness {
        fn new() -> Harness {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/enums");
            let loaded =
                crate::loader::load(&dir.join("Project.m1prj"), None).expect("enums fixture loads");
            Harness {
                project: loaded.project,
                calib: Calibration::default(),
                env: Env::new(),
                state: StateStore::new(),
            }
        }

        fn enum_id(&self) -> usize {
            self.project
                .symbols()
                .enum_by_name("Drive State")
                .expect("Drive State enum")
        }

        fn ctx(&mut self) -> EvalCtx<'_> {
            EvalCtx {
                project: &self.project,
                calib: &self.calib,
                env: &mut self.env,
                state: &mut self.state,
                group: Some("Root.Demo"),
                fn_symbol: Some("Root.Demo.Update"),
                script_name: "Demo.Update.m1scr",
                dt: 0.01,
                scripts: &[],
                signature_m1_types: None,
                object_rules: None,
                depth: 0,
                trace: None,
            }
        }

        fn as_int(&mut self, object: &str) -> Result<Option<Value>, EvalError> {
            let mut ctx = self.ctx();
            as_integer(object, &mut ctx)
        }
    }

    // ---- Form 1: enum-type-qualified member literal ----

    #[test]
    fn literal_form_returns_member_container_order() {
        let mut h = Harness::new();
        // Idle has ContainerOrder 0.
        assert_eq!(
            h.as_int("Drive State.Idle").unwrap(),
            Some(Value::m1_integer(0))
        );
        // Precharging has ContainerOrder 2 (NOT ordinal index 1) — proves the
        // declared value is used, not the position in the member list.
        assert_eq!(
            h.as_int("Drive State.Precharging").unwrap(),
            Some(Value::m1_integer(2))
        );
    }

    #[test]
    fn literal_form_unknown_member_fails_loud() {
        let mut h = Harness::new();
        // The prefix is a real enum type but `Nope` is not one of its members.
        match h.as_int("Drive State.Nope") {
            Err(EvalError::TypeError { .. }) => {}
            other => panic!("expected TypeError for unknown member, got {other:?}"),
        }
    }

    // ---- Form 2: value-holding source ----

    #[test]
    fn value_form_reads_enum_typed_channel() {
        let mut h = Harness::new();
        let id = h.enum_id();
        // Seed the channel with the current enum value.
        h.env.set(
            "Root.Demo.Mode",
            Value::Enum {
                id,
                member: "Precharging".to_string(),
            },
        );
        // `Mode` resolves (group-relative) to Root.Demo.Mode, an enum channel.
        assert_eq!(h.as_int("Mode").unwrap(), Some(Value::m1_integer(2)));
        // And the absolute path works too.
        assert_eq!(
            h.as_int("Root.Demo.Mode").unwrap(),
            Some(Value::m1_integer(2))
        );
    }

    #[test]
    fn value_form_reads_compound_dot_value_child() {
        let mut h = Harness::new();
        let id = h.enum_id();
        // The value-compound's enum value lives on its `.Value` child.
        h.env.set(
            "Root.Demo.Compound.Value",
            Value::Enum {
                id,
                member: "Idle".to_string(),
            },
        );
        // Addressing the compound itself reads through to its `.Value` child.
        assert_eq!(
            h.as_int("Root.Demo.Compound").unwrap(),
            Some(Value::m1_integer(0))
        );
    }

    #[test]
    fn value_form_unset_channel_is_missing_input() {
        let mut h = Harness::new();
        // The channel is an enum source but no value was seeded: fail loud.
        match h.as_int("Root.Demo.Mode") {
            Err(EvalError::MissingInput { channel }) => {
                assert_eq!(channel, "Root.Demo.Mode");
            }
            other => panic!("expected MissingInput, got {other:?}"),
        }
    }

    #[test]
    fn non_enum_source_falls_through() {
        let mut h = Harness::new();
        // A name that resolves to no enum type and no project symbol returns
        // Ok(None) so dispatch can try other routes.
        assert_eq!(h.as_int("Totally.Unknown.Thing").unwrap(), None);
    }
}
