// SPDX-License-Identifier: GPL-3.0-or-later
//! Tier-3 IO builtins (`CanComms.*`, `Serial.*`, `System.*`, `Logging.*`) as
//! scenario-fed values or documented stubs.
//!
//! These builtins touch hardware (CAN/serial buses, the firmware clock, the
//! logger). An offline deterministic evaluator cannot truly run them, so each
//! call resolves in this order:
//!
//! 1. **Scenario override.** If the scenario seeded a value for this exact call
//!    (`Env::io_override("Object.Method")`), return it. This is how a scenario or
//!    a log replay externally drives a hardware-backed builtin.
//! 2. **Specific documented stub.** A small set of calls have a *meaningful*
//!    offline value — e.g. `System.TickPeriod()` is the evaluator's tick step
//!    `ctx.dt`, and `System.XcpConnected()`/`Logging.Running()` are false because
//!    no tuning tool or logger is attached offline. The `Void` side-effect calls
//!    (`System.Debug`, `System.AllowTuning`, …) are no-ops returning a benign
//!    value. Each stub's meaning is documented in [`documented_stub`], never
//!    copied from MoTeC.
//! 3. **Generic typed stub.** Every *other* method that the intrinsic registry
//!    lists for the object is a hardware-backed read/write with no meaningful
//!    offline value, but a determinate *type*. Rather than abort a whole-project
//!    run on the first CAN read, we return the type-correct zero/false/empty
//!    default for the overload's declared return type (see [`typed_io_default`]).
//!    This is the externally-driven default a scenario/log replay would override.
//! 4. **Fail loud.** A method the registry does *not* list on the object is
//!    genuinely unknown — we never invent a value for it, so it returns
//!    [`EvalError::UnsupportedBuiltin`].
//!
//! Whenever a value is produced (override or stub), the call is flagged
//! externally driven in the [`Trace`](crate::trace::Trace) so a consumer knows
//! that column is simulated input, not evaluated output.

use crate::error::EvalError;
use crate::expr::EvalCtx;
use crate::value::Value;
use m1_typecheck::intrinsics;

/// Evaluate one Tier-3 IO call. See the module docs for the resolution order.
pub fn call(
    object: &str,
    method: &str,
    args: &[Value],
    ctx: &mut EvalCtx,
) -> Result<Value, EvalError> {
    let key = format!("{object}.{method}");

    // 1. Scenario override wins.
    if let Some(v) = ctx.env.io_override(&key).cloned() {
        mark_external(ctx, &key);
        return Ok(v);
    }

    // 2. Specific documented offline stubs (a *meaningful* offline value).
    if let Some(v) = documented_stub(object, method, args, ctx)? {
        mark_external(ctx, &key);
        return Ok(v);
    }

    // 3. Generic typed stub: any other method the intrinsic registry lists for
    //    this object is a hardware-backed read/write. It has no meaningful offline
    //    value, but a determinate return *type* — so return the type-correct
    //    default rather than abort the run on the first CAN read. The default is
    //    the externally-driven value a scenario / log replay would override.
    if let Some(v) = typed_io_default(object, method) {
        mark_external(ctx, &key);
        return Ok(v);
    }

    // 4. Fail loud — the method is not in the registry for this object, so it is
    //    genuinely unknown. We never fabricate a value for an unknown method.
    Err(EvalError::UnsupportedBuiltin {
        object: object.to_string(),
        method: method.to_string(),
    })
}

/// The type-correct externally-driven default for an IO-library `object.method`,
/// or `None` when the registry lists no such method on the object (so the caller
/// fails loud).
///
/// The value is chosen from the method's declared *return type* in the intrinsic
/// registry — a zero/false/empty of the right kind, never a guessed reading:
///
/// - `Boolean` → `false` (no frame arrived / not connected),
/// - `FloatingPoint` / `FixedPoint7dps` → `0.0`,
/// - `Integer` → `0`,
/// - `UnsignedInteger` → `0`,
/// - `String` → `""`,
/// - `Void` → the benign unit (`Bool(true)`) the void side-effect writers use,
/// - anything else (`Handle`, `Bit`, `Enumeration`, an `Integer|FloatingPoint`
///   union) → the benign unit `Bool(true)`: a transmit/receive handle or a single
///   bus bit has no determinate numeric offline value, so we return the unit
///   rather than invent one.
///
/// All overloads of a method declare the same return type in the IO objects, so
/// the first overload's `returns` fixes the default. An empty overload set means
/// the method is not a real IO method → `None` (fail loud).
fn typed_io_default(object: &str, method: &str) -> Option<Value> {
    let overloads = intrinsics::get().library_overloads(object, method);
    let returns = &overloads.first()?.returns;
    Some(match returns.as_str() {
        "Boolean" => Value::Bool(false),
        "FloatingPoint" | "FixedPoint7dps" => Value::Float(0.0),
        "Integer" => Value::Int(0),
        "UnsignedInteger" => Value::Uint(0),
        "String" => Value::Str(String::new()),
        // `Void` writers and any unmappable return (`Handle`, `Bit`,
        // `Enumeration`, the `Integer|FloatingPoint` union) collapse to the benign
        // unit value, matching the void side-effect convention used elsewhere.
        _ => Value::Bool(true),
    })
}

