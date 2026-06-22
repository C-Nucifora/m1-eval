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
use m1_typecheck::symbols::SymbolKind;

/// Convert `<object>.AsInteger()` to its declared enum integer.
///
/// Returns:
/// - `Ok(Some(Value::Int(n)))` when `object` is a recognised enum source (either
///   form) and the conversion succeeds;
/// - `Ok(None)` when `object` is neither an enum-type-qualified member literal
///   nor a value-holding enum source — so the caller can fall through to other
///   dispatch (e.g. a Timer object method);
/// - a fail-loud [`EvalError`] when `object` *is* an enum source but the
///   conversion cannot proceed (an unknown member, an unset enum channel, or a
///   non-enum runtime value) — never a guessed integer.
pub fn as_integer(object: &str, ctx: &mut EvalCtx) -> Result<Option<Value>, EvalError> {
    // Form 1: an enum-type-qualified member literal `<EnumTypeName>.<Member>`.
    // Split on the rightmost `.` only — both the type name and the member may
    // contain spaces.
    if let Some((prefix, leaf)) = object.rsplit_once('.')
        && let Some(id) = ctx.project.symbols().enum_by_name(prefix)
    {
        if ctx.project.symbols().enum_has_member(id, leaf) {
            let v = Value::Enum {
                id,
                member: leaf.to_string(),
            };
            return Ok(Some(Value::Int(v.as_enum_int(ctx.project)?)));
        }
        // The prefix *is* an enum type but the leaf is not one of its members:
        // a fail-loud error rather than a silent miss — the author wrote an
        // enum-literal `.AsInteger` against a non-member.
        return Err(EvalError::TypeError {
            detail: format!("{leaf:?} is not a member of enum {prefix:?}"),
        });
    }

    // Form 2: a value-holding source. Classify the object against the project.
    let target = classify(object, ctx.group, ctx.fn_symbol, ctx.project, &ctx.env.locals);
    let Target::Symbol(canon) = target else {
        // Not an enum literal and not a resolvable project symbol: let the caller
        // fall through to other dispatch.
        return Ok(None);
    };
    let Some(kind) = ctx.project.symbols().get(&canon).map(|s| s.kind) else {
        return Ok(None);
    };

    match kind {
        // An enum-typed channel (or parameter): read its current enum value.
        SymbolKind::Channel | SymbolKind::Parameter => {
            convert_value_at(&canon, ctx).map(Some)
        }
        // A value-compound: the enum value lives on its `.Value` child.
        SymbolKind::Group => {
            let value_path = format!("{canon}.Value");
            // Only treat it as a value-compound when the `.Value` child exists.
            if ctx.project.symbols().get(&value_path).is_none() {
                return Ok(None);
            }
            convert_value_at(&value_path, ctx).map(Some)
        }
        // Any other symbol kind is not an enum source — fall through.
        _ => Ok(None),
    }
}

/// Read the current value at a canonical channel path and convert it to its enum
/// integer. The value must already be a [`Value::Enum`] in the env (seeded as a
/// scenario input or written by an earlier statement); an unset channel is a
/// fail-loud [`EvalError::MissingInput`] and a non-enum value a `TypeError`.
fn convert_value_at(canon: &str, ctx: &EvalCtx) -> Result<Value, EvalError> {
    let value = ctx
        .env
        .get(canon)
        .ok_or_else(|| EvalError::MissingInput {
            channel: canon.to_string(),
        })?;
    Ok(Value::Int(value.as_enum_int(ctx.project)?))
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
            let loaded = crate::loader::load(&dir.join("Project.m1prj"), None)
                .expect("enums fixture loads");
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
        assert_eq!(h.as_int("Drive State.Idle").unwrap(), Some(Value::Int(0)));
        // Precharging has ContainerOrder 2 (NOT ordinal index 1) — proves the
        // declared value is used, not the position in the member list.
        assert_eq!(
            h.as_int("Drive State.Precharging").unwrap(),
            Some(Value::Int(2))
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
        assert_eq!(h.as_int("Mode").unwrap(), Some(Value::Int(2)));
        // And the absolute path works too.
        assert_eq!(h.as_int("Root.Demo.Mode").unwrap(), Some(Value::Int(2)));
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
        assert_eq!(h.as_int("Root.Demo.Compound").unwrap(), Some(Value::Int(0)));
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
