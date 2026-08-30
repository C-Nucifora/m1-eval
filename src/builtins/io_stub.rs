// SPDX-License-Identifier: GPL-3.0-or-later
//! Hardware-call routing for library and project receivers.
//!
//! These builtins touch hardware (CAN/serial buses, the firmware clock, the
//! logger). Each call resolves in this order:
//!
//! 1. An exact-call-site scenario override.
//! 2. A wildcard scenario override for the call name.
//! 3. An external [`HardwareAdapter`](crate::hardware::HardwareAdapter).
//! 4. The deterministic `System` clock/tick model.
//! 5. A generic typed stub. Every other method that the intrinsic registry
//!    lists for the object is a hardware-backed read/write with no meaningful
//!    offline value, but a determinate *type*. Rather than abort a whole-project
//!    run on the first CAN read, we return the type-correct zero/false/empty
//!    default for the overload's declared return type (see `typed_io_default`).
//!    This is the externally-driven default a scenario/log replay would override.
//! 6. Fail loud. A method the registry does not list on the object is
//!    genuinely unknown — we never invent a value for it, so it returns
//!    [`EvalError::UnsupportedBuiltin`].
//!
//! `System.FlashSize` and `System.FlashFree` deliberately skip the zero fallback.
//! A scenario or adapter must supply them, otherwise evaluation returns an
//! actionable [`EvalError::MissingHardwareMetadata`]. Every successful route is
//! recorded as structured provenance in the [`Trace`](crate::trace::Trace).

use crate::env::{CallSite, OpState};
use crate::error::EvalError;
use crate::expr::{EvalCtx, coerce_for_declared_type, coerce_for_scalar_kind};
use crate::hardware::{
    AdapterReply, HardwareCall, HardwareProvenance, HardwareValueSource, ResolvedReceiver,
};
use crate::value::{FixedPoint7dps, M1Scalar, M1ScalarKind, Value};
use m1_typecheck::intrinsics;
use m1_typecheck::types::ValueType;

/// Evaluate one hardware-library call. `library_object` is the canonical
/// intrinsic catalog object. `source_object` keeps the exact spelling used for
/// compatibility scenario keys, trace keys, and errors. See the module docs for
/// the routing order.
pub fn call(
    library_object: &str,
    source_object: &str,
    method: &str,
    args: &[Value],
    site: CallSite,
    ctx: &mut EvalCtx,
) -> Result<Value, EvalError> {
    let call = HardwareCall {
        receiver: ResolvedReceiver::Library {
            object: library_object.to_string(),
        },
        source_receiver: source_object.to_string(),
        method: method.to_string(),
        site,
        arguments: args.to_vec(),
        time: ctx.time,
    };
    let returns = library_return_type(library_object, method);

    if let Some((value, source)) = scenario_override(ctx, &call) {
        let value = coerce_hardware_value(value, returns, &call, ctx)?;
        return complete(ctx, &call, value, source);
    }

    if let Some(hardware) = ctx.hardware.as_deref_mut()
        && let AdapterReply::Value(value) = hardware.call(&call)?
    {
        let value = coerce_hardware_value(value, returns, &call, ctx)?;
        return complete(ctx, &call, value, HardwareValueSource::Adapter);
    }

    if let Some(value) = system_model(library_object, method, args, &call.site, ctx)? {
        return complete(ctx, &call, value, HardwareValueSource::SystemModel);
    }

    if required_metadata(library_object, method) {
        return Err(EvalError::MissingHardwareMetadata {
            call: call.canonical_name(),
        });
    }

    if let Some(value) = documented_stub(library_object, method, args) {
        return complete(ctx, &call, value, HardwareValueSource::GenericStub);
    }

    if let Some(value) = typed_io_default(library_object, method) {
        return complete(ctx, &call, value, HardwareValueSource::GenericStub);
    }

    Err(EvalError::UnsupportedBuiltin {
        object: source_object.to_string(),
        method: method.to_string(),
    })
}

