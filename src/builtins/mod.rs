// SPDX-License-Identifier: GPL-3.0-or-later
//! Builtin call dispatch.
//!
//! Every `Object.Method(...)` builtin call in an M1 script routes through
//! [`dispatch`]. M5 wires the *pure* builtins:
//!
//! - the math/clamp/convert library objects `Calculate.*`, `Limit.*`,
//!   `Convert.*` (see the [`calculate`], [`limit`], [`convert`] submodules), and
//! - table `.Lookup()` interpolation over the loaded calibration.
//!
//! Arity is validated up front against `m1_typecheck::intrinsics` (the builtin
//! *signature* registry): a call whose argument count matches no overload of the
//! named method is a fail-loud [`EvalError::BadCall`]; a method the registry does
//! not list on the object is an [`EvalError::UnsupportedBuiltin`].
//!
//! The stateful operators (`Filter`/`Integral`/`Delay`/… and the stateful
//! `Calculate.{Stable,Hysteresis,Between,Beyond}`) and the Tier-3 IO objects
//! arrive in later milestones; until then they match no implemented branch here
//! and fall through to the fail-loud default. That default is the whole point:
//! an unimplemented builtin must surface as an error, never a guessed number.

pub mod calculate;
pub mod convert;
pub mod limit;

use crate::env::CallSite;
use crate::error::EvalError;
use crate::expr::EvalCtx;
use crate::ident::{Target, classify};
use crate::value::Value;
use m1_typecheck::intrinsics;
use m1_typecheck::symbols::SymbolKind;

/// Dispatch one builtin call `object.method(args)`.
///
/// `object`/`method` are the source spellings of the callee's member parts;
/// `args` are the already-evaluated arguments (left to right). `site` is the
/// stable [`CallSite`] of the call node, which the stateful operators (M6) use
/// to key per-occurrence state. `ctx` carries the evaluation environment
/// (project model, calibration, value/state stores, `dt`, lexical context).
///
/// Resolution order:
/// 1. A `Lookup` method on a project [`SymbolKind::Table`] object → table
///    interpolation over the calibration.
/// 2. A pure library object (`Calculate`/`Limit`/`Convert`) → the matching
///    submodule, after arity validation against the intrinsic library.
/// 3. Anything else → fail loud ([`EvalError::UnsupportedBuiltin`]).
pub fn dispatch(
    object: &str,
    method: &str,
    args: &[Value],
    _site: CallSite,
    ctx: &mut EvalCtx,
) -> Result<Value, EvalError> {
    // 1. Table `.Lookup()` — the object is a project table symbol, not a library
    //    object. Classify it against the project; a Table + `Lookup` interpolates.
    if method == "Lookup" {
        if let Some(value) = try_table_lookup(object, args, ctx)? {
            return Ok(value);
        }
    }

    // 2. Pure library objects. Validate arity against the intrinsic signatures
    //    first (a wrong arg count is a BadCall, an unknown method an
    //    UnsupportedBuiltin), then route to the implementing submodule.
    match object {
        "Calculate" | "Limit" | "Convert" => {
            validate_arity(object, method, args.len())?;
            let result = match object {
                "Calculate" => calculate::call(method, args)?,
                "Limit" => limit::call(method, args)?,
                "Convert" => convert::call(method, args)?,
                _ => unreachable!(),
            };
            // `Some` -> the submodule handled it. `None` -> a known-but-stateful
            // or otherwise-unimplemented method on a pure object: fail loud.
            match result {
                Some(v) => Ok(v),
                None => Err(unsupported(object, method)),
            }
        }
        // 3. Not a pure library object and not a table lookup: unimplemented.
        _ => Err(unsupported(object, method)),
    }
}

