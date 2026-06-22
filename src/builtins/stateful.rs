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
    match object {
        "Filter" => filter(method, args, site, ctx),
        "Integral" => integral(method, args, site, ctx),
        "Derivative" => derivative(method, args, site, ctx),
        "Calculate" => calculate_stateful(method, args, site, ctx),
        _ => Ok(None),
    }
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

// ---- Integral family --------------------------------------------------------
//
// `Integral.Normal(x, min, max, reset, preset)` accumulates the running area
// under the input by the **trapezoidal rule**: each tick adds the area of the
// trapezoid between the previous and current samples,
//
//     area = (x[n] + x[n-1]) / 2 * dt
//
// to a running accumulator, then **clamps** the accumulator into `[min, max]`
// (anti-windup: the stored state is the clamped value, so the integral cannot
// run away while saturated). On the first tick there is no previous sample, so
// the accumulator seeds to `0` (or to the clamped `preset` if `reset` is true)
// and no area is added. When `reset` is true on any tick the accumulator is
// reloaded to the clamped `preset` before that tick's output.

/// Dispatch the `Integral.*` family.
fn integral(
    method: &str,
    args: &[Value],
    site: CallSite,
    ctx: &mut EvalCtx,
) -> Result<Option<Value>, EvalError> {
    match method {
        "Normal" => integral_normal(args, site, ctx).map(Some),
        _ => Ok(None),
    }
}

/// `Integral.Normal(x, min, max, reset, preset)` — trapezoidal accumulation,
/// clamped to `[min, max]`, with `reset` reloading the clamped `preset`.
fn integral_normal(args: &[Value], site: CallSite, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    let x = args[0].as_f64()?;
    let min = args[1].as_f64()?;
    let max = args[2].as_f64()?;
    let reset = args[3].as_bool()?;
    let preset = args[4].as_f64()?;
    let dt = ctx.dt;

    let slot = ctx.state.entry(site);
    let prev = match slot {
        OpState::Integral { acc, prev_x } => Some((*acc, *prev_x)),
        _ => None,
    };

    let (acc, prev_x) = match prev {
        // Reset on any tick: reload the clamped preset; this tick adds no area.
        _ if reset => (clamp(preset, min, max), x),
        // First tick: seed to zero (clamped), record the input.
        None => (clamp(0.0, min, max), x),
        Some((acc, prev_x)) => {
            let area = (x + prev_x) * 0.5 * dt;
            (clamp(acc + area, min, max), x)
        }
    };
    *slot = OpState::Integral { acc, prev_x };
    Ok(Value::Float(acc))
}

/// Clamp `v` into `[lo, hi]`. A reversed range (`lo > hi`) collapses to `hi`.
fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    v.max(lo).min(hi)
}

// ---- Derivative family ------------------------------------------------------
//
// `Derivative.Normal(x)` is the backward finite difference `(x[n] - x[n-1]) / dt`
// (units of input-per-second). The first tick has no previous sample, so it
// outputs `0` and seeds the previous input.
//
// `Derivative.Filtered(x)` passes that raw difference through a first-order
// smoother to tame the noise amplification differentiation causes. The intrinsic
// exposes no time constant, so Phase 1 uses a fixed internal constant
// [`FILTERED_DERIVATIVE_TC`] (documented assumption, to be validated against M1
// Sim): `d_filt[n] = a0*d_raw[n] + (1-a0)*d_filt[n-1]`, `a0 = dt/(tc+dt)`.
//
// `Derivative.Adaptive(x, delta, max_dt)` only recomputes the slope when the
// input has moved by at least `delta` or `max_dt` seconds have elapsed since the
// last update — otherwise it holds the previous value. This suppresses the
// divide-by-tiny-dt noise on near-constant signals. The slope is taken over the
// actual elapsed interval since the last accepted update.

/// The fixed time constant (seconds) for `Derivative.Filtered`'s internal
/// smoother. A documented Phase-1 assumption pending M1 Sim validation.
const FILTERED_DERIVATIVE_TC: f64 = 0.1;

/// Dispatch the `Derivative.*` family.
fn derivative(
    method: &str,
    args: &[Value],
    site: CallSite,
    ctx: &mut EvalCtx,
) -> Result<Option<Value>, EvalError> {
    match method {
        "Normal" => derivative_normal(args, site, ctx, false).map(Some),
        "Filtered" => derivative_normal(args, site, ctx, true).map(Some),
        "Adaptive" => derivative_adaptive(args, site, ctx).map(Some),
        _ => Ok(None),
    }
}