fn library_return_type(object: &str, method: &str) -> Option<&'static str> {
    intrinsics::get()
        .library_overloads(object, method)
        .first()
        .map(|overload| overload.returns.as_str())
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
        "FloatingPoint" => Value::m1_float(0.0),
        "FixedPoint7dps" => Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::ZERO)),
        "Integer" => Value::m1_integer(0),
        "UnsignedInteger" => Value::m1_unsigned(0),
        "String" => Value::Str(String::new()),
        // `Void` writers and any unmappable return (`Handle`, `Bit`,
        // `Enumeration`, the `Integer|FloatingPoint` union) collapse to the benign
        // unit value, matching the void side-effect convention used elsewhere.
        _ => Value::Bool(true),
    })
}

/// Restore a scenario or adapter value to the method's declared return family.
fn coerce_hardware_value(
    value: Value,
    returns: Option<&str>,
    call: &HardwareCall,
    ctx: &EvalCtx,
) -> Result<Value, EvalError> {
    let key = call.canonical_name();
    match returns {
        Some("Integer") => coerce_for_scalar_kind(&key, value, M1ScalarKind::Integer),
        Some("UnsignedInteger") => {
            coerce_for_scalar_kind(&key, value, M1ScalarKind::UnsignedInteger)
        }
        Some("FloatingPoint") => coerce_for_scalar_kind(&key, value, M1ScalarKind::FloatingPoint),
        Some("FixedPoint7dps") => coerce_for_scalar_kind(&key, value, M1ScalarKind::FixedPoint7dps),
        Some("Boolean") => coerce_for_declared_type(&key, value, ValueType::Boolean, ctx.project),
        Some("String") => coerce_for_declared_type(&key, value, ValueType::String, ctx.project),
        // Void and opaque handles do not have an M1 scalar family to restore.
        _ => Ok(value),
    }
}

/// Find an exact-site scenario override before either canonical or
/// source-spelled wildcard keys.
fn scenario_override(ctx: &EvalCtx, call: &HardwareCall) -> Option<(Value, HardwareValueSource)> {
    let canonical = call.canonical_name();
    let source = call.source_name();
    let keys = if canonical == source {
        vec![canonical]
    } else {
        vec![canonical, source]
    };

    for key in &keys {
        if let Some(value) = ctx.env.io_override_at(key, &call.site) {
            return Some((value.clone(), HardwareValueSource::ScenarioExact));
        }
    }
    for key in &keys {
        if let Some(value) = ctx.env.io_override(key) {
            return Some((value.clone(), HardwareValueSource::ScenarioWildcard));
        }
    }
    None
}

/// Record the selected route, flag external values, and return the result.
fn complete(
    ctx: &mut EvalCtx,
    call: &HardwareCall,
    value: Value,
    source: HardwareValueSource,
) -> Result<Value, EvalError> {
    if let Some(trace) = ctx.trace.as_deref_mut() {
        if source.is_external() {
            trace.mark_external(call.source_name());
        }
        trace.record_hardware(HardwareProvenance::new(call, source));
    }
    Ok(value)
}

