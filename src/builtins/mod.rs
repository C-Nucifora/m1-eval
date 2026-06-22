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
pub mod io_stub;
pub mod limit;
pub mod stateful;

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
    site: CallSite,
    ctx: &mut EvalCtx,
) -> Result<Value, EvalError> {
    // 1. Table `.Lookup()` — the object is a project table symbol, not a library
    //    object. Classify it against the project; a Table + `Lookup` interpolates.
    if method == "Lookup"
        && let Some(value) = try_table_lookup(object, args, ctx)?
    {
        return Ok(value);
    }

    // 2. Pure library objects. Validate arity against the intrinsic signatures
    //    first (a wrong arg count is a BadCall, an unknown method an
    //    UnsupportedBuiltin), then route to the implementing submodule.
    match object {
        "Calculate" | "Limit" | "Convert" => {
            validate_arity(object, method, args.len())?;
            // A stateful `Calculate.*` method (Stable/Hysteresis/Between/Beyond)
            // is a time-domain operator, not a pure one: route it to the stateful
            // engine. The pure `Calculate.*` math stays in its own submodule.
            if object == "Calculate"
                && let Some(v) = stateful::call(object, method, args, site.clone(), ctx)?
            {
                return Ok(v);
            }
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
        // 3. Stateful (time-domain) library objects: each is a state machine keyed
        //    by `site` and advanced by `ctx.dt`. Validate arity, then evaluate.
        "Filter" | "Integral" | "Derivative" | "Debounce" | "Delay" | "Change" => {
            validate_arity(object, method, args.len())?;
            match stateful::call(object, method, args, site, ctx)? {
                Some(v) => Ok(v),
                // The object is stateful but this specific method is not yet
                // implemented (e.g. the buffered `Delay.SignalN`): fail loud.
                None => Err(unsupported(object, method)),
            }
        }
        // 4. Tier-3 IO objects: scenario-fed / documented stubs.
        "CanComms" | "Serial" | "System" | "Logging" => {
            io_stub::call(object, method, args, ctx)
        }
        // 4b. `Math` is a *calibration-only* library object (its functions are
        //     flagged `calibrationOnly` in the intrinsics) and is not, strictly,
        //     valid in ECU `.m1scr` scripts — yet real EV-M1 control scripts
        //     reference `Math.atan2`. We route that one function to the same
        //     `y.atan2(x)` as `Calculate.InverseTan2` (a pragmatic, faithful
        //     evaluation) and flag it `Stubbed` in coverage so the user sees it is
        //     a calibration-only object surfaced in an ECU script. Every other
        //     `Math.*` method is left to fail loud (we do not implement the
        //     calibration maths library wholesale).
        "Math" => {
            validate_arity(object, method, args.len())?;
            match method {
                "atan2" => {
                    let y = args[0].as_f64()?;
                    let x = args[1].as_f64()?;
                    Ok(Value::Float(y.atan2(x)))
                }
                _ => Err(unsupported(object, method)),
            }
        }
        // 5. Not a library object and not a table lookup. It may be a project
        //    object (e.g. a `Timer`) carrying an intrinsic object-method, or
        //    something genuinely unsupported.
        _ => dispatch_object_method(object, method, args, site, ctx),
    }
}

/// Dispatch a method call whose `object` is not a firmware library object — it is
/// a project object (a `Timer`, a CAN signal, …) carrying an *object method*
/// (`Start`/`Remaining`/`Receive`/…) from the intrinsic registry. Only the
/// stateful `Timer` methods are implemented in Phase 1; everything else fails
/// loud.
fn dispatch_object_method(
    object: &str,
    method: &str,
    args: &[Value],
    _site: CallSite,
    ctx: &mut EvalCtx,
) -> Result<Value, EvalError> {
    // The Timer object methods (Start/Stop/Reset/Remaining) are stateful and must
    // share one countdown across all of an object's method calls — so key the
    // state by the object *path*, not the individual call site. A canonical path
    // is preferred (so `This.MyTimer` and `Root.Demo.MyTimer` address the same
    // timer); fall back to the source spelling if the object does not resolve.
    let object_key = timer_object_key(object, ctx);
    if let Some(v) = stateful::timer(method, args, object_key, ctx)? {
        return Ok(v);
    }
    Err(unsupported(object, method))
}

/// The state key for a Timer object: a [`CallSite`] whose script slot is the
/// object's canonical path (offset 0), so every method call on the same Timer
/// shares one countdown. Resolves the object spelling against the project for
/// path stability; falls back to the raw spelling when unresolved.
fn timer_object_key(object: &str, ctx: &EvalCtx) -> CallSite {
    let canon = match classify(object, ctx.group, ctx.fn_symbol, ctx.project, &ctx.env.locals) {
        Target::Symbol(path) => path,
        _ => object.to_string(),
    };
    CallSite::new(canon, 0)
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

/// How the engine handles a given builtin `Object.Method`. Drives the `--coverage`
/// report (Task 28): a method is **supported** when the dispatch table evaluates
/// it directly, **stubbed** when a Tier-3 IO object returns a documented offline
/// value (or is scenario-fed), and **unsupported** when no branch implements it
/// (it would fail loud at runtime).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinSupport {
    /// Evaluated faithfully by the dispatch table.
    Supported,
    /// A Tier-3 IO object handled as a documented/scenario-fed stub.
    Stubbed,
    /// Not implemented — fails loud at runtime.
    Unsupported,
}

/// Library/object methods implemented faithfully (Tier-1 + Tier-2). Kept in sync
/// with the dispatch arms above; this is the single source of truth the coverage
/// report consults so it never disagrees with what `dispatch` actually evaluates.
const SUPPORTED_METHODS: &[(&str, &str)] = &[
    // Calculate.* pure math.
    ("Calculate", "Max"),
    ("Calculate", "Min"),
    ("Calculate", "Absolute"),
    ("Calculate", "Average"),
    ("Calculate", "Modulo"),
    ("Calculate", "Bias"),
    ("Calculate", "PI"),
    ("Calculate", "NAN"),
    ("Calculate", "Infinity"),
    ("Calculate", "MaximumFloat"),
    ("Calculate", "Floor"),
    ("Calculate", "Ceiling"),
    ("Calculate", "Power"),
    ("Calculate", "FastSquareRoot"),
    ("Calculate", "IsNAN"),
    ("Calculate", "IsFinite"),
    ("Calculate", "FastSin"),
    ("Calculate", "FastCos"),
    ("Calculate", "FastTan"),
    ("Calculate", "InverseSin"),
    ("Calculate", "InverseCos"),
    ("Calculate", "InverseTan"),
    ("Calculate", "InverseTan2"),
    // Calculate.* stateful predicates.
    ("Calculate", "Stable"),
    ("Calculate", "Hysteresis"),
    ("Calculate", "Between"),
    ("Calculate", "Beyond"),
    // Limit.* and Convert.*.
    ("Limit", "Range"),
    ("Limit", "Max"),
    ("Limit", "Min"),
    ("Convert", "ToInteger"),
    ("Convert", "ToUnsignedInteger"),
    // Stateful Filter/Integral/Derivative.
    ("Filter", "FirstOrder"),
    ("Filter", "Maximum"),
    ("Filter", "Minimum"),
    ("Integral", "Normal"),
    ("Derivative", "Normal"),
    ("Derivative", "Filtered"),
    ("Derivative", "Adaptive"),
    // Delay/Debounce/Change.
    ("Delay", "Rising"),
    ("Delay", "Falling"),
    ("Delay", "Stable"),
    ("Debounce", "Stable"),
    ("Debounce", "Fast"),
    ("Debounce", "Verify"),
    ("Change", "By"),
    ("Change", "Up"),
    ("Change", "Down"),
    ("Change", "To"),
    ("Change", "From"),
    ("Change", "Either"),
];

/// The Tier-3 IO library objects: their methods are handled as documented/
/// scenario-fed stubs (flagged externally driven), not faithfully evaluated.
const STUB_OBJECTS: &[&str] = &["CanComms", "Serial", "System", "Logging"];

/// Individual `(object, method)` pairs handled as documented stubs even though
/// their object is not a whole stub object. `Math.atan2` is the calibration-only
/// `Math` object surfaced in an ECU script: it is routed (to `y.atan2(x)`) but
/// flagged external/stubbed so coverage stays honest about its provenance.
const STUB_METHODS: &[(&str, &str)] = &[("Math", "atan2")];

/// Classify a builtin `object.method` for the coverage report.
///
/// A `Lookup` method on any object is treated as **supported** here — it is the
/// table-interpolation path, resolved at runtime against a project table symbol
/// (coverage cannot see the calibration, so it reports the construct as supported
/// and the runtime fails loud if the specific object is not a table).
pub fn classify_builtin(object: &str, method: &str) -> BuiltinSupport {
    if method == "Lookup" {
        return BuiltinSupport::Supported;
    }
    if SUPPORTED_METHODS.contains(&(object, method)) {
        return BuiltinSupport::Supported;
    }
    if STUB_OBJECTS.contains(&object) || STUB_METHODS.contains(&(object, method)) {
        return BuiltinSupport::Stubbed;
    }
    BuiltinSupport::Unsupported
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
    fn stateful_calculate_method_dispatches_to_state_engine() {
        let mut h = Harness::new();
        // Calculate.Stable (arity 2) is a stateful predicate: M6 routes it to the
        // state engine. Its first tick has not yet been stable, so it is false —
        // a real evaluated value, not a fail-loud error.
        match h.call("Calculate", "Stable", &[Value::Float(1.0), Value::Float(0.1)]) {
            Ok(Value::Bool(false)) => {}
            other => panic!("expected Ok(Bool(false)) on first tick, got {other:?}"),
        }
    }

    #[test]
    fn filter_first_order_dispatches_to_state_engine() {
        let mut h = Harness::new();
        // A stateful library object routes through dispatch with arity validation;
        // the first tick of FirstOrder seeds to the input (1.0).
        match h.call("Filter", "FirstOrder", &[Value::Float(1.0), Value::Float(0.1)]) {
            Ok(Value::Float(x)) => assert!((x - 1.0).abs() < 1e-9),
            other => panic!("expected seeded Float(1.0), got {other:?}"),
        }
    }

    #[test]
    fn stateful_wrong_arity_is_bad_call() {
        let mut h = Harness::new();
        // Integral.Normal needs five arguments; fewer is a BadCall, not a guess.
        match h.call("Integral", "Normal", &[Value::Float(1.0)]) {
            Err(EvalError::BadCall { .. }) => {}
            other => panic!("expected BadCall, got {other:?}"),
        }
    }

    #[test]
    fn unimplemented_stateful_method_fails_loud() {
        let mut h = Harness::new();
        // Delay.Signal15 is a buffered sample delay we do not implement; the
        // object is recognised but the method falls through to fail loud.
        match h.call("Delay", "Signal15", &[Value::Float(1.0), Value::Int(3)]) {
            Err(EvalError::UnsupportedBuiltin { object, method }) => {
                assert_eq!(object, "Delay");
                assert_eq!(method, "Signal15");
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

    // ---- Math.atan2 (calibration-only object surfaced in ECU scripts) ----

    #[test]
    fn math_atan2_routes_to_atan2() {
        let mut h = Harness::new();
        // atan2(1, 1) = pi/4. The calibration-only `Math` object is surfaced in
        // real ECU scripts; we route its `atan2` to the same evaluation as
        // Calculate.InverseTan2 (and flag it Stubbed for coverage).
        match h.call("Math", "atan2", &[Value::Float(1.0), Value::Float(1.0)]) {
            Ok(Value::Float(x)) => assert!((x - std::f64::consts::FRAC_PI_4).abs() < 1e-12),
            other => panic!("expected Float(pi/4), got {other:?}"),
        }
    }

    #[test]
    fn math_atan2_wrong_arity_is_bad_call() {
        let mut h = Harness::new();
        // Math.atan2 takes two arguments (validated against intrinsics).
        match h.call("Math", "atan2", &[Value::Float(1.0)]) {
            Err(EvalError::BadCall { .. }) => {}
            other => panic!("expected BadCall, got {other:?}"),
        }
    }

    #[test]
    fn math_atan2_is_classified_stubbed() {
        // Coverage flags Math.atan2 as a stub: it is a calibration-only object
        // surfaced in an ECU script, routed pragmatically but marked external.
        assert_eq!(classify_builtin("Math", "atan2"), BuiltinSupport::Stubbed);
    }

    #[test]
    fn math_unknown_method_is_unsupported() {
        let mut h = Harness::new();
        // Only `atan2` is routed from the calibration-only Math object; anything
        // else fails loud rather than being silently evaluated.
        match h.call("Math", "Sqrt", &[Value::Float(4.0)]) {
            Err(EvalError::UnsupportedBuiltin { object, method }) => {
                assert_eq!(object, "Math");
                assert_eq!(method, "Sqrt");
            }
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
