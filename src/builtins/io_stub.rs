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
//! 2. **Documented stub.** A small set of calls have a determinate offline value
//!    — e.g. `System.TickPeriod()` is the evaluator's tick step `ctx.dt`, and
//!    `System.XcpConnected()`/`Logging.Running()` are false because no tuning
//!    tool or logger is attached offline. The `Void` side-effect calls
//!    (`System.Debug`, `System.AllowTuning`, …) are no-ops returning a benign
//!    value. Each stub's meaning is documented below, never copied from MoTeC.
//! 3. **Fail loud.** Anything else — a CAN/serial *read* whose value we would
//!    have to fabricate, an un-stubbed `System`/`Logging` call — returns
//!    [`EvalError::UnsupportedBuiltin`]. We never invent a hardware value.
//!
//! Whenever a value is produced (override or stub), the call is flagged
//! externally driven in the [`Trace`](crate::trace::Trace) so a consumer knows
//! that column is simulated input, not evaluated output.

use crate::error::EvalError;
use crate::expr::EvalCtx;
use crate::value::Value;

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

    // 2. Documented offline stubs.
    if let Some(v) = documented_stub(object, method, args, ctx)? {
        mark_external(ctx, &key);
        return Ok(v);
    }

    // 3. Fail loud — never fabricate a hardware value.
    Err(EvalError::UnsupportedBuiltin {
        object: object.to_string(),
        method: method.to_string(),
    })
}

/// Flag an IO call's channel as externally driven in the trace, if a sink is
/// active.
fn mark_external(ctx: &mut EvalCtx, key: &str) {
    if let Some(trace) = ctx.trace.as_deref_mut() {
        trace.mark_external(key);
    }
}

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
    fn unstubbed_can_read_fails_loud() {
        let mut h = Harness::new();
        // No scenario value, no documented stub: must fail loud, never fabricate.
        match h.io("CanComms", "GetFloat", &[Value::Uint(0), Value::Int(0)]) {
            Err(EvalError::UnsupportedBuiltin { object, method }) => {
                assert_eq!(object, "CanComms");
                assert_eq!(method, "GetFloat");
            }
            other => panic!("expected UnsupportedBuiltin, got {other:?}"),
        }
        // A failed call is not marked external.
        assert!(!h.trace.is_external("CanComms.GetFloat"));
    }

    #[test]
    fn unstubbed_system_call_fails_loud() {
        let mut h = Harness::new();
        // ElapsedTime has no determinate offline value -> fail loud.
        match h.io("System", "ElapsedTime", &[]) {
            Err(EvalError::UnsupportedBuiltin { method, .. }) => assert_eq!(method, "ElapsedTime"),
            other => panic!("expected UnsupportedBuiltin, got {other:?}"),
        }
    }

    #[test]
    fn per_system_logging_overload_fails_loud() {
        let mut h = Harness::new();
        // Logging.Running(system) (one Integer arg) has no offline stub.
        match h.io("Logging", "Running", &[Value::Int(0)]) {
            Err(EvalError::UnsupportedBuiltin { .. }) => {}
            other => panic!("expected UnsupportedBuiltin, got {other:?}"),
        }
    }
}