/// Evaluate one project-object IO call `<object>.<method>(...)`.
///
/// These are the project-object analogue of the Tier-3 library stubs above: a
/// DBC CAN message/signal object (`Balls3EV25.DashVals.Tx/TxOpen/SetBit/…`,
/// `IZZE DBC.*.GetScaled/Receive`), a `GroupCompound` CAN service-bits push
/// (`Service Bits.Update`), a package `Output.SetState`, or a buzzer's `.Buzze`.
/// None of these can be evaluated from project data alone: they read from or
/// write to a CAN bus or output pin. Each call uses the shared hardware routing
/// order:
///
/// 1. **Exact-site scenario override.** A value for this script and byte offset
///    wins over every other route.
/// 2. **Wildcard scenario override.** A value seeded under
///    `"<object>.<method>"` drives every matching call site.
/// 3. **Adapter.** An attached [`HardwareAdapter`](crate::HardwareAdapter) sees
///    the resolved receiver, call site, arguments, and evaluator time.
/// 4. **Documented stub.** A reader has a determinate offline default
///    (`Receive` → `false`, no message arrived; `GetScaled` → `0.0`;
///    `GetUnsignedInteger` → `0`; `TxOpen` → an opaque handle `0`); a void writer
///    (`Tx`/`TxInitialise`/`Init`/`SetBit`/`SetUnsignedInteger`/`Update`/
///    `SetState`/`Buzze`) returns the unit value (a no-op offline). The stub `0`
///    for reads is deliberate (not fail-loud) so a whole-project run does not
///    abort on every CAN read.
/// 5. **Fail loud.** Any other method on the object has no determinate offline
///    value → [`EvalError::UnsupportedBuiltin`]. We never invent a bus value.
///
/// Every produced value flags `"<object>.<method>"` externally driven in the
/// trace, so a consumer knows the value came from outside evaluator computation.
/// Structured provenance retains the resolved receiver and selected route.
pub fn project_object_call(
    receiver: ResolvedReceiver,
    source_object: &str,
    method: &str,
    args: &[Value],
    site: CallSite,
    ctx: &mut EvalCtx,
) -> Result<Value, EvalError> {
    let call = HardwareCall {
        receiver,
        source_receiver: source_object.to_string(),
        method: method.to_string(),
        site,
        arguments: args.to_vec(),
        time: ctx.time,
    };
    let returns = project_return_type(method);

    if let Some((value, source)) = scenario_override(ctx, &call) {
        let value = coerce_hardware_value(value, returns, &call, ctx)?;
        return complete(ctx, &call, value, source);
    }

    if let Some(hardware) = ctx.hardware.as_deref_mut()
        && let AdapterReply::Value(value) = hardware.call(&call)?
    {
        let value = coerce_hardware_value(value, returns, &call, ctx)?;
        return complete(ctx, &call, value, HardwareValueSource::Adapter);
    }

    if let Some(value) = project_stub(method) {
        return complete(ctx, &call, value, HardwareValueSource::GenericStub);
    }

    Err(EvalError::UnsupportedBuiltin {
        object: source_object.to_string(),
        method: method.to_string(),
    })
}

fn project_return_type(method: &str) -> Option<&'static str> {
    match method {
        "TxOpen" | "GetUnsignedInteger" => Some("UnsignedInteger"),
        "GetInteger" => Some("Integer"),
        "GetScaled" | "GetFloat" => Some("FloatingPoint"),
        "Receive" | "GetBit" => Some("Boolean"),
        _ => None,
    }
}

/// Documented offline fallback for a recognized project-object method.
fn project_stub(method: &str) -> Option<Value> {
    Some(match method {
        // A CAN message `.TxOpen()` returns an opaque transmit handle; offline it
        // is the determinate zero handle.
        "TxOpen" => Value::m1_unsigned(0),
        // A CAN signal `.Receive()` is false offline — no frame has arrived.
        "Receive" => Value::Bool(false),
        // A scaled or floating-point CAN signal read has no offline value; the
        // documented stub is 0.
        "GetScaled" | "GetFloat" => Value::m1_float(0.0),
        // A raw unsigned CAN signal read stubs to 0.
        "GetUnsignedInteger" => Value::m1_unsigned(0),
        // A raw signed CAN signal read stubs to 0.
        "GetInteger" => Value::m1_integer(0),
        // A single bus bit read is false offline — no frame has set it.
        "GetBit" => Value::Bool(false),
        // Void writers: a CAN transmit / bit set / service-bits push / output set
        // / buzzer actuation is a no-op offline. Return the unit value so an
        // expression statement evaluating the call succeeds.
        // `Set` here is the package/reference output-drive write (`ASSI
        // Yellow.Drive.Set(...)` on a Reference with an auto-channel target); a
        // real channel `.Set` never reaches this route — `try_channel_set`
        // claims it first.
        "Tx" | "TxInitialise" | "Init" | "SetBit" | "SetUnsignedInteger" | "SetInteger"
        | "SetScaled" | "SetFloat" | "SetFromBaseUnit" | "Set" | "Update" | "SetState"
        | "Buzze" => Value::Bool(true),
        // Any other method on the object has no determinate offline value.
        _ => return None,
    })
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
    "SetInteger",
    "SetScaled",
    "SetFloat",
    "SetFromBaseUnit",
    "Set",
    "GetScaled",
    "GetFloat",
    "GetUnsignedInteger",
    "GetInteger",
    "GetBit",
    "Receive",
    "Update",
    "SetState",
    "Buzze",
];

