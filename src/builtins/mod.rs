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
pub mod enum_conv;
pub mod io_stub;
pub mod limit;
pub mod stateful;
pub mod userfn;

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
        "CanComms" | "Serial" | "System" | "Logging" => io_stub::call(object, method, args, ctx),
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
/// a project object (a `Timer`, an enum source, a CAN signal, …) carrying an
/// *object method* (`AsInteger`/`Start`/`Remaining`/`Receive`/…) from the
/// intrinsic registry. The enum `.AsInteger` accessor and the stateful `Timer`
/// methods are implemented; everything else fails loud.
fn dispatch_object_method(
    object: &str,
    method: &str,
    args: &[Value],
    _site: CallSite,
    ctx: &mut EvalCtx,
) -> Result<Value, EvalError> {
    // Enum `.AsInteger()` — always called with empty parens. The object is either
    // an enum-type-qualified member literal (`Drive State.Idle`) or a
    // value-holding enum source (an enum channel / value-compound). Resolved at
    // runtime against the enum model; an object that is no enum source returns
    // `Ok(None)` here and falls through to the Timer attempt below. An object that
    // *is* an enum source but cannot convert (unknown member, unset channel)
    // fails loud inside `as_integer`.
    if method == "AsInteger"
        && args.is_empty()
        && let Some(v) = enum_conv::as_integer(object, ctx)?
    {
        return Ok(v);
    }

    // Channel `.Set(value)` — the imperative setter. When the object resolves to a
    // `Channel`/`Parameter` symbol, this is a *write* of that channel (exactly
    // what `m1-typecheck` `schedule.rs` and `summary::io_sets` treat `Chan.Set*`
    // as), not a fail-loud unknown method. Falls through to IO/Timer routing when
    // the object is not a channel.
    if method == "Set"
        && let Some(v) = try_channel_set(object, args, ctx)?
    {
        return Ok(v);
    }

    // Project-object IO (DBC CAN messages/signals, a `Service Bits` GroupCompound
    // push, a package `Output.SetState`, a buzzer's `.Buzze`). These are the
    // project-object analogue of the Tier-3 library stubs: externally driven,
    // never truly evaluated offline. Route an object that classifies to a
    // package/group/reference symbol — or an unresolved DBC object (no `.m1dbc`
    // loaded) — through the IO stub, *before* the Timer fallback so a real Timer
    // (which resolves to its own object) is never swallowed by a stub method that
    // a Timer does not carry.
    if is_io_stub_object(object, ctx) && is_project_object_io_method(method) {
        return io_stub::project_object_call(object, method, args, ctx);
    }

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

/// Attempt a channel `.Set(value)` imperative setter. Returns `Ok(Some(unit))`
/// when `object` resolves to a `Channel`/`Parameter` symbol — writing the single
/// evaluated argument to its canonical path and recording the write to the trace
/// — and `Ok(None)` when `object` is not a channel (so dispatch falls through to
/// IO/Timer routing). A wrong argument count is a fail-loud [`EvalError::BadCall`]
/// (the setter takes exactly one value).
fn try_channel_set(
    object: &str,
    args: &[Value],
    ctx: &mut EvalCtx,
) -> Result<Option<Value>, EvalError> {
    // Only a project channel/parameter has an imperative `.Set`.
    let Target::Symbol(canon) = classify(
        object,
        ctx.group,
        ctx.fn_symbol,
        ctx.project,
        &ctx.env.locals,
    ) else {
        return Ok(None);
    };
    let is_writable = ctx
        .project
        .symbols()
        .get(&canon)
        .map(|s| matches!(s.kind, SymbolKind::Channel | SymbolKind::Parameter))
        .unwrap_or(false);
    if !is_writable {
        return Ok(None);
    }

    // The setter takes exactly one value — fail loud on any other arity rather
    // than guessing which argument to write.
    if args.len() != 1 {
        return Err(EvalError::BadCall {
            detail: format!("{object}.Set expects 1 argument, got {}", args.len()),
        });
    }
    // Coerce a numeric value written to an enum-typed channel to its enum member
    // (M1 enum channels store an integer; `Precharge State.Set(0)` writes the
    // member with that declared value), so the channel holds a typed enum value.
    let value = crate::expr::coerce_for_channel(&canon, args[0].clone(), ctx.project);
    ctx.env.set(canon.clone(), value.clone());
    if let Some(trace) = ctx.trace.as_deref_mut() {
        trace.record_channel(canon, value.clone());
    }
    // The setter is a statement-level write; reuse the unit value the IO void
    // writers return so an expression-statement call succeeds.
    Ok(Some(Value::Bool(true)))
}

/// Whether `method` is a project-object IO stub method (a CAN Tx/Get/Receive, a
/// GroupCompound `.Update`, an `Output.SetState`, a buzzer `.Buzze`).
fn is_project_object_io_method(method: &str) -> bool {
    io_stub::PROJECT_OBJECT_STUB_METHODS.contains(&method)
}

/// Whether `object` is a project object whose IO methods are externally-driven
/// stubs: it classifies to a package/group/reference symbol (`Object`/`Group`/
/// `Reference`/`Other`) — or it does not resolve at all, which is how a DBC CAN
/// object appears when no `.m1dbc` is loaded. A library object, a channel, a
/// table, or a function is *not* an IO-stub object (each has its own route).
fn is_io_stub_object(object: &str, ctx: &EvalCtx) -> bool {
    match classify(
        object,
        ctx.group,
        ctx.fn_symbol,
        ctx.project,
        &ctx.env.locals,
    ) {
        Target::Symbol(canon) => ctx
            .project
            .symbols()
            .get(&canon)
            .map(|s| {
                matches!(
                    s.kind,
                    SymbolKind::Object
                        | SymbolKind::Group
                        | SymbolKind::Reference
                        | SymbolKind::Other
                )
            })
            .unwrap_or(false),
        // An unresolved object spelling is the DBC case (the CAN message/signal is
        // sourced from a `.m1dbc` we did not load). A builtin/library object or a
        // resolved local is handled elsewhere and is never an IO-stub object.
        Target::Unresolved => true,
        Target::Local(_) | Target::Builtin { .. } => false,
    }
}

/// The state key for a Timer object: a [`CallSite`] whose script slot is the
/// object's canonical path (offset 0), so every method call on the same Timer
/// shares one countdown. Resolves the object spelling against the project for
/// path stability; falls back to the raw spelling when unresolved.
fn timer_object_key(object: &str, ctx: &EvalCtx) -> CallSite {
    let canon = match classify(
        object,
        ctx.group,
        ctx.fn_symbol,
        ctx.project,
        &ctx.env.locals,
    ) {
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
    let target = classify(
        object,
        ctx.group,
        ctx.fn_symbol,
        ctx.project,
        &ctx.env.locals,
    );
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
    let table = match ctx
        .calib
        .table(&canon)
        .or_else(|| canon.strip_prefix("Root.").and_then(|p| ctx.calib.table(p)))
    {
        Some(table) => table,
        // No calibration cells for this table. In whole-project mode (no `.m1cfg`)
        // the table is an unseeded externally-driven output, like a tunable
        // parameter: it falls back to the documented float default (its `.Value`
        // output type), flagged externally driven, rather than aborting the run. In
        // single-function / cone mode a `.Lookup` with no cells is still fail-loud
        // `MissingCalibration` — the user must supply the calibration.
        None if ctx.env.default_unseeded_channels => {
            if let Some(trace) = ctx.trace.as_deref_mut() {
                trace.mark_external(canon.clone());
            }
            return Ok(Some(Value::Float(0.0)));
        }
        None => {
            return Err(EvalError::MissingCalibration {
                path: canon.clone(),
            });
        }
    };

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

/// Project-object methods supported regardless of the object's *name* — the
/// object varies per project (a timer is `Startup Delay`, an enum source is
/// `Drive State.Idle` or `Control.Drive State`), but the method name fixes the
/// runtime route, so coverage classifies on the method alone:
///
/// - `AsInteger` is the enum→integer accessor (P15-B): resolved at runtime
///   against the enum model. Like `Lookup`, coverage cannot see the value, so it
///   is reported supported and the runtime fails loud if the object is not an
///   enum source.
/// - `Start`/`Stop`/`Reset`/`Remaining` are the project `Timer` object methods,
///   evaluated by `stateful::timer` at runtime. They were already evaluated but
///   the coverage report formerly flagged them unsupported; listing them here
///   reconciles coverage with what `dispatch_object_method` actually does.
/// - `Set` is the imperative channel setter (P15-C): resolved at runtime to a
///   channel write. Like `AsInteger`, coverage classifies on the method alone and
///   the runtime fails loud if the object is not a channel.
const SUPPORTED_OBJECT_METHODS: &[&str] =
    &["AsInteger", "Set", "Start", "Stop", "Reset", "Remaining"];

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
    // Object-name-independent project-object methods (`AsInteger` enum accessor,
    // the `Timer` Start/Stop/Reset/Remaining) — classified on the method alone,
    // because the object spelling is the project symbol's name, not a fixed
    // library object.
    if SUPPORTED_OBJECT_METHODS.contains(&method) {
        return BuiltinSupport::Supported;
    }
    if SUPPORTED_METHODS.contains(&(object, method)) {
        return BuiltinSupport::Supported;
    }
    // Project-object IO methods (DBC CAN Tx/Get/Receive, GroupCompound `.Update`,
    // `Output.SetState`, a buzzer `.Buzze`) are externally-driven documented stubs.
    // Classified on the method name alone — the object varies per project, but the
    // method fixes the offline route (matching `dispatch_object_method`).
    if io_stub::PROJECT_OBJECT_STUB_METHODS.contains(&method) {
        return BuiltinSupport::Stubbed;
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
        crate::loader::load(
            &dir.join("Project.m1prj"),
            Some(&dir.join("parameters.m1cfg")),
        )
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
                scripts: &[],
                depth: 0,
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
            h.call("Calculate", "Max", &[Value::Int(2), Value::Int(3)])
                .unwrap(),
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
            h.call("Convert", "ToInteger", &[Value::Float(2.9)])
                .unwrap(),
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
        match h.call(
            "Calculate",
            "Stable",
            &[Value::Float(1.0), Value::Float(0.1)],
        ) {
            Ok(Value::Bool(false)) => {}
            other => panic!("expected Ok(Bool(false)) on first tick, got {other:?}"),
        }
    }

    #[test]
    fn filter_first_order_dispatches_to_state_engine() {
        let mut h = Harness::new();
        // A stateful library object routes through dispatch with arity validation;
        // the first tick of FirstOrder seeds to the input (1.0).
        match h.call(
            "Filter",
            "FirstOrder",
            &[Value::Float(1.0), Value::Float(0.1)],
        ) {
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
            h.call("Map", "Lookup", &[Value::Float(0.0), Value::Float(0.0)])
                .unwrap(),
            Value::Float(10.0)
        );
        assert_eq!(
            h.call("Map", "Lookup", &[Value::Float(100.0), Value::Float(1.0)])
                .unwrap(),
            Value::Float(40.0)
        );
        // Halfway in x at y=0: between 10 and 30 -> 20.
        assert_eq!(
            h.call("Map", "Lookup", &[Value::Float(50.0), Value::Float(0.0)])
                .unwrap(),
            Value::Float(20.0)
        );
        // Out-of-range inputs clamp.
        assert_eq!(
            h.call("Map", "Lookup", &[Value::Float(999.0), Value::Float(9.0)])
                .unwrap(),
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

    // ---- enum .AsInteger through dispatch (P15-B, Task 5) ----

    /// A harness over the synthetic enums fixture so `.AsInteger` dispatch can
    /// resolve the project-local `Drive State` enum and its enum-typed channel.
    struct EnumHarness {
        project: Project,
        calib: Calibration,
        env: Env,
        state: StateStore,
    }

    impl EnumHarness {
        fn new() -> EnumHarness {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/enums");
            let loaded =
                crate::loader::load(&dir.join("Project.m1prj"), None).expect("enums fixture loads");
            EnumHarness {
                project: loaded.project,
                calib: Calibration::default(),
                env: Env::new(),
                state: StateStore::new(),
            }
        }

        fn enum_id(&self) -> usize {
            self.project.symbols().enum_by_name("Drive State").unwrap()
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
                depth: 0,
                trace: None,
            }
        }

        fn call(&mut self, object: &str, method: &str, args: &[Value]) -> Result<Value, EvalError> {
            let site = CallSite::new("Demo.Update.m1scr", 0);
            let mut ctx = self.ctx();
            dispatch(object, method, args, site, &mut ctx)
        }
    }

    #[test]
    fn dispatch_as_integer_on_enum_literal() {
        let mut h = EnumHarness::new();
        // `Drive State.Idle.AsInteger()` → 0 (ContainerOrder), via the literal form.
        assert_eq!(
            h.call("Drive State.Idle", "AsInteger", &[]).unwrap(),
            Value::Int(0)
        );
        // Precharging is ContainerOrder 2.
        assert_eq!(
            h.call("Drive State.Precharging", "AsInteger", &[]).unwrap(),
            Value::Int(2)
        );
    }

    #[test]
    fn dispatch_as_integer_on_enum_channel() {
        let mut h = EnumHarness::new();
        let id = h.enum_id();
        h.env.set(
            "Root.Demo.Mode",
            Value::Enum {
                id,
                member: "Precharging".to_string(),
            },
        );
        // The value form reads the channel's current enum value and converts it.
        assert_eq!(
            h.call("Root.Demo.Mode", "AsInteger", &[]).unwrap(),
            Value::Int(2)
        );
    }

    #[test]
    fn dispatch_as_integer_on_non_enum_fails_loud() {
        let mut h = EnumHarness::new();
        // A name that is neither an enum literal nor an enum-typed project symbol:
        // `.AsInteger` cannot convert it, so dispatch falls through to the Timer
        // attempt and ultimately fails loud rather than guessing.
        match h.call("No Such Thing", "AsInteger", &[]) {
            Err(EvalError::UnsupportedBuiltin { method, .. }) => {
                assert_eq!(method, "AsInteger");
            }
            other => panic!("expected UnsupportedBuiltin, got {other:?}"),
        }
    }

    #[test]
    fn as_integer_is_classified_supported() {
        // Coverage reports `.AsInteger` supported on any object (resolved at
        // runtime against the enum model, like `Lookup`).
        assert_eq!(
            classify_builtin("Drive State.Idle", "AsInteger"),
            BuiltinSupport::Supported
        );
        assert_eq!(
            classify_builtin("Control.Drive State", "AsInteger"),
            BuiltinSupport::Supported
        );
    }

    #[test]
    fn timer_methods_are_classified_supported() {
        // The project Timer methods are evaluated by `stateful::timer` at runtime;
        // coverage now matches reality and reports them supported.
        for method in ["Start", "Stop", "Reset", "Remaining"] {
            assert_eq!(
                classify_builtin("Startup Delay", method),
                BuiltinSupport::Supported,
                "Timer.{method} should be supported"
            );
        }
    }

    // ---- project-object method routing (P15-C, Tasks 6-7) ----

    /// A harness over the enums fixture that *owns a trace*, so `.Set` channel
    /// writes and externally-driven IO stubs can assert on the recorded columns.
    /// The fixture carries a plain `Precharge State` channel, a `Service Bits`
    /// value-compound, a `DashVals` CAN message (+ `Aux Switch` signal), and a
    /// `Fan Output` package object — the project-object analogues of the EV-M1
    /// constructs Task 6/7 route.
    struct ProjectObjHarness {
        project: Project,
        calib: Calibration,
        env: Env,
        state: StateStore,
        trace: crate::trace::Trace,
    }

    impl ProjectObjHarness {
        fn new() -> ProjectObjHarness {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/enums");
            let loaded =
                crate::loader::load(&dir.join("Project.m1prj"), None).expect("enums fixture loads");
            ProjectObjHarness {
                project: loaded.project,
                calib: Calibration::default(),
                env: Env::new(),
                state: StateStore::new(),
                trace: crate::trace::Trace::new(),
            }
        }

        fn call(&mut self, object: &str, method: &str, args: &[Value]) -> Result<Value, EvalError> {
            let site = CallSite::new("Demo.Update.m1scr", 0);
            let mut ctx = EvalCtx {
                project: &self.project,
                calib: &self.calib,
                env: &mut self.env,
                state: &mut self.state,
                group: Some("Root.Demo"),
                fn_symbol: Some("Root.Demo.Update"),
                script_name: "Demo.Update.m1scr",
                dt: 0.01,
                scripts: &[],
                depth: 0,
                trace: Some(&mut self.trace),
            };
            dispatch(object, method, args, site, &mut ctx)
        }
    }

    // ---- Task 6: Channel .Set(value) imperative setter ----

    #[test]
    fn channel_set_writes_the_channel_and_records_it() {
        let mut h = ProjectObjHarness::new();
        // `Precharge State.Set(1)` writes the channel under its canonical path and
        // records the write to the trace, returning the unit value.
        let result = h
            .call("Precharge State", "Set", &[Value::Int(1)])
            .expect("Channel.Set succeeds");
        assert_eq!(result, Value::Bool(true), "Set returns the unit value");
        // The canonical path now holds the written value.
        assert_eq!(h.env.get("Root.Demo.Precharge State"), Some(&Value::Int(1)));
        // And the write was recorded to the trace.
        assert_eq!(
            h.trace.channels.get("Root.Demo.Precharge State"),
            Some(&vec![Value::Int(1)])
        );
    }

    #[test]
    fn channel_set_via_absolute_path_writes_the_channel() {
        let mut h = ProjectObjHarness::new();
        h.call("Root.Demo.Precharge State", "Set", &[Value::Float(3.5)])
            .expect("Channel.Set on absolute path succeeds");
        assert_eq!(
            h.env.get("Root.Demo.Precharge State"),
            Some(&Value::Float(3.5))
        );
    }

    #[test]
    fn channel_set_wrong_arity_is_bad_call() {
        let mut h = ProjectObjHarness::new();
        // `.Set` is a single-argument setter; zero or many args is a BadCall.
        match h.call("Precharge State", "Set", &[]) {
            Err(EvalError::BadCall { .. }) => {}
            other => panic!("expected BadCall on zero-arg Set, got {other:?}"),
        }
        match h.call("Precharge State", "Set", &[Value::Int(1), Value::Int(2)]) {
            Err(EvalError::BadCall { .. }) => {}
            other => panic!("expected BadCall on two-arg Set, got {other:?}"),
        }
    }

    #[test]
    fn set_is_classified_supported() {
        // Coverage reports `.Set` supported on any object (resolved at runtime to a
        // channel write; the runtime fails loud if the object is not a channel).
        assert_eq!(
            classify_builtin("Precharge State", "Set"),
            BuiltinSupport::Supported
        );
    }

    // ---- Task 7: project-object IO stubs ----

    #[test]
    fn dbc_message_tx_open_returns_opaque_handle_and_is_external() {
        let mut h = ProjectObjHarness::new();
        // A CAN message object's `.TxOpen()` cannot be evaluated offline; it
        // returns a documented opaque handle and is flagged externally driven.
        assert_eq!(h.call("DashVals", "TxOpen", &[]).unwrap(), Value::Uint(0));
        assert!(h.trace.is_external("DashVals.TxOpen"));
    }

    #[test]
    fn dbc_void_writers_return_unit_value() {
        let mut h = ProjectObjHarness::new();
        // The void CAN writers all return the unit value (a no-op offline).
        for method in ["Tx", "TxInitialise", "Init", "SetBit", "SetUnsignedInteger"] {
            assert_eq!(
                h.call("DashVals", method, &[]).unwrap(),
                Value::Bool(true),
                "{method} should return the unit value"
            );
        }
    }

    #[test]
    fn dbc_signal_receive_is_false_offline() {
        let mut h = ProjectObjHarness::new();
        // No CAN message arrives offline, so `.Receive()` is false.
        assert_eq!(
            h.call("DashVals.Aux Switch", "Receive", &[]).unwrap(),
            Value::Bool(false)
        );
        assert!(h.trace.is_external("DashVals.Aux Switch.Receive"));
    }

    #[test]
    fn dbc_signal_get_scaled_is_zero_offline() {
        let mut h = ProjectObjHarness::new();
        // A CAN signal read has no offline value; the documented stub is 0.0 so a
        // whole-project run does not abort on every CAN read.
        assert_eq!(
            h.call("DashVals.Aux Switch", "GetScaled", &[]).unwrap(),
            Value::Float(0.0)
        );
        assert!(h.trace.is_external("DashVals.Aux Switch.GetScaled"));
    }

    #[test]
    fn io_stub_scenario_override_wins() {
        let mut h = ProjectObjHarness::new();
        // A scenario can externally drive a CAN read (e.g. from a log replay).
        h.env
            .set_io_override("DashVals.Aux Switch.GetScaled", Value::Float(42.0));
        assert_eq!(
            h.call("DashVals.Aux Switch", "GetScaled", &[]).unwrap(),
            Value::Float(42.0)
        );
        assert!(h.trace.is_external("DashVals.Aux Switch.GetScaled"));
    }

    #[test]
    fn group_compound_update_is_a_void_stub() {
        let mut h = ProjectObjHarness::new();
        // `Service Bits.Update()` (a GroupCompound CAN service-bits push) is an
        // externally-driven void writer.
        assert_eq!(
            h.call("Service Bits", "Update", &[]).unwrap(),
            Value::Bool(true)
        );
        assert!(h.trace.is_external("Service Bits.Update"));
    }

    #[test]
    fn output_set_state_is_a_void_stub() {
        let mut h = ProjectObjHarness::new();
        // `Fan Output.SetState(...)` (a package Output object) is a void writer.
        assert_eq!(
            h.call("Fan Output", "SetState", &[Value::Bool(true)])
                .unwrap(),
            Value::Bool(true)
        );
        assert!(h.trace.is_external("Fan Output.SetState"));
    }

    #[test]
    fn buzzer_buzze_is_a_void_stub() {
        let mut h = ProjectObjHarness::new();
        // The buzzer's `.Buzze` is an externally-driven void writer (the buzzer is
        // hardware we cannot actuate offline).
        assert_eq!(
            h.call("Fan Output", "Buzze", &[]).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn unknown_object_method_still_fails_loud() {
        let mut h = ProjectObjHarness::new();
        // A project object with a method that is neither a setter, an enum
        // accessor, a Timer method, nor a known IO stub fails loud — never a guess.
        match h.call("Fan Output", "NotAKnownMethod", &[]) {
            Err(EvalError::UnsupportedBuiltin { object, method }) => {
                assert_eq!(object, "Fan Output");
                assert_eq!(method, "NotAKnownMethod");
            }
            other => panic!("expected UnsupportedBuiltin, got {other:?}"),
        }
    }

    #[test]
    fn phase15_calculate_overloads_are_classified_supported() {
        // Every Tier-1 pure Calculate overload P15-A added must report Supported so
        // the coverage report agrees with the dispatch table.
        for method in [
            "Absolute",
            "Average",
            "NAN",
            "Infinity",
            "IsFinite",
            "MaximumFloat",
            "InverseSin",
            "InverseCos",
            "InverseTan",
        ] {
            assert_eq!(
                classify_builtin("Calculate", method),
                BuiltinSupport::Supported,
                "Calculate.{method} should be Supported"
            );
        }
    }

    #[test]
    fn io_stub_methods_are_classified_stubbed() {
        // Each project-object IO method is reported as a documented stub.
        for method in [
            "Tx",
            "TxOpen",
            "TxInitialise",
            "Init",
            "SetBit",
            "SetUnsignedInteger",
            "GetScaled",
            "GetUnsignedInteger",
            "Receive",
            "Update",
            "SetState",
            "Buzze",
        ] {
            assert_eq!(
                classify_builtin("DashVals", method),
                BuiltinSupport::Stubbed,
                "{method} should be a stub"
            );
        }
    }

    #[test]
    fn io_library_methods_are_classified_stubbed() {
        // Every method on a Tier-3 IO *library* object (CanComms/Serial/System/
        // Logging) the generic typed-default stub now handles must classify as
        // Stubbed, so coverage stays consistent with what the IO stub returns at
        // runtime — including the `CanComms.*` reads/setup the old design left
        // unstubbed (the EV-M1 whole-project blocker this fix closed).
        let cases = [
            ("CanComms", "RxOpenStandard"),     // Handle -> unit stub
            ("CanComms", "GetFloat"),           // FloatingPoint -> 0.0
            ("CanComms", "GetUnsignedInteger"), // Integer -> 0
            ("CanComms", "RxMessage"),          // Boolean -> false
            ("CanComms", "SetFloat"),           // Void -> unit
            ("Serial", "GetFloat"),
            ("System", "ElapsedTime"),
            ("System", "TickPeriod"),
            ("Logging", "Running"),
        ];
        for (object, method) in cases {
            assert_eq!(
                classify_builtin(object, method),
                BuiltinSupport::Stubbed,
                "{object}.{method} should be a stub"
            );
        }
    }
}