/// Flag an IO call's channel as externally driven in the trace, if a sink is
/// active.
fn mark_external(ctx: &mut EvalCtx, key: &str) {
    if let Some(trace) = ctx.trace.as_deref_mut() {
        trace.mark_external(key);
    }
}

/// Evaluate one project-object IO call `<object>.<method>(...)`.
///
/// These are the project-object analogue of the Tier-3 library stubs above: a
/// DBC CAN message/signal object (`Balls3EV25.DashVals.Tx/TxOpen/SetBit/…`,
/// `IZZE DBC.*.GetScaled/Receive`), a `GroupCompound` CAN service-bits push
/// (`Service Bits.Update`), a package `Output.SetState`, or a buzzer's `.Buzze`.
/// None of these can be truly evaluated offline — they read from / write to a
/// CAN bus or an output pin we are not driving — so each resolves, like the
/// library stubs, in three steps:
///
/// 1. **Scenario override.** A value seeded under `"<object>.<method>"` (e.g. a
///    log replay driving a `GetScaled`/`Receive`) wins.
/// 2. **Documented stub.** A reader has a determinate offline default
///    (`Receive` → `false`, no message arrived; `GetScaled` → `0.0`;
///    `GetUnsignedInteger` → `0`; `TxOpen` → an opaque handle `0`); a void writer
///    (`Tx`/`TxInitialise`/`Init`/`SetBit`/`SetUnsignedInteger`/`Update`/
///    `SetState`/`Buzze`) returns the unit value (a no-op offline). The stub `0`
///    for reads is deliberate (not fail-loud) so a whole-project run does not
///    abort on every CAN read.
/// 3. **Fail loud.** Any other method on the object has no determinate offline
///    value → [`EvalError::UnsupportedBuiltin`]. We never invent a bus value.
///
/// Every produced value flags `"<object>.<method>"` externally driven in the
/// trace, so a consumer knows the column is simulated input, not evaluated
/// output. Routing is keyed by the *method* name — the object varies per project
/// (`DashVals`, `Service Bits`, `Fan Output`) but the method fixes the offline
/// semantics.
pub fn project_object_call(
    object: &str,
    method: &str,
    _args: &[Value],
    ctx: &mut EvalCtx,
) -> Result<Value, EvalError> {
    let key = format!("{object}.{method}");

    // 1. Scenario override wins (e.g. a log replay driving a CAN read).
    if let Some(v) = ctx.env.io_override(&key).cloned() {
        mark_external(ctx, &key);
        return Ok(v);
    }

    // 2. Documented offline stub, keyed by the method name.
    let v = match method {
        // A CAN message `.TxOpen()` returns an opaque transmit handle; offline it
        // is the determinate zero handle.
        "TxOpen" => Value::Uint(0),
        // A CAN signal `.Receive()` is false offline — no frame has arrived.
        "Receive" => Value::Bool(false),
        // A scaled CAN signal read has no offline value; the documented stub is 0.
        "GetScaled" => Value::Float(0.0),
        // A raw unsigned CAN signal read stubs to 0.
        "GetUnsignedInteger" => Value::Uint(0),
        // Void writers: a CAN transmit / bit set / service-bits push / output set
        // / buzzer actuation is a no-op offline. Return the unit value so an
        // expression statement evaluating the call succeeds.
        "Tx" | "TxInitialise" | "Init" | "SetBit" | "SetUnsignedInteger" | "Update"
        | "SetState" | "Buzze" => Value::Bool(true),
        // Any other method on the object has no determinate offline value.
        _ => {
            return Err(EvalError::UnsupportedBuiltin {
                object: object.to_string(),
                method: method.to_string(),
            });
        }
    };
    mark_external(ctx, &key);
    Ok(v)
}

/// The project-object IO methods handled as documented offline stubs (flagged
/// externally driven). The single source of truth the coverage classifier
/// consults so it agrees with [`project_object_call`].
pub const PROJECT_OBJECT_STUB_METHODS: &[&str] = &[
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
];