/// Deterministic clock and tick behavior derived only from [`EvalCtx::time`].
fn system_model(
    object: &str,
    method: &str,
    args: &[Value],
    site: &CallSite,
    ctx: &mut EvalCtx,
) -> Result<Option<Value>, EvalError> {
    if object != "System" {
        return Ok(None);
    }
    let now = ctx.time.base_tick as u32;
    let v = match (object, method) {
        // The catalogue defines this relative to the line which calls it, so
        // each occurrence owns an epoch instead of inheriting run-global time.
        ("System", "ElapsedTime") => elapsed_time(site, ctx),
        ("System", "TickPeriod") | ("System", "HiResTickPeriod") => {
            Value::m1_float(ctx.time.base_period_s as f32)
        }
        ("System", "Ticks") | ("System", "HiResTicks") => Value::m1_unsigned(now),
        ("System", "TicksSince") | ("System", "HiResTicksSince") => {
            Value::m1_unsigned(now.wrapping_sub(unsigned_arg(args, 0, method)?))
        }
        ("System", "TicksBetween") => Value::m1_unsigned(
            unsigned_arg(args, 1, method)?.wrapping_sub(unsigned_arg(args, 0, method)?),
        ),
        ("System", "TicksRemaining") => Value::m1_unsigned(
            unsigned_arg(args, 1, method)?.wrapping_sub(unsigned_arg(args, 0, method)?),
        ),
        _ => return Ok(None),
    };
    Ok(Some(v))
}

/// Seconds since this exact `System.ElapsedTime` occurrence first executed.
fn elapsed_time(site: &CallSite, ctx: &mut EvalCtx) -> Value {
    let now = ctx.time.elapsed_s;
    let slot = ctx.state.entry(site.clone());
    let first_execution_s = match slot {
        OpState::SystemElapsed { first_execution_s } => *first_execution_s,
        state => {
            *state = OpState::SystemElapsed {
                first_execution_s: now,
            };
            now
        }
    };
    Value::m1_float((now - first_execution_s) as f32)
}

fn unsigned_arg(args: &[Value], index: usize, method: &str) -> Result<u32, EvalError> {
    let value = args.get(index).ok_or_else(|| EvalError::BadCall {
        detail: format!("System.{method} is missing argument {}", index + 1),
    })?;
    match coerce_for_scalar_kind(
        &format!("System.{method} argument {}", index + 1),
        value.clone(),
        M1ScalarKind::UnsignedInteger,
    )? {
        Value::M1(M1Scalar::UnsignedInteger(value)) => Ok(value),
        _ => unreachable!("unsigned coercion returns an unsigned M1 scalar"),
    }
}

/// Metadata whose zero value would be unsafe to mistake for a real ECU fact.
pub(crate) fn required_metadata(object: &str, method: &str) -> bool {
    object == "System" && matches!(method, "FlashSize" | "FlashFree")
}

