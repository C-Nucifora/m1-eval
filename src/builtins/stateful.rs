// SPDX-License-Identifier: GPL-3.0-or-later
//! The stateful (time-domain) builtins — the hard core of the evaluator.
//!
//! Each operator is a small state machine keyed by its [`CallSite`] and advanced
//! once per tick by `ctx.dt`. State lives in [`crate::env::StateStore`] as an
//! [`OpState`] variant; a fresh site starts [`OpState::Uninit`] and the operator
//! seeds itself on its first tick so the discretisation has a defined prior
//! value. The update laws below are paraphrased from our understanding of the M1
//! library (never copied from the proprietary manuals) and are unit-tested with
//! hand-derived values; exact MoTeC fidelity is a follow-up validated against M1
//! Sim.
//!
//! ## Filter family — `Filter.FirstOrder/Maximum/Minimum`
//!
//! A first-order (single-pole) low-pass discretised as the exponential smoother
//!
//! ```text
//! a0 = dt / (tc + dt)            (the per-tick blend factor, in [0, 1))
//! y[n] = a0 * x[n] + (1 - a0) * y[n-1]
//! ```
//!
//! with time constant `tc` (seconds). A larger `tc` (relative to `dt`) means a
//! smaller `a0` and heavier smoothing; `tc = 0` makes `a0 = 1` so the output
//! follows the input exactly. The output seeds to the first input on the first
//! tick (no startup transient). An optional `reset` argument reloads the state to
//! the current input on the tick it is true.
//!
//! `Filter.Maximum` tracks a decaying peak: when the input is at or above the
//! held value the output jumps straight to the input (instantaneous attack);
//! otherwise it relaxes *down* toward the lower input through the first-order
//! law. `Filter.Minimum` is the mirror image (instant downward attack, filtered
//! rise).

use crate::env::{CallSite, OpState};
use crate::error::EvalError;
use crate::expr::EvalCtx;
use crate::value::Value;

/// Evaluate one stateful builtin call `object.method(args)`. Returns `Ok(None)`
/// when `object` is not a stateful family handled here, so the dispatcher can
/// continue to other branches (and ultimately fail loud). Arity is validated by
/// the dispatcher against the intrinsic library before this runs.
pub fn call(
    object: &str,
    method: &str,
    args: &[Value],
    site: CallSite,
    ctx: &mut EvalCtx,
) -> Result<Option<Value>, EvalError> {
    let v = match object {
        "Filter" => filter(method, args, site, ctx)?,
        _ => return Ok(None),
    };
    Ok(v.map(Some).unwrap_or(None))
}

/// Evaluate a `Timer` object method (`Start`/`Stop`/`Reset`/`Remaining`).
/// Returns `Ok(None)` for a non-Timer method so the caller fails loud.
/// Placeholder pending Task 18.
pub fn timer(
    _method: &str,
    _args: &[Value],
    _site: CallSite,
    _ctx: &mut EvalCtx,
) -> Result<Option<Value>, EvalError> {
    Ok(None)
}

/// The blend factor `a0 = dt / (tc + dt)` for a first-order filter with time
/// constant `tc`. Clamped to `[0, 1]`: a non-positive `tc` makes the output
/// follow the input (`a0 = 1`); a huge `tc` makes `a0 -> 0` (maximum smoothing).
fn first_order_alpha(tc: f64, dt: f64) -> f64 {
    let denom = tc + dt;
    if denom <= 0.0 {
        return 1.0;
    }
    (dt / denom).clamp(0.0, 1.0)
}

/// Dispatch the `Filter.*` family. Returns `Ok(None)` for an unrecognised method
/// so the caller fails loud.
fn filter(
    method: &str,
    args: &[Value],
    site: CallSite,
    ctx: &mut EvalCtx,
) -> Result<Option<Value>, EvalError> {
    match method {
        "FirstOrder" => first_order(args, site, ctx).map(Some),
        "Maximum" => extremum(args, site, ctx, true).map(Some),
        "Minimum" => extremum(args, site, ctx, false).map(Some),
        _ => Ok(None),
    }
}