/// Attempt a table `.Lookup()`. Returns `Ok(Some(value))` when `object` resolves
/// to a project table and the calibration carries its cells; `Ok(None)` when
/// `object` is not a table (so the caller continues to library-object dispatch);
/// and an error when the table exists but the lookup cannot proceed (missing
/// calibration values, wrong arity).
fn try_table_lookup(
    object: &str,
    args: &[Value],
    ctx: &mut EvalCtx,
) -> Result<Option<Value>, EvalError> {
    // Resolve the object spelling to a canonical symbol path in the current scope.
    let target = classify(object, ctx.group, ctx.fn_symbol, ctx.project, &ctx.env.locals);
    let Target::Symbol(canon) = target else {
        return Ok(None);
    };
    // Only a Table symbol has a `.Lookup()`.
    let is_table = ctx
        .project
        .symbols()
        .get(&canon)
        .map(|s| s.kind == SymbolKind::Table)
        .unwrap_or(false);
    if !is_table {
        return Ok(None);
    }

    // The cells live in the calibration, keyed by the name the `.m1cfg` wrote.
    // Real exports omit the implicit leading `Root.` group prefix, so try the
    // canonical path first, then the `Root.`-stripped form (mirrors parameter
    // lookup in the expression evaluator).
    let table = ctx
        .calib
        .table(&canon)
        .or_else(|| canon.strip_prefix("Root.").and_then(|p| ctx.calib.table(p)))
        .ok_or_else(|| EvalError::MissingCalibration { path: canon.clone() })?;

    // Each lookup coordinate must be numeric; collect them then interpolate.
    let mut inputs = Vec::with_capacity(args.len());
    for a in args {
        inputs.push(a.as_f64()?);
    }
    // `table::lookup` validates arity (inputs vs axes) and clamps out-of-range
    // coordinates, returning a BadCall on a mismatch.
    let value = crate::table::lookup(table, &inputs)?;
    Ok(Some(Value::Float(value)))
}

/// Validate the argument count of a library-object call against the intrinsic
/// signature registry. The method must exist on the object (else
/// [`EvalError::UnsupportedBuiltin`]) and `argc` must match some overload's
/// parameter count (else [`EvalError::BadCall`]).
fn validate_arity(object: &str, method: &str, argc: usize) -> Result<(), EvalError> {
    let overloads = intrinsics::get().library_overloads(object, method);
    if overloads.is_empty() {
        // The registry lists no such method on this object.
        return Err(unsupported(object, method));
    }
    let accepted: Vec<usize> = overloads.iter().map(|o| o.params.len()).collect();
    if accepted.contains(&argc) {
        Ok(())
    } else {
        Err(EvalError::BadCall {
            detail: format!(
                "{object}.{method} expects {} argument(s), got {argc}",
                arities_display(&accepted)
            ),
        })
    }
}

/// Render the accepted arities for a `BadCall` message, deduplicated and sorted
/// (an overloaded method may accept several counts).
fn arities_display(accepted: &[usize]) -> String {
    let mut counts: Vec<usize> = accepted.to_vec();
    counts.sort_unstable();
    counts.dedup();
    counts
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(" or ")
}