/// A documented offline fallback which carries a definite non-hardware meaning.
fn documented_stub(object: &str, method: &str, args: &[Value]) -> Option<Value> {
    let v = match (object, method) {
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
        // zero-argument overloads have this specific meaning; per-system
        // overloads continue to the generic typed fallback.
        ("Logging", "Running") if args.is_empty() => Value::Bool(false),
        ("Logging", "Unloading") if args.is_empty() => Value::Bool(false),
        // Everything else has no determinate offline value.
        _ => return None,
    };
    Some(v)
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
            self.io_at(
                object,
                method,
                args,
                CallSite::new("Demo.Update.m1scr", 0),
                crate::hardware::EvalTime::at_start(0.02),
                None,
            )
        }

        fn io_at(
            &mut self,
            object: &str,
            method: &str,
            args: &[Value],
            site: CallSite,
            time: crate::hardware::EvalTime,
            hardware: Option<&mut dyn crate::hardware::HardwareAdapter>,
        ) -> Result<Value, EvalError> {
            let mut ctx = EvalCtx {
                project: &self.project,
                calib: &self.calib,
                env: &mut self.env,
                state: &mut self.state,
                group: Some("Root.Demo"),
                fn_symbol: Some("Root.Demo.Update"),
                script_name: "Demo.Update.m1scr",
                time,
                hardware,
                scripts: &[],
                signature_m1_types: None,
                object_rules: None,
                depth: 0,
                trace: Some(&mut self.trace),
            };
            crate::builtins::dispatch(object, method, args, site, &mut ctx)
        }
    }

    #[test]
    fn system_tick_period_uses_the_base_grid_not_the_function_step() {
        let mut h = Harness::new();
        let time = crate::hardware::EvalTime::periodic(4, 0.04, 0.01, 0.02);
        assert_eq!(
            h.io_at(
                "System",
                "TickPeriod",
                &[],
                CallSite::new("Demo.Update.m1scr", 10),
                time,
                None,
            )
            .unwrap(),
            Value::m1_float(0.01)
        );
        assert!(!h.trace.is_external("System.TickPeriod"));
        assert!(h.trace.hardware.iter().any(|record| {
            record.source == HardwareValueSource::SystemModel
                && record.canonical_call() == "System.TickPeriod"
        }));
    }

    #[test]
    fn system_xcp_connected_is_false_offline() {
        let mut h = Harness::new();
        assert_eq!(
            h.io("System", "XcpConnected", &[]).unwrap(),
            Value::Bool(false)
        );
    }

    #[test]
    fn system_void_side_effects_are_noops() {
        let mut h = Harness::new();
        assert_eq!(
            h.io("System", "Debug", &[Value::Str("hello".into())])
                .unwrap(),
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
        assert_eq!(
            h.io("Logging", "Unloading", &[]).unwrap(),
            Value::Bool(false)
        );
    }

    #[test]
    fn scenario_override_wins_over_stub_and_failure() {
        let mut h = Harness::new();
        // Seed a scenario value for a CAN read that would otherwise fail loud.
        h.env
            .set_io_override("CanComms.GetFloat", Value::m1_float(12.5));
        assert_eq!(
            h.io(
                "CanComms",
                "GetFloat",
                &[Value::m1_unsigned(0), Value::m1_integer(0)]
            )
            .unwrap(),
            Value::m1_float(12.5)
        );
        assert!(h.trace.is_external("CanComms.GetFloat"));
    }

    #[test]
    fn wildcard_scenario_keys_do_not_collide_across_receivers() {
        let mut h = Harness::new();
        h.env
            .set_io_override("CanComms.GetFloat", Value::m1_float(12.5));
        h.env
            .set_io_override("Serial.GetFloat", Value::m1_float(99.0));
        let args = [Value::m1_unsigned(0), Value::m1_integer(0)];

        assert_eq!(
            h.io("CanComms", "GetFloat", &args).unwrap(),
            Value::m1_float(12.5)
        );
        assert_eq!(
            h.io("Serial", "GetFloat", &args).unwrap(),
            Value::m1_float(99.0)
        );
    }

    #[test]
    fn library_anchor_keeps_source_spelling_for_io_override_and_trace() {
        let mut h = Harness::new();
        h.env
            .set_io_override("Library.CanComms.GetFloat", Value::m1_float(12.5));

        assert_eq!(
            h.io(
                "Library.CanComms",
                "GetFloat",
                &[Value::m1_unsigned(0), Value::m1_integer(0)]
            )
            .unwrap(),
            Value::m1_float(12.5)
        );
        assert!(h.trace.is_external("Library.CanComms.GetFloat"));
        assert!(!h.trace.is_external("CanComms.GetFloat"));
    }

    #[test]
    fn scenario_overrides_restore_declared_io_return_families() {
        let mut h = Harness::new();
        h.env
            .set_io_override("CanComms.GetFloat", Value::m1_integer(5));
        assert_eq!(
            h.io(
                "CanComms",
                "GetFloat",
                &[Value::m1_unsigned(0), Value::m1_integer(0)]
            )
            .unwrap(),
            Value::m1_float(5.0)
        );

        h.env
            .set_io_override("System.FlashSize", Value::m1_integer(-1));
        assert_eq!(
            h.io("System", "FlashSize", &[]).unwrap(),
            Value::m1_unsigned(u32::MAX)
        );

        h.env.set_io_override(
            "DashVals.Aux Switch.GetUnsignedInteger",
            Value::m1_integer(-2),
        );
        assert_eq!(
            h.io("DashVals.Aux Switch", "GetUnsignedInteger", &[])
                .unwrap(),
            Value::m1_unsigned(u32::MAX - 1)
        );
    }

    #[test]
    fn can_read_returns_typed_external_stub() {
        let mut h = Harness::new();
        // No scenario value, no specific stub: a real CAN read now returns the
        // type-correct externally-driven default (never a guessed reading).
        // `CanComms.GetFloat` declares a `FloatingPoint` return, so the stub is 0.0.
        assert_eq!(
            h.io(
                "CanComms",
                "GetFloat",
                &[Value::m1_unsigned(0), Value::m1_integer(0)]
            )
            .unwrap(),
            Value::m1_float(0.0)
        );
        // The stub is flagged externally driven.
        assert!(h.trace.is_external("CanComms.GetFloat"));
    }

    #[test]
    fn fixed_point_io_stub_preserves_its_declared_family() {
        let mut h = Harness::new();
        assert_eq!(
            h.io(
                "CanComms",
                "GetFixed7DP",
                &[Value::m1_unsigned(0), Value::m1_integer(0)]
            )
            .unwrap(),
            Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::ZERO))
        );
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
                &[
                    Value::m1_unsigned(0),
                    Value::m1_unsigned(0),
                    Value::m1_unsigned(0),
                    Value::m1_unsigned(0)
                ],
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
                &[
                    Value::m1_unsigned(0),
                    Value::m1_integer(0),
                    Value::m1_float(1.0)
                ],
            )
            .unwrap(),
            Value::Bool(true)
        );
        assert!(h.trace.is_external("CanComms.SetFloat"));
    }

    #[test]
    fn system_elapsed_time_and_ticks_follow_the_evaluator_timeline() {
        let mut h = Harness::new();
        let time = crate::hardware::EvalTime::periodic(125, 1.25, 0.01, 0.05);
        assert_eq!(
            h.io_at(
                "System",
                "ElapsedTime",
                &[],
                CallSite::new("Demo.Update.m1scr", 20),
                time,
                None,
            )
            .unwrap(),
            Value::m1_float(0.0)
        );
        assert_eq!(
            h.io_at(
                "System",
                "Ticks",
                &[],
                CallSite::new("Demo.Update.m1scr", 30),
                time,
                None,
            )
            .unwrap(),
            Value::m1_unsigned(125)
        );
        assert_eq!(
            h.io_at(
                "System",
                "TicksSince",
                &[Value::m1_unsigned(120)],
                CallSite::new("Demo.Update.m1scr", 40),
                time,
                None,
            )
            .unwrap(),
            Value::m1_unsigned(5)
        );
        assert_eq!(
            h.io_at(
                "System",
                "TicksBetween",
                &[Value::m1_unsigned(u32::MAX - 1), Value::m1_unsigned(1)],
                CallSite::new("Demo.Update.m1scr", 50),
                time,
                None,
            )
            .unwrap(),
            Value::m1_unsigned(3),
            "tick differences retain unsigned wraparound"
        );
        assert!(!h.trace.is_external("System.ElapsedTime"));
    }

    #[test]
    fn system_elapsed_time_starts_when_the_site_first_executes() {
        let mut h = Harness::new();
        let site = CallSite::new("Demo.Update.m1scr", 20);
        assert_eq!(
            h.io_at(
                "System",
                "ElapsedTime",
                &[],
                site.clone(),
                crate::hardware::EvalTime::periodic(750, 7.5, 0.01, 0.01),
                None,
            )
            .unwrap(),
            Value::m1_float(0.0),
            "a late first execution establishes its own zero"
        );
        assert_eq!(
            h.io_at(
                "System",
                "ElapsedTime",
                &[],
                site,
                crate::hardware::EvalTime::periodic(800, 8.0, 0.01, 0.01),
                None,
            )
            .unwrap(),
            Value::m1_float(0.5)
        );
    }

    #[test]
    fn system_elapsed_time_sites_keep_independent_epochs() {
        let mut h = Harness::new();
        let first = CallSite::new("Demo.Update.m1scr", 20);
        let second = CallSite::new("Demo.Update.m1scr", 40);

        assert_eq!(
            h.io_at(
                "System",
                "ElapsedTime",
                &[],
                first.clone(),
                crate::hardware::EvalTime::periodic(100, 1.0, 0.01, 0.01),
                None,
            )
            .unwrap(),
            Value::m1_float(0.0)
        );
        assert_eq!(
            h.io_at(
                "System",
                "ElapsedTime",
                &[],
                second.clone(),
                crate::hardware::EvalTime::periodic(250, 2.5, 0.01, 0.01),
                None,
            )
            .unwrap(),
            Value::m1_float(0.0)
        );
        let same_time = crate::hardware::EvalTime::periodic(300, 3.0, 0.01, 0.01);
        assert_eq!(
            h.io_at("System", "ElapsedTime", &[], first, same_time, None,)
                .unwrap(),
            Value::m1_float(2.0)
        );
        assert_eq!(
            h.io_at("System", "ElapsedTime", &[], second, same_time, None,)
                .unwrap(),
            Value::m1_float(0.5)
        );
    }

    #[test]
    fn flash_metadata_fails_loud_without_scenario_or_adapter_data() {
        let mut h = Harness::new();
        for method in ["FlashSize", "FlashFree"] {
            let error = h.io("System", method, &[]).unwrap_err();
            assert_eq!(
                error,
                EvalError::MissingHardwareMetadata {
                    call: format!("System.{method}")
                }
            );
            let message = error.to_string();
            assert!(message.contains("[[io]]"), "{message}");
            assert!(message.contains("HardwareAdapter"), "{message}");
        }
    }

    #[derive(Debug)]
    struct RecordingAdapter {
        reply: AdapterReply,
        calls: Vec<HardwareCall>,
    }

    impl crate::hardware::HardwareAdapter for RecordingAdapter {
        fn call(&mut self, call: &HardwareCall) -> Result<AdapterReply, EvalError> {
            self.calls.push(call.clone());
            Ok(self.reply.clone())
        }
    }

    #[test]
    fn adapter_receives_resolved_call_site_arguments_and_time() {
        let mut h = Harness::new();
        let site = CallSite::new("Demo.Update.m1scr", 77);
        let time = crate::hardware::EvalTime::periodic(9, 0.09, 0.01, 0.02);
        let args = [Value::m1_unsigned(4), Value::m1_integer(8)];
        let mut adapter = RecordingAdapter {
            // The registry return family is FloatingPoint, so the evaluator must
            // restore this integer adapter reply to binary32.
            reply: AdapterReply::Value(Value::m1_integer(12)),
            calls: Vec::new(),
        };

        let value = h
            .io_at(
                "Library.CanComms",
                "GetFloat",
                &args,
                site.clone(),
                time,
                Some(&mut adapter),
            )
            .unwrap();
        assert_eq!(value, Value::m1_float(12.0));
        assert_eq!(adapter.calls.len(), 1);
        let call = &adapter.calls[0];
        assert_eq!(
            call.receiver,
            ResolvedReceiver::Library {
                object: "CanComms".to_string()
            }
        );
        assert_eq!(call.source_receiver, "Library.CanComms");
        assert_eq!(call.site, site);
        assert_eq!(call.arguments, args);
        assert_eq!(call.time, time);
        assert!(h.trace.hardware.iter().any(|record| {
            record.source == HardwareValueSource::Adapter && record.site == site
        }));
    }

    #[test]
    fn exact_site_then_wildcard_scenario_values_precede_the_adapter() {
        let mut h = Harness::new();
        let exact = CallSite::new("Demo.Update.m1scr", 10);
        let other = CallSite::new("Demo.Update.m1scr", 20);
        h.env
            .set_io_override("CanComms.GetFloat", Value::m1_float(1.0));
        h.env
            .set_io_override_at("CanComms.GetFloat", exact.clone(), Value::m1_float(2.0));
        let mut adapter = RecordingAdapter {
            reply: AdapterReply::Value(Value::m1_float(3.0)),
            calls: Vec::new(),
        };
        let args = [Value::m1_unsigned(0), Value::m1_integer(0)];

        assert_eq!(
            h.io_at(
                "CanComms",
                "GetFloat",
                &args,
                exact,
                crate::hardware::EvalTime::at_start(0.01),
                Some(&mut adapter),
            )
            .unwrap(),
            Value::m1_float(2.0)
        );
        assert_eq!(
            h.io_at(
                "CanComms",
                "GetFloat",
                &args,
                other,
                crate::hardware::EvalTime::at_start(0.01),
                Some(&mut adapter),
            )
            .unwrap(),
            Value::m1_float(1.0)
        );
        assert!(adapter.calls.is_empty(), "scenario values must win");
        let sources: Vec<HardwareValueSource> = h
            .trace
            .hardware
            .iter()
            .map(|record| record.source)
            .collect();
        assert!(sources.contains(&HardwareValueSource::ScenarioExact));
        assert!(sources.contains(&HardwareValueSource::ScenarioWildcard));
    }

    #[test]
    fn per_system_logging_overload_returns_typed_external_stub() {
        let mut h = Harness::new();
        // `Logging.Running(system)` (one Integer arg) has no *specific* stub (only
        // the zero-arg overload does), but it is a real `Boolean` IO method, so the
        // generic typed stub is false (externally driven), not a fail-loud abort.
        assert_eq!(
            h.io("Logging", "Running", &[Value::m1_integer(0)]).unwrap(),
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
    fn adapter_gets_first_refusal_before_an_unknown_hardware_method_fails() {
        let mut h = Harness::new();
        let mut adapter = RecordingAdapter {
            reply: AdapterReply::Value(Value::m1_unsigned(44)),
            calls: Vec::new(),
        };
        let value = h
            .io_at(
                "CanComms",
                "NotARealMethod",
                &[],
                CallSite::new("Demo.Update.m1scr", 90),
                crate::hardware::EvalTime::at_start(0.01),
                Some(&mut adapter),
            )
            .expect("adapter handles method absent from the catalog");
        assert_eq!(value, Value::m1_unsigned(44));
        assert_eq!(adapter.calls[0].canonical_name(), "CanComms.NotARealMethod");
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
        assert_eq!(
            typed_io_default("CanComms", "GetFloat"),
            Some(Value::m1_float(0.0))
        );
        assert_eq!(
            typed_io_default("CanComms", "GetFixed7DP"),
            Some(Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::ZERO)))
        );
        assert_eq!(
            typed_io_default("CanComms", "GetID"),
            Some(Value::m1_integer(0))
        );
        assert_eq!(
            typed_io_default("CanComms", "BusRxTotal"),
            Some(Value::m1_unsigned(0))
        );
        assert_eq!(
            typed_io_default("CanComms", "RxMessage"),
            Some(Value::Bool(false))
        );
        assert_eq!(
            typed_io_default("CanComms", "SetFloat"),
            Some(Value::Bool(true))
        );
        assert_eq!(
            typed_io_default("CanComms", "RxOpenStandard"),
            Some(Value::Bool(true))
        );
        // A method not in the registry → None (the caller fails loud).
        assert_eq!(typed_io_default("CanComms", "NotARealMethod"), None);
    }

    #[test]
    fn project_can_reader_stubs_are_typed() {
        let mut h = Harness::new();
        // A DBC CAN signal reader has no offline value; each stubs to the
        // type-correct zero of its M1 return type, flagged externally driven.
        assert_eq!(
            h.io("DashVals.Aux Switch", "GetInteger", &[]).unwrap(),
            Value::m1_integer(0)
        );
        assert_eq!(
            h.io("DashVals.Aux Switch", "GetBit", &[]).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            h.io("DashVals.Aux Switch", "GetFloat", &[]).unwrap(),
            Value::m1_float(0.0)
        );
        assert!(h.trace.is_external("DashVals.Aux Switch.GetInteger"));
        assert!(h.trace.is_external("DashVals.Aux Switch.GetBit"));
        assert!(h.trace.is_external("DashVals.Aux Switch.GetFloat"));
    }

    #[test]
    fn project_can_writer_stubs_are_noops() {
        let mut h = Harness::new();
        // A DBC CAN signal write is a no-op offline: each returns the unit value
        // so an expression-statement call succeeds, flagged externally driven.
        for method in ["SetInteger", "SetScaled", "SetFloat", "SetFromBaseUnit"] {
            assert_eq!(
                h.io(
                    "DashVals.Aux Switch",
                    method,
                    &[Value::m1_unsigned(0), Value::m1_integer(1)],
                )
                .unwrap(),
                Value::Bool(true),
                "{method} should stub to the unit value"
            );
            assert!(
                h.trace
                    .is_external(&format!("DashVals.Aux Switch.{method}"))
            );
        }
    }
}