/// `Filter.FirstOrder(x, tc [, reset])`: the exponential smoother documented in
/// the module header. Seeds to the first input; `reset` reloads the state to the
/// current input.
fn first_order(args: &[Value], site: CallSite, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    let x = args[0].as_f64()?;
    let tc = args[1].as_f64()?;
    let reset = match args.get(2) {
        Some(v) => v.as_bool()?,
        None => false,
    };
    let dt = ctx.dt;
    let a0 = first_order_alpha(tc, dt);

    let slot = ctx.state.entry(site);
    let prev = match slot {
        OpState::Filter { y } => Some(*y),
        _ => None,
    };
    // Seed to the input on the first tick or on reset; otherwise blend.
    let y = if reset || prev.is_none() {
        x
    } else {
        a0 * x + (1.0 - a0) * prev.unwrap()
    };
    *slot = OpState::Filter { y };
    Ok(Value::Float(y))
}

/// `Filter.Maximum`/`Minimum`(x, tc [, reset]). `want_max` selects the decaying
/// peak (instant rise, filtered fall) versus the decaying trough (instant fall,
/// filtered rise). On the attack side the output snaps to the input; on the
/// relax side it follows the first-order law toward the input.
fn extremum(
    args: &[Value],
    site: CallSite,
    ctx: &mut EvalCtx,
    want_max: bool,
) -> Result<Value, EvalError> {
    let x = args[0].as_f64()?;
    let tc = args[1].as_f64()?;
    let reset = match args.get(2) {
        Some(v) => v.as_bool()?,
        None => false,
    };
    let a0 = first_order_alpha(tc, ctx.dt);

    let slot = ctx.state.entry(site);
    let prev = match slot {
        OpState::Filter { y } => Some(*y),
        _ => None,
    };
    let y = match prev {
        // First tick or reset: seed to the input.
        _ if reset => x,
        None => x,
        Some(p) => {
            let attack = if want_max { x >= p } else { x <= p };
            if attack {
                // Instant attack toward the new extremum.
                x
            } else {
                // Filtered relax toward the input.
                a0 * x + (1.0 - a0) * p
            }
        }
    };
    *slot = OpState::Filter { y };
    Ok(Value::Float(y))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calib::Calibration;
    use crate::env::{Env, StateStore};
    use m1_typecheck::Project;
    use std::path::Path;

    fn mini_project() -> Project {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini");
        crate::loader::load(&dir.join("Project.m1prj"), None)
            .expect("mini fixture loads")
            .project
    }

    /// A harness owning the stores so a fresh `EvalCtx` (with a chosen `dt`) can
    /// be built per tick, driving one operator at a fixed call site.
    struct Harness {
        project: Project,
        calib: Calibration,
        env: Env,
        state: StateStore,
        dt: f64,
    }

    impl Harness {
        fn new(dt: f64) -> Harness {
            Harness {
                project: mini_project(),
                calib: Calibration::default(),
                env: Env::new(),
                state: StateStore::new(),
                dt,
            }
        }

        /// Advance the operator one tick. The call site is fixed (offset 0) so
        /// state accumulates across ticks, as it would for one source occurrence.
        fn tick(&mut self, object: &str, method: &str, args: &[Value]) -> Value {
            let site = CallSite::new("Demo.Update.m1scr", 0);
            let mut ctx = EvalCtx {
                project: &self.project,
                calib: &self.calib,
                env: &mut self.env,
                state: &mut self.state,
                group: Some("Root.Demo"),
                fn_symbol: Some("Root.Demo.Update"),
                script_name: "Demo.Update.m1scr",
                dt: self.dt,
                trace: None,
            };
            call(object, method, args, site, &mut ctx)
                .expect("call ok")
                .expect("stateful family handled")
        }
    }

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "expected {b}, got {a}");
    }

    #[test]
    fn first_order_step_response_matches_hand_derivation() {
        // dt = 0.1, tc = 0.1  =>  a0 = 0.1 / (0.1 + 0.1) = 0.5.
        let mut h = Harness::new(0.1);
        let tc = Value::Float(0.1);

        // Tick 1: seeds to the first input (0.0). No transient.
        approx(h.tick("Filter", "FirstOrder", &[Value::Float(0.0), tc.clone()]).as_f64().unwrap(), 0.0);
        // Step the input to 1.0. y = 0.5*1 + 0.5*0 = 0.5.
        approx(h.tick("Filter", "FirstOrder", &[Value::Float(1.0), tc.clone()]).as_f64().unwrap(), 0.5);
        // Hold at 1.0. y = 0.5*1 + 0.5*0.5 = 0.75.
        approx(h.tick("Filter", "FirstOrder", &[Value::Float(1.0), tc.clone()]).as_f64().unwrap(), 0.75);
        // Hold. y = 0.5*1 + 0.5*0.75 = 0.875.
        approx(h.tick("Filter", "FirstOrder", &[Value::Float(1.0), tc]).as_f64().unwrap(), 0.875);
    }

    #[test]
    fn first_order_reset_reloads_to_current_input() {
        // a0 = 0.5 again.
        let mut h = Harness::new(0.1);
        let tc = Value::Float(0.1);
        h.tick("Filter", "FirstOrder", &[Value::Float(0.0), tc.clone(), Value::Bool(false)]);
        // Build up some state.
        h.tick("Filter", "FirstOrder", &[Value::Float(10.0), tc.clone(), Value::Bool(false)]);
        // Reset true: output snaps to the current input regardless of history.
        approx(
            h.tick("Filter", "FirstOrder", &[Value::Float(3.0), tc, Value::Bool(true)])
                .as_f64()
                .unwrap(),
            3.0,
        );
    }

    #[test]
    fn filter_maximum_instant_rise_filtered_fall() {
        // a0 = 0.5.
        let mut h = Harness::new(0.1);
        let tc = Value::Float(0.1);
        // Seed at 0.
        approx(h.tick("Filter", "Maximum", &[Value::Float(0.0), tc.clone()]).as_f64().unwrap(), 0.0);
        // Rise to 5: instant attack -> 5.
        approx(h.tick("Filter", "Maximum", &[Value::Float(5.0), tc.clone()]).as_f64().unwrap(), 5.0);
        // Drop to 1: filtered fall -> 0.5*1 + 0.5*5 = 3.0.
        approx(h.tick("Filter", "Maximum", &[Value::Float(1.0), tc.clone()]).as_f64().unwrap(), 3.0);
        // A new higher value 4 < 3.0? No, 4 >= 3.0 -> instant -> 4.
        approx(h.tick("Filter", "Maximum", &[Value::Float(4.0), tc]).as_f64().unwrap(), 4.0);
    }

    #[test]
    fn filter_minimum_instant_fall_filtered_rise() {
        let mut h = Harness::new(0.1);
        let tc = Value::Float(0.1);
        approx(h.tick("Filter", "Minimum", &[Value::Float(10.0), tc.clone()]).as_f64().unwrap(), 10.0);
        // Drop to 2: instant attack downward -> 2.
        approx(h.tick("Filter", "Minimum", &[Value::Float(2.0), tc.clone()]).as_f64().unwrap(), 2.0);
        // Rise to 6: filtered rise -> 0.5*6 + 0.5*2 = 4.0.
        approx(h.tick("Filter", "Minimum", &[Value::Float(6.0), tc]).as_f64().unwrap(), 4.0);
    }

    #[test]
    fn distinct_call_sites_keep_independent_state() {
        let mut h = Harness::new(0.1);
        let tc = Value::Float(0.1);
        // Two different sites driven through the store directly.
        let site_a = CallSite::new("Demo.Update.m1scr", 0);
        let site_b = CallSite::new("Demo.Update.m1scr", 50);
        for (site, seed) in [(site_a, 0.0), (site_b, 100.0)] {
            let mut ctx = EvalCtx {
                project: &h.project,
                calib: &h.calib,
                env: &mut h.env,
                state: &mut h.state,
                group: Some("Root.Demo"),
                fn_symbol: Some("Root.Demo.Update"),
                script_name: "Demo.Update.m1scr",
                dt: h.dt,
                trace: None,
            };
            let v = call("Filter", "FirstOrder", &[Value::Float(seed), tc.clone()], site, &mut ctx)
                .unwrap()
                .unwrap();
            approx(v.as_f64().unwrap(), seed);
        }
        // Two independent slots.
        assert_eq!(h.state.0.len(), 2);
    }

    #[test]
    fn unhandled_object_returns_none() {
        let mut h = Harness::new(0.1);
        let site = CallSite::new("Demo.Update.m1scr", 0);
        let mut ctx = EvalCtx {
            project: &h.project,
            calib: &h.calib,
            env: &mut h.env,
            state: &mut h.state,
            group: Some("Root.Demo"),
            fn_symbol: Some("Root.Demo.Update"),
            script_name: "Demo.Update.m1scr",
            dt: h.dt,
            trace: None,
        };
        assert!(call("NotStateful", "X", &[], site, &mut ctx).unwrap().is_none());
    }
}