fn unsupported(object: &str, method: &str) -> EvalError {
    EvalError::UnsupportedBuiltin {
        object: object.to_string(),
        method: method.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calib::Calibration;
    use crate::env::{Env, StateStore};
    use m1_typecheck::Project;
    use std::path::Path;

    /// Load the synthetic mini fixture project (with calibration) for the
    /// table-lookup and resolution-backed tests.
    fn mini_loaded() -> crate::loader::Loaded {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini");
        crate::loader::load(&dir.join("Project.m1prj"), Some(&dir.join("parameters.m1cfg")))
            .expect("mini fixture loads")
    }

    /// A harness owning the stores so a fresh `EvalCtx` can be built per call.
    struct Harness {
        project: Project,
        calib: Calibration,
        env: Env,
        state: StateStore,
    }

    impl Harness {
        fn new() -> Harness {
            let loaded = mini_loaded();
            Harness {
                project: loaded.project,
                calib: loaded.calib,
                env: Env::new(),
                state: StateStore::new(),
            }
        }

        fn empty_calib() -> Harness {
            let mut h = Harness::new();
            h.calib = Calibration::default();
            h
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

        fn call(&mut self, object: &str, method: &str, args: &[Value]) -> Result<Value, EvalError> {
            let site = CallSite::new("Demo.Update.m1scr", 0);
            let mut ctx = self.ctx();
            dispatch(object, method, args, site, &mut ctx)
        }
    }

    // ---- pure library dispatch ----

    #[test]
    fn calculate_max_dispatches() {
        let mut h = Harness::new();
        assert_eq!(
            h.call("Calculate", "Max", &[Value::Int(2), Value::Int(3)]).unwrap(),
            Value::Int(3)
        );
    }

    #[test]
    fn limit_range_dispatches() {
        let mut h = Harness::new();
        assert_eq!(
            h.call(
                "Limit",
                "Range",
                &[Value::Float(9.0), Value::Float(0.0), Value::Float(5.0)]
            )
            .unwrap(),
            Value::Float(5.0)
        );
    }

    #[test]
    fn convert_to_integer_dispatches() {
        let mut h = Harness::new();
        assert_eq!(
            h.call("Convert", "ToInteger", &[Value::Float(2.9)]).unwrap(),
            Value::Int(2)
        );
    }

    // ---- arity validation against intrinsics ----

    #[test]
    fn wrong_arity_is_bad_call() {
        let mut h = Harness::new();
        // Calculate.Max takes two arguments; one is a BadCall, not a guess.
        match h.call("Calculate", "Max", &[Value::Int(1)]) {
            Err(EvalError::BadCall { .. }) => {}
            other => panic!("expected BadCall, got {other:?}"),
        }
        // Limit.Range takes three.
        match h.call("Limit", "Range", &[Value::Int(1), Value::Int(2)]) {
            Err(EvalError::BadCall { .. }) => {}
            other => panic!("expected BadCall, got {other:?}"),
        }
    }

    #[test]
    fn unknown_method_on_pure_object_is_unsupported() {
        let mut h = Harness::new();
        match h.call("Calculate", "NotAMethod", &[Value::Int(1)]) {
            Err(EvalError::UnsupportedBuiltin { object, method }) => {
                assert_eq!(object, "Calculate");
                assert_eq!(method, "NotAMethod");
            }
            other => panic!("expected UnsupportedBuiltin, got {other:?}"),
        }
    }

    #[test]
    fn stateful_calculate_method_is_unsupported_until_m6() {
        let mut h = Harness::new();
        // Calculate.Stable exists in intrinsics (arity 2) but is stateful: not
        // implemented in M5, so it must fail loud, not no-op.
        match h.call("Calculate", "Stable", &[Value::Float(1.0), Value::Float(0.1)]) {
            Err(EvalError::UnsupportedBuiltin { object, method }) => {
                assert_eq!(object, "Calculate");
                assert_eq!(method, "Stable");
            }
            other => panic!("expected UnsupportedBuiltin, got {other:?}"),
        }
    }

    #[test]
    fn unknown_object_is_unsupported() {
        let mut h = Harness::new();
        match h.call("NoSuchObject", "Whatever", &[]) {
            Err(EvalError::UnsupportedBuiltin { .. }) => {}
            other => panic!("expected UnsupportedBuiltin, got {other:?}"),
        }
    }

    // ---- table .Lookup() ----

    #[test]
    fn table_lookup_interpolates_over_calibration() {
        let mut h = Harness::new();
        // The mini fixture's Demo.Map is 2-D: x in {0,100}, y in {0,1}, body
        // (10,20,30,40). Corner and midpoint values come straight from table.rs.
        assert_eq!(
            h.call("Map", "Lookup", &[Value::Float(0.0), Value::Float(0.0)]).unwrap(),
            Value::Float(10.0)
        );
        assert_eq!(
            h.call("Map", "Lookup", &[Value::Float(100.0), Value::Float(1.0)]).unwrap(),
            Value::Float(40.0)
        );
        // Halfway in x at y=0: between 10 and 30 -> 20.
        assert_eq!(
            h.call("Map", "Lookup", &[Value::Float(50.0), Value::Float(0.0)]).unwrap(),
            Value::Float(20.0)
        );
        // Out-of-range inputs clamp.
        assert_eq!(
            h.call("Map", "Lookup", &[Value::Float(999.0), Value::Float(9.0)]).unwrap(),
            Value::Float(40.0)
        );
    }

    #[test]
    fn table_lookup_wrong_arity_is_bad_call() {
        let mut h = Harness::new();
        // Demo.Map has two axes; one coordinate is a BadCall.
        match h.call("Map", "Lookup", &[Value::Float(0.0)]) {
            Err(EvalError::BadCall { .. }) => {}
            other => panic!("expected BadCall, got {other:?}"),
        }
    }

    #[test]
    fn table_lookup_without_calibration_is_missing() {
        let mut h = Harness::empty_calib();
        // The table symbol resolves, but no calibration cells were loaded.
        match h.call("Map", "Lookup", &[Value::Float(0.0), Value::Float(0.0)]) {
            Err(EvalError::MissingCalibration { .. }) => {}
            other => panic!("expected MissingCalibration, got {other:?}"),
        }
    }

    #[test]
    fn lookup_on_non_table_is_not_a_table_lookup() {
        let mut h = Harness::new();
        // `Calculate.Lookup` is not a table lookup; Calculate has no Lookup
        // overload either, so it is UnsupportedBuiltin (fail loud), not a panic.
        match h.call("Calculate", "Lookup", &[Value::Float(0.0)]) {
            Err(EvalError::UnsupportedBuiltin { .. }) => {}
            other => panic!("expected UnsupportedBuiltin, got {other:?}"),
        }
    }
}