/// `Derivative.Normal`/`Filtered`(x). `filtered` selects whether the raw backward
/// difference is passed through the fixed first-order smoother.
fn derivative_normal(
    args: &[Value],
    site: CallSite,
    ctx: &mut EvalCtx,
    filtered: bool,
) -> Result<Value, EvalError> {
    let x = args[0].as_f64()?;
    let dt = ctx.dt;
    let slot = ctx.state.entry(site);
    let prev = match slot {
        OpState::Derivative { prev_x, prev_d } => Some((*prev_x, *prev_d)),
        _ => None,
    };

    let (d, prev_d_out) = match prev {
        // First tick: no slope yet.
        None => (0.0, 0.0),
        Some((prev_x, prev_d)) => {
            let raw = if dt > 0.0 { (x - prev_x) / dt } else { 0.0 };
            if filtered {
                let a0 = first_order_alpha(FILTERED_DERIVATIVE_TC, dt);
                let d = a0 * raw + (1.0 - a0) * prev_d;
                (d, d)
            } else {
                (raw, raw)
            }
        }
    };
    *slot = OpState::Derivative {
        prev_x: x,
        prev_d: prev_d_out,
    };
    Ok(Value::Float(d))
}

/// `Derivative.Adaptive(x, delta, max_dt)` — recompute the slope only when the
/// input has moved by `>= delta` or `max_dt` seconds have elapsed since the last
/// accepted update; otherwise hold the previous derivative.
fn derivative_adaptive(args: &[Value], site: CallSite, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    let x = args[0].as_f64()?;
    let delta = args[1].as_f64()?;
    let max_dt = args[2].as_f64()?;
    let dt = ctx.dt;
    let slot = ctx.state.entry(site);

    let prev = match slot {
        OpState::DerivativeAdaptive {
            last_x,
            prev_d,
            elapsed,
        } => Some((*last_x, *prev_d, *elapsed)),
        _ => None,
    };

    let (d, last_x, elapsed) = match prev {
        // First tick: seed the reference input, no slope yet.
        None => (0.0, x, 0.0),
        Some((last_x, prev_d, elapsed)) => {
            let elapsed = elapsed + dt;
            let moved_enough = (x - last_x).abs() >= delta.abs();
            let timed_out = max_dt > 0.0 && elapsed >= max_dt;
            if moved_enough || timed_out {
                // Accept an update: slope over the actual elapsed interval.
                let d = if elapsed > 0.0 { (x - last_x) / elapsed } else { prev_d };
                (d, x, 0.0)
            } else {
                // Hold the previous derivative; keep accumulating elapsed time.
                (prev_d, last_x, elapsed)
            }
        }
    };
    *slot = OpState::DerivativeAdaptive {
        last_x,
        prev_d: d,
        elapsed,
    };
    Ok(Value::Float(d))
}