/// The documented offline value for a Tier-3 call, or `Ok(None)` when there is no
/// determinate stub (so the caller fails loud). Each stub is a paraphrased,
/// defensible offline interpretation — not a guessed sensor reading.
fn documented_stub(
    object: &str,
    method: &str,
    _args: &[Value],
    ctx: &mut EvalCtx,
) -> Result<Option<Value>, EvalError> {
    let v = match (object, method) {
        // The scheduler tick period is exactly the evaluator's tick step.
        ("System", "TickPeriod") => Value::Float(ctx.dt),
        // No tuning tool (XCP) is connected during offline evaluation.
        ("System", "XcpConnected") => Value::Bool(false),
        // Void side-effects: no observable result offline. Return a benign value
        // so an expression statement evaluating the call succeeds.
        ("System", "AllowTuning")
        | ("System", "Debug")
        | ("System", "TimedDebug")
        | ("System", "Unused")
        | ("System", "Preserve") => Value::Bool(true),
        // No data logger is running / unloading in offline evaluation. Only the
        // zero-argument overloads have a determinate stub; the per-system
        // overloads (one Integer arg) fail loud.
        ("Logging", "Running") if _args.is_empty() => Value::Bool(false),
        ("Logging", "Unloading") if _args.is_empty() => Value::Bool(false),
        // Everything else has no determinate offline value.
        _ => return Ok(None),
    };
    Ok(Some(v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calib::Calibration;
    use crate::env::{CallSite, Env, StateStore};
    use crate::trace::Trace;
    use m1_typecheck::Project;
    use std::path::Path;

    fn mini_project() -> Project {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini");
        crate::loader::load(&dir.join("Project.m1prj"), None)
            .expect("mini fixture loads")
            .project
    }

    struct Harness {
        project: Project,
        calib: Calibration,
        env: Env,
        state: StateStore,
        trace: Trace,
    }

    impl Harness {
        fn new() -> Harness {
            Harness {
                project: mini_project(),
                calib: Calibration::default(),
                env: Env::new(),
                state: StateStore::new(),
                trace: Trace::new(),
            }
        }

        /// Dispatch an IO call through the public `builtins::dispatch` so the
        /// routing (object recognition + this stub module) is exercised end to
        /// end, with the trace sink attached.
        fn io(&mut self, object: &str, method: &str, args: &[Value]) -> Result<Value, EvalError> {
            let site = CallSite::new("Demo.Update.m1scr", 0);
            let mut ctx = EvalCtx {
                project: &self.project,
                calib: &self.calib,
                env: &mut self.env,
                state: &mut self.state,
                group: Some("Root.Demo"),
                fn_symbol: Some("Root.Demo.Update"),
                script_name: "Demo.Update.m1scr",
                dt: 0.02,
                scripts: &[],
                depth: 0,
                trace: Some(&mut self.trace),
            };
            crate::builtins::dispatch(object, method, args, site, &mut ctx)
        }
    }

    #[test]
    fn system_tick_period_is_dt() {
        let mut h = Harness::new();
        assert_eq!(h.io("System", "TickPeriod", &[]).unwrap(), Value::Float(0.02));
        // The call is flagged externally driven.
        assert!(h.trace.is_external("System.TickPeriod"));
    }

    #[test]
    fn system_xcp_connected_is_false_offline() {
        let mut h = Harness::new();
        assert_eq!(h.io("System", "XcpConnected", &[]).unwrap(), Value::Bool(false));
    }

    #[test]
    fn system_void_side_effects_are_noops() {
        let mut h = Harness::new();
        assert_eq!(
            h.io("System", "Debug", &[Value::Str("hello".into())]).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            h.io("System", "AllowTuning", &[Value::Bool(true)]).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn logging_running_is_false_offline() {
        let mut h = Harness::new();
        assert_eq!(h.io("Logging", "Running", &[]).unwrap(), Value::Bool(false));
        assert_eq!(h.io("Logging", "Unloading", &[]).unwrap(), Value::Bool(false));
    }

    #[test]
    fn scenario_override_wins_over_stub_and_failure() {
        let mut h = Harness::new();
        // Seed a scenario value for a CAN read that would otherwise fail loud.
        h.env.set_io_override("CanComms.GetFloat", Value::Float(12.5));
        assert_eq!(
            h.io("CanComms", "GetFloat", &[Value::Uint(0), Value::Int(0)]).unwrap(),
            Value::Float(12.5)
        );
        assert!(h.trace.is_external("CanComms.GetFloat"));
    }

    #[test]
    fn can_read_returns_typed_external_stub() {
        let mut h = Harness::new();
        // No scenario value, no specific stub: a real CAN read now returns the
        // type-correct externally-driven default (never a guessed reading).
        // `CanComms.GetFloat` declares a `FloatingPoint` return, so the stub is 0.0.
        assert_eq!(
            h.io("CanComms", "GetFloat", &[Value::Uint(0), Value::Int(0)]).unwrap(),
            Value::Float(0.0)
        );
        // The stub is flagged externally driven.
        assert!(h.trace.is_external("CanComms.GetFloat"));
    }

    #[test]
    fn can_open_handle_returns_unit_external_stub() {
        let mut h = Harness::new();
        // `CanComms.RxOpenStandard` declares a `Handle` return — an opaque
        // receive handle with no determinate offline value — so the typed stub is
        // the benign unit (`Bool(true)`), externally driven, not a fail-loud abort.
        assert_eq!(
            h.io(
                "CanComms",
                "RxOpenStandard",
                &[Value::Uint(0), Value::Uint(0), Value::Uint(0), Value::Uint(0)],
            )
            .unwrap(),
            Value::Bool(true)
        );
        assert!(h.trace.is_external("CanComms.RxOpenStandard"));
    }

    #[test]
    fn can_void_writer_returns_unit_external_stub() {
        let mut h = Harness::new();
        // `CanComms.SetFloat` declares a `Void` return — a bus write that is a
        // no-op offline — so the typed stub is the benign unit value.
        assert_eq!(
            h.io(
                "CanComms",
                "SetFloat",
                &[Value::Uint(0), Value::Int(0), Value::Float(1.0)],
            )
            .unwrap(),
            Value::Bool(true)
        );
        assert!(h.trace.is_external("CanComms.SetFloat"));
    }

    #[test]
    fn system_call_returns_typed_external_stub() {
        let mut h = Harness::new();
        // `System.ElapsedTime` has no *meaningful* offline value, but the registry
        // lists it with a `FloatingPoint` return, so the typed stub is 0.0 (an
        // externally-driven default), not a fail-loud abort.
        assert_eq!(h.io("System", "ElapsedTime", &[]).unwrap(), Value::Float(0.0));
        assert!(h.trace.is_external("System.ElapsedTime"));
    }

    #[test]
    fn per_system_logging_overload_returns_typed_external_stub() {
        let mut h = Harness::new();
        // `Logging.Running(system)` (one Integer arg) has no *specific* stub (only
        // the zero-arg overload does), but it is a real `Boolean` IO method, so the
        // generic typed stub is false (externally driven), not a fail-loud abort.
        assert_eq!(
            h.io("Logging", "Running", &[Value::Int(0)]).unwrap(),
            Value::Bool(false)
        );
        assert!(h.trace.is_external("Logging.Running"));
    }

    #[test]
    fn unknown_io_method_fails_loud() {
        let mut h = Harness::new();
        // A method the registry does NOT list on the object is genuinely unknown:
        // we never invent a value for it, so it fails loud.
        match h.io("CanComms", "NotARealMethod", &[]) {
            Err(EvalError::UnsupportedBuiltin { object, method }) => {
                assert_eq!(object, "CanComms");
                assert_eq!(method, "NotARealMethod");
            }
            other => panic!("expected UnsupportedBuiltin, got {other:?}"),
        }
        // A failed call is not marked external.
        assert!(!h.trace.is_external("CanComms.NotARealMethod"));
    }

    #[test]
    fn unknown_io_object_method_fails_loud() {
        let mut h = Harness::new();
        // `Serial` is an IO object, but `Frobnicate` is not a method the registry
        // lists for it — genuinely unknown, so fail loud rather than fabricate.
        match h.io("Serial", "Frobnicate", &[]) {
            Err(EvalError::UnsupportedBuiltin { object, method }) => {
                assert_eq!(object, "Serial");
                assert_eq!(method, "Frobnicate");
            }
            other => panic!("expected UnsupportedBuiltin, got {other:?}"),
        }
    }

    #[test]
    fn typed_io_default_maps_each_return_kind() {
        // The generic fallback maps each registry return type to its zero/unit.
        // `GetFloat` → FloatingPoint, `GetID` → Integer, `BusRxTotal` →
        // UnsignedInteger, `RxMessage` → Boolean, `SetFloat` → Void,
        // `RxOpenStandard` → Handle (unmappable → unit).
        assert_eq!(typed_io_default("CanComms", "GetFloat"), Some(Value::Float(0.0)));
        assert_eq!(typed_io_default("CanComms", "GetID"), Some(Value::Int(0)));
        assert_eq!(
            typed_io_default("CanComms", "BusRxTotal"),
            Some(Value::Uint(0))
        );
        assert_eq!(typed_io_default("CanComms", "RxMessage"), Some(Value::Bool(false)));
        assert_eq!(typed_io_default("CanComms", "SetFloat"), Some(Value::Bool(true)));
        assert_eq!(
            typed_io_default("CanComms", "RxOpenStandard"),
            Some(Value::Bool(true))
        );
        // A method not in the registry → None (the caller fails loud).
        assert_eq!(typed_io_default("CanComms", "NotARealMethod"), None);
    }
}