/// Dispatch the stateful `Calculate.*` methods (`Stable`/`Hysteresis`/`Between`/
/// `Beyond`). Implemented in Task 18; until then a placeholder returning `None`
/// so the dispatcher falls through to the pure-`Calculate` submodule (which also
/// returns `None` for these), ultimately failing loud.
fn calculate_stateful(
    _method: &str,
    _args: &[Value],
    _site: CallSite,
    _ctx: &mut EvalCtx,
) -> Result<Option<Value>, EvalError> {
    Ok(None)
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

    // ---- Task 17: Integral.Normal ----

    fn integral(h: &mut Harness, x: f64, min: f64, max: f64, reset: bool, preset: f64) -> f64 {
        h.tick(
            "Integral",
            "Normal",
            &[
                Value::Float(x),
                Value::Float(min),
                Value::Float(max),
                Value::Bool(reset),
                Value::Float(preset),
            ],
        )
        .as_f64()
        .unwrap()
    }

    #[test]
    fn integral_trapezoidal_accumulation() {
        // dt = 0.1. Inputs 2, 4, 6 with a wide clamp.
        let mut h = Harness::new(0.1);
        // Tick 1: seed -> 0 (no area on the first sample).
        approx(integral(&mut h, 2.0, -100.0, 100.0, false, 0.0), 0.0);
        // Tick 2: area = (4+2)/2 * 0.1 = 0.3 -> acc 0.3.
        approx(integral(&mut h, 4.0, -100.0, 100.0, false, 0.0), 0.3);
        // Tick 3: area = (6+4)/2 * 0.1 = 0.5 -> acc 0.8.
        approx(integral(&mut h, 6.0, -100.0, 100.0, false, 0.0), 0.8);
    }

    #[test]
    fn integral_clamps_to_range() {
        // dt = 1.0 so areas are large; clamp at 1.0.
        let mut h = Harness::new(1.0);
        approx(integral(&mut h, 10.0, 0.0, 1.0, false, 0.0), 0.0); // seed
        // area = (10+10)/2 * 1 = 10 -> clamps to max 1.0.
        approx(integral(&mut h, 10.0, 0.0, 1.0, false, 0.0), 1.0);
        // Stays clamped (anti-windup: stored state is the clamped value).
        approx(integral(&mut h, 10.0, 0.0, 1.0, false, 0.0), 1.0);
    }

    #[test]
    fn integral_reset_reloads_clamped_preset() {
        let mut h = Harness::new(0.1);
        integral(&mut h, 2.0, -100.0, 100.0, false, 0.0); // seed
        integral(&mut h, 4.0, -100.0, 100.0, false, 0.0); // acc 0.3
        // Reset to preset 5.0 (within range): output becomes 5.0.
        approx(integral(&mut h, 9.0, -100.0, 100.0, true, 5.0), 5.0);
        // After reset, prev_x is the current input (9), so next area uses it.
        // area = (1+9)/2 * 0.1 = 0.5 -> acc 5.5.
        approx(integral(&mut h, 1.0, -100.0, 100.0, false, 0.0), 5.5);
    }

    // ---- Task 17: Derivative.* ----

    fn deriv(h: &mut Harness, method: &str, args: &[Value]) -> f64 {
        h.tick("Derivative", method, args).as_f64().unwrap()
    }

    #[test]
    fn derivative_normal_backward_difference() {
        // dt = 0.1.
        let mut h = Harness::new(0.1);
        // First tick: 0 (no slope yet), seeds prev = 1.0.
        approx(deriv(&mut h, "Normal", &[Value::Float(1.0)]), 0.0);
        // (3 - 1)/0.1 = 20.
        approx(deriv(&mut h, "Normal", &[Value::Float(3.0)]), 20.0);
        // (3 - 3)/0.1 = 0.
        approx(deriv(&mut h, "Normal", &[Value::Float(3.0)]), 0.0);
        // (2.5 - 3)/0.1 = -5.
        approx(deriv(&mut h, "Normal", &[Value::Float(2.5)]), -5.0);
    }

    #[test]
    fn derivative_filtered_smooths_the_raw_difference() {
        // dt = 0.1, fixed tc = 0.1 -> a0 = 0.5 in the internal smoother.
        let mut h = Harness::new(0.1);
        // First tick: 0 (seed).
        approx(deriv(&mut h, "Filtered", &[Value::Float(0.0)]), 0.0);
        // raw = (1 - 0)/0.1 = 10; d = 0.5*10 + 0.5*0 = 5.0.
        approx(deriv(&mut h, "Filtered", &[Value::Float(1.0)]), 5.0);
        // raw = (2 - 1)/0.1 = 10; d = 0.5*10 + 0.5*5 = 7.5.
        approx(deriv(&mut h, "Filtered", &[Value::Float(2.0)]), 7.5);
    }

    #[test]
    fn derivative_adaptive_holds_until_delta_or_timeout() {
        // dt = 0.1, delta = 1.0, max_dt = 0.5.
        let mut h = Harness::new(0.1);
        let args = |x: f64| {
            [
                Value::Float(x),
                Value::Float(1.0),
                Value::Float(0.5),
            ]
        };
        // First tick: seed at 0.0, output 0.
        approx(deriv(&mut h, "Adaptive", &args(0.0)), 0.0);
        // Move +0.3 (< delta 1.0) and elapsed 0.1 (< 0.5): hold previous (0).
        approx(deriv(&mut h, "Adaptive", &args(0.3)), 0.0);
        // Move to 1.5 (delta from 0.0 is 1.5 >= 1.0): accept. elapsed = 0.2,
        // slope = (1.5 - 0.0)/0.2 = 7.5.
        approx(deriv(&mut h, "Adaptive", &args(1.5)), 7.5);
        // Now hold near 1.5: small moves under delta accumulate elapsed time.
        approx(deriv(&mut h, "Adaptive", &args(1.6)), 7.5); // elapsed 0.1, held
        approx(deriv(&mut h, "Adaptive", &args(1.7)), 7.5); // elapsed 0.2, held
        approx(deriv(&mut h, "Adaptive", &args(1.8)), 7.5); // elapsed 0.3, held
        approx(deriv(&mut h, "Adaptive", &args(1.9)), 7.5); // elapsed 0.4, held
        // elapsed reaches 0.5 (>= max_dt): accept. slope = (2.0 - 1.5)/0.5 = 1.0.
        approx(deriv(&mut h, "Adaptive", &args(2.0)), 1.0);
    }
}
