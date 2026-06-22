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
        "Delay" => delay(method, args, site, ctx),
        "Debounce" => debounce(method, args, site, ctx),
        "Change" => change(method, args, site, ctx),
        "Calculate" => calculate_stateful(method, args, site, ctx),
        _ => Ok(None),
    }
}

/// Evaluate a `Timer` object method (`Start`/`Stop`/`Reset`/`Remaining`).
/// Returns `Ok(None)` for a non-Timer method so the caller fails loud.
///
/// A `Timer` is a project *object*, so all four methods on the same object must
/// share one countdown. We therefore key the state by the **object path**
/// (`object_key`, a [`CallSite`] with the object path in the script slot and a
/// zero offset) rather than the individual call site — `Start`/`Remaining`/… on
/// one Timer all address the same state. The countdown advances by `ctx.dt` each
/// time [`Remaining`](Timer::Remaining) is read (the documented Phase-1 model: a
/// Timer is read once per tick, so reading advances it one tick), clamped at
/// zero. `Start(period)` (re)loads the period and runs; `Stop` halts without
/// clearing; `Reset` clears to zero and halts.
pub fn timer(
    method: &str,
    args: &[Value],
    object_key: CallSite,
    ctx: &mut EvalCtx,
) -> Result<Option<Value>, EvalError> {
    // Only the Timer object methods are handled; anything else is not a timer.
    if !matches!(method, "Start" | "Stop" | "Reset" | "Remaining") {
        return Ok(None);
    }
    let dt = ctx.dt;
    let slot = ctx.state.entry(object_key);
    // Read the current countdown (default: stopped at zero).
    let (mut remaining, mut running) = match slot {
        OpState::Timer { remaining, running } => (*remaining, *running),
        _ => (0.0, false),
    };

    let result = match method {
        "Start" => {
            // Start (or restart) counting down from `period`.
            remaining = args[0].as_f64()?.max(0.0);
            running = true;
            Value::Bool(true) // Void in M1; we return a benign value.
        }
        "Stop" => {
            running = false;
            Value::Bool(true)
        }
        "Reset" => {
            remaining = 0.0;
            running = false;
            Value::Bool(true)
        }
        "Remaining" => {
            // Advance the countdown one tick on read, clamped at zero.
            if running {
                remaining = (remaining - dt).max(0.0);
                if remaining == 0.0 {
                    running = false;
                }
            }
            Value::Float(remaining)
        }
        _ => unreachable!("guarded above"),
    };
    *slot = OpState::Timer { remaining, running };
    Ok(Some(result))
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
    let y = match prev {
        Some(p) if !reset => a0 * x + (1.0 - a0) * p,
        _ => x,
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

// ---- Delay family -----------------------------------------------------------
//
// `Delay.Rising(cond, delay)` delays only the *rising* edge: the output goes
// true once `cond` has been continuously true for `>= delay` seconds, and falls
// immediately when `cond` goes false. `Delay.Falling(cond, delay)` is the mirror
// image — it delays only the *falling* edge: the output goes false once `cond`
// has been continuously false for `>= delay` seconds, and rises immediately when
// `cond` goes true. The held-time accumulator counts the time the *delayed-edge*
// candidate has been sustained.

/// Dispatch the `Delay.*` family. Only the boolean edge-delay overloads
/// (`Rising`/`Falling`) and the value `Stable` predicate are implemented in
/// Phase 1; the buffered sample-delays (`SignalN`) return `None` (fail loud).
fn delay(
    method: &str,
    args: &[Value],
    site: CallSite,
    ctx: &mut EvalCtx,
) -> Result<Option<Value>, EvalError> {
    match method {
        "Rising" => edge_delay(args, site, ctx, true).map(Some),
        "Falling" => edge_delay(args, site, ctx, false).map(Some),
        "Stable" => delay_stable(args, site, ctx).map(Some),
        _ => Ok(None),
    }
}

/// `Delay.Rising`/`Falling`. For `rising`, the delayed edge is the transition to
/// true; for falling, the transition to false. While `cond` equals the
/// delayed-edge target value, the held time accumulates and the output switches
/// to that value once held `>= delay`. The opposite value takes effect at once.
fn edge_delay(
    args: &[Value],
    site: CallSite,
    ctx: &mut EvalCtx,
    rising: bool,
) -> Result<Value, EvalError> {
    let cond = args[0].as_bool()?;
    let delay = args[1].as_f64()?;
    let dt = ctx.dt;
    let slot = ctx.state.entry(site);
    let prev = match slot {
        OpState::Timed { output, held, .. } => Some((*output, *held)),
        _ => None,
    };

    // The value whose edge is delayed (true for Rising, false for Falling).
    let delayed_value = rising;
    let (output, held) = match prev {
        // First tick: the delayed edge cannot have ripened yet, so the output is
        // the immediate (opposite) value. If cond already equals the delayed
        // value we begin timing from zero.
        None => (!delayed_value, 0.0),
        Some((output, held)) => {
            if cond == delayed_value {
                // Building toward the delayed edge: accumulate, switch when ripe.
                let held = held + dt;
                let output = if held >= delay { delayed_value } else { output };
                (output, held)
            } else {
                // The opposite edge is immediate; reset the held timer.
                (!delayed_value, 0.0)
            }
        }
    };
    *slot = OpState::Timed {
        output,
        candidate: cond,
        held,
    };
    Ok(Value::Bool(output))
}

/// `Delay.Stable(arg, delay [, delta])` — the output goes true once the numeric
/// `arg` has stayed within `delta` of its reference for `>= delay` seconds; any
/// move beyond `delta` restarts the timer (and the output drops to false). With
/// no `delta` the argument must be exactly unchanged. Uses the `ChangeBy` slot to
/// hold the reference value and the held time.
fn delay_stable(args: &[Value], site: CallSite, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    let x = args[0].as_f64()?;
    let delay = args[1].as_f64()?;
    let delta = match args.get(2) {
        Some(v) => v.as_f64()?.abs(),
        None => 0.0,
    };
    let dt = ctx.dt;
    let slot = ctx.state.entry(site);
    let prev = match slot {
        OpState::ChangeBy { prev_x, held, pending } => Some((*prev_x, *held, *pending)),
        _ => None,
    };
    let (reference, held, output) = match prev {
        // First tick: start timing from the current value, not yet stable.
        None => (x, 0.0, false),
        Some((reference, held, _)) => {
            if (x - reference).abs() > delta {
                // Moved beyond tolerance: restart timing from the new value.
                (x, 0.0, false)
            } else {
                let held = held + dt;
                (reference, held, held >= delay)
            }
        }
    };
    *slot = OpState::ChangeBy {
        prev_x: reference,
        held,
        pending: output,
    };
    Ok(Value::Bool(output))
}

// ---- Debounce family --------------------------------------------------------
//
// A debounce suppresses spurious flips: the output only adopts a new condition
// value once that value has been held *stably* for `>= filter` seconds; any
// reversal before the filter time restarts the timer. `Stable`, `Fast` and
// `Verify` share this accept-after-stable model in Phase 1 (their finer MoTeC
// distinctions are a fidelity follow-up). `Filter` instead low-pass filters the
// 0/1 condition (time constant `response`) and applies a Schmitt trigger:
// output rises at `>= 0.8`, falls at `<= 0.2`.

/// Dispatch the `Debounce.*` family.
fn debounce(
    method: &str,
    args: &[Value],
    site: CallSite,
    ctx: &mut EvalCtx,
) -> Result<Option<Value>, EvalError> {
    match method {
        "Stable" | "Fast" | "Verify" => debounce_stable(args, site, ctx).map(Some),
        "Filter" => debounce_filter(args, site, ctx).map(Some),
        _ => Ok(None),
    }
}

/// The accept-after-stable debounce: the output adopts `cond` once `cond` has
/// been held continuously for `>= filter` seconds. A change resets the timer; the
/// output holds its last committed value until the new value is confirmed.
fn debounce_stable(args: &[Value], site: CallSite, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    let cond = args[0].as_bool()?;
    let filter = args[1].as_f64()?;
    let dt = ctx.dt;
    let slot = ctx.state.entry(site);
    let prev = match slot {
        OpState::Timed { output, candidate, held } => Some((*output, *candidate, *held)),
        _ => None,
    };
    let (output, candidate, held) = match prev {
        // First tick: adopt cond immediately as the committed output.
        None => (cond, cond, 0.0),
        Some((output, candidate, held)) => {
            if cond != candidate {
                // New candidate: restart timing from this tick.
                (output, cond, 0.0)
            } else {
                let held = held + dt;
                // Once the candidate has been stable long enough, commit it.
                let output = if held >= filter { cond } else { output };
                (output, candidate, held)
            }
        }
    };
    *slot = OpState::Timed { output, candidate, held };
    Ok(Value::Bool(output))
}

/// The filtered debounce: low-pass the 0/1 condition with time constant
/// `response`, then a 0.2/0.8 Schmitt trigger. The filtered level lives in the
/// `Filter` slot's `y`; the committed boolean lives in `Timed::output` — but we
/// only have one slot per site, so we pack the filtered level into `held` and the
/// output into `output` of a `Timed` state (candidate unused).
fn debounce_filter(args: &[Value], site: CallSite, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    let cond = args[0].as_bool()?;
    let response = args[1].as_f64()?;
    let a0 = first_order_alpha(response, ctx.dt);
    let target = if cond { 1.0 } else { 0.0 };
    let slot = ctx.state.entry(site);
    let (prev_level, prev_out) = match slot {
        OpState::Timed { output, held, .. } => (*held, *output),
        _ => (target, cond), // seed the filter to the first input
    };
    let level = a0 * target + (1.0 - a0) * prev_level;
    // Schmitt trigger with 0.2/0.8 thresholds; hold otherwise.
    let output = if level >= 0.8 {
        true
    } else if level <= 0.2 {
        false
    } else {
        prev_out
    };
    *slot = OpState::Timed {
        output,
        candidate: cond,
        held: level,
    };
    Ok(Value::Bool(output))
}

// ---- Change family ----------------------------------------------------------
//
// The `Change.*` operators emit a one-tick (one-shot) pulse when their argument
// changes in a particular way, comparing against the *previous tick's* value
// (documented Phase-1 model):
//
// - `By(x, delta)`   — pulse when `|x - prev| >= delta`.
// - `Up(x, delta)`   — pulse when `x - prev >= delta`.
// - `Down(x, delta)` — pulse when `prev - x >= delta`.
// - `To(cond)`       — pulse on the rising edge of `cond`.
// - `From(cond)`     — pulse on the falling edge of `cond`.
// - `Either(cond)`   — pulse on any edge of `cond`.
//
// The `filter`-bearing overloads require the change to persist for `>= filter`
// seconds before the single pulse is emitted (then re-arm). The very first tick
// seeds the previous value and emits no pulse.

/// Dispatch the `Change.*` family.
fn change(
    method: &str,
    args: &[Value],
    site: CallSite,
    ctx: &mut EvalCtx,
) -> Result<Option<Value>, EvalError> {
    match method {
        "By" => change_by(args, site, ctx, ChangeDir::By).map(Some),
        "Up" => change_by(args, site, ctx, ChangeDir::Up).map(Some),
        "Down" => change_by(args, site, ctx, ChangeDir::Down).map(Some),
        "To" => change_edge(args, site, ctx, EdgeDir::To).map(Some),
        "From" => change_edge(args, site, ctx, EdgeDir::From).map(Some),
        "Either" => change_edge(args, site, ctx, EdgeDir::Either).map(Some),
        _ => Ok(None),
    }
}

#[derive(Clone, Copy)]
enum ChangeDir {
    By,
    Up,
    Down,
}

#[derive(Clone, Copy)]
enum EdgeDir {
    To,
    From,
    Either,
}

/// `Change.By`/`Up`/`Down`(x, delta [, filter]). Compares the current value
/// against the previous tick's. Without `filter`, a qualifying change pulses true
/// for that one tick. With `filter`, the change must persist for `>= filter`
/// seconds (no opposing reversal) before a single pulse is emitted.
fn change_by(
    args: &[Value],
    site: CallSite,
    ctx: &mut EvalCtx,
    dir: ChangeDir,
) -> Result<Value, EvalError> {
    let x = args[0].as_f64()?;
    let delta = args[1].as_f64()?.abs();
    let filter = match args.get(2) {
        Some(v) => Some(v.as_f64()?),
        None => None,
    };
    let dt = ctx.dt;
    let slot = ctx.state.entry(site);
    let prev = match slot {
        OpState::ChangeBy { prev_x, held, pending } => Some((*prev_x, *held, *pending)),
        _ => None,
    };

    let qualifies = |from: f64, to: f64| -> bool {
        match dir {
            ChangeDir::By => (to - from).abs() >= delta,
            ChangeDir::Up => to - from >= delta,
            ChangeDir::Down => from - to >= delta,
        }
    };

    let (pulse, prev_x, held, pending) = match prev {
        // First tick: seed, no pulse.
        None => (false, x, 0.0, false),
        Some((prev_x, held, pending)) => {
            match filter {
                // Unfiltered: instantaneous one-shot against the previous tick.
                None => (qualifies(prev_x, x), x, 0.0, false),
                Some(filter) => {
                    if qualifies(prev_x, x) {
                        // The change condition holds this tick; time it.
                        let held = held + dt;
                        if !pending && held >= filter {
                            // Emit a single pulse and mark armed-spent.
                            (true, prev_x, held, true)
                        } else {
                            (false, prev_x, held, pending)
                        }
                    } else {
                        // Condition lapsed: re-arm against the new reference.
                        (false, x, 0.0, false)
                    }
                }
            }
        }
    };
    *slot = OpState::ChangeBy { prev_x, held, pending };
    Ok(Value::Bool(pulse))
}

/// `Change.To`/`From`/`Either`(cond [, filter]). Detects an edge of the boolean
/// `cond`. Without `filter`, pulses true for the one tick after the edge. With
/// `filter`, the new level must persist for `>= filter` seconds before the pulse.
fn change_edge(
    args: &[Value],
    site: CallSite,
    ctx: &mut EvalCtx,
    dir: EdgeDir,
) -> Result<Value, EvalError> {
    let cond = args[0].as_bool()?;
    let filter = match args.get(1) {
        Some(v) => Some(v.as_f64()?),
        None => None,
    };
    let dt = ctx.dt;
    let slot = ctx.state.entry(site);
    let prev = match slot {
        OpState::ChangeEdge { prev, held } => Some((*prev, *held)),
        _ => None,
    };

    let edge_matches = |from: bool, to: bool| -> bool {
        match dir {
            EdgeDir::To => !from && to,   // rising
            EdgeDir::From => from && !to, // falling
            EdgeDir::Either => from != to,
        }
    };

    let (pulse, prev_out, held) = match prev {
        // First tick: seed, no pulse.
        None => (false, cond, 0.0),
        Some((prev, held)) => {
            match filter {
                None => {
                    // Instant one-shot on the matching edge.
                    (edge_matches(prev, cond), cond, 0.0)
                }
                Some(filter) => {
                    if edge_matches(prev, cond) {
                        // Edge just happened: start (or continue) timing the new
                        // level; reuse `held` to count time since the edge.
                        (false, cond, dt)
                    } else if held > 0.0 && cond == prev {
                        // Level held since the edge: accumulate; pulse once ripe.
                        let held = held + dt;
                        if held >= filter {
                            (true, cond, 0.0) // pulse, then disarm
                        } else {
                            (false, cond, held)
                        }
                    } else {
                        // Reverted before ripening, or steady state: disarm.
                        (false, cond, 0.0)
                    }
                }
            }
        }
    };
    *slot = OpState::ChangeEdge { prev: prev_out, held };
    Ok(Value::Bool(pulse))
}

// ---- Stateful Calculate predicates ------------------------------------------
//
// `Calculate.Stable(x, filter)`   — true once `x` has not changed for `>= filter`.
// `Calculate.Between(x, min, max, filter)` — true once `x` is within `[min,max]`
//                                            for `>= filter` seconds.
// `Calculate.Beyond(x, min, max, filter)`  — true once `x` is outside `[min,max]`
//                                            for `>= filter` seconds.
// `Calculate.Hysteresis(x, low, high, filter)` — Schmitt trigger: goes true once
//   `x >= high` for `>= filter`, false once `x <= low` for `>= filter`, else
//   holds. All share the accept-after-stable timing model.

/// Dispatch the stateful `Calculate.*` predicates. Returns `Ok(None)` for the
/// pure methods so the dispatcher routes them to the pure submodule.
fn calculate_stateful(
    method: &str,
    args: &[Value],
    site: CallSite,
    ctx: &mut EvalCtx,
) -> Result<Option<Value>, EvalError> {
    match method {
        "Stable" => calc_stable(args, site, ctx).map(Some),
        "Between" => calc_between(args, site, ctx, false).map(Some),
        "Beyond" => calc_between(args, site, ctx, true).map(Some),
        "Hysteresis" => calc_hysteresis(args, site, ctx).map(Some),
        _ => Ok(None),
    }
}

/// `Calculate.Stable(x, filter)` — true once `x` has been exactly unchanged for
/// `>= filter` seconds; any change restarts the timer (output false meanwhile).
fn calc_stable(args: &[Value], site: CallSite, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    let x = args[0].as_f64()?;
    let filter = args[1].as_f64()?;
    let dt = ctx.dt;
    let slot = ctx.state.entry(site);
    let prev = match slot {
        OpState::ChangeBy { prev_x, held, .. } => Some((*prev_x, *held)),
        _ => None,
    };
    let (reference, held, output) = match prev {
        None => (x, 0.0, false),
        Some((reference, held)) => {
            if x != reference {
                (x, 0.0, false)
            } else {
                let held = held + dt;
                (reference, held, held >= filter)
            }
        }
    };
    *slot = OpState::ChangeBy {
        prev_x: reference,
        held,
        pending: output,
    };
    Ok(Value::Bool(output))
}

/// `Calculate.Between`/`Beyond`(x, min, max, filter). The instantaneous predicate
/// (`in range` for Between, `out of range` for Beyond) must hold for `>= filter`
/// seconds before the output goes true; any lapse drops it and restarts timing.
fn calc_between(
    args: &[Value],
    site: CallSite,
    ctx: &mut EvalCtx,
    beyond: bool,
) -> Result<Value, EvalError> {
    let x = args[0].as_f64()?;
    let min = args[1].as_f64()?;
    let max = args[2].as_f64()?;
    let filter = args[3].as_f64()?;
    let dt = ctx.dt;
    let in_range = x >= min && x <= max;
    let cond = if beyond { !in_range } else { in_range };
    Ok(Value::Bool(timed_predicate(cond, filter, dt, ctx, site)))
}

/// `Calculate.Hysteresis(x, low, high, filter)` — a timed Schmitt trigger. Goes
/// true once `x >= high` has held for `>= filter`; false once `x <= low` has held
/// for `>= filter`; holds its state in the dead band. The held-state output and
/// the timed candidate share the `Timed` slot.
fn calc_hysteresis(args: &[Value], site: CallSite, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    let x = args[0].as_f64()?;
    let low = args[1].as_f64()?;
    let high = args[2].as_f64()?;
    let filter = args[3].as_f64()?;
    let dt = ctx.dt;
    // Candidate target: above-high wants true, below-low wants false, else hold.
    let want = if x >= high {
        Some(true)
    } else if x <= low {
        Some(false)
    } else {
        None
    };
    let slot = ctx.state.entry(site);
    let (mut output, candidate, held) = match slot {
        OpState::Timed { output, candidate, held } => (*output, *candidate, *held),
        _ => (false, false, 0.0),
    };
    let (candidate, held) = match want {
        // In the dead band: stop timing, hold the output.
        None => (candidate, 0.0),
        Some(target) => {
            if target == output {
                // Already there; nothing to time.
                (target, 0.0)
            } else if target == candidate {
                // Continuing to push toward the new state: accumulate.
                let held = held + dt;
                if held >= filter {
                    output = target;
                }
                (candidate, held)
            } else {
                // A fresh push toward `target`: start timing.
                (target, dt)
            }
        }
    };
    *slot = OpState::Timed { output, candidate, held };
    Ok(Value::Bool(output))
}

/// The shared accept-after-stable timing kernel: the instantaneous `cond` must
/// hold for `>= filter` seconds before the output latches true; any lapse drops
/// the output and restarts timing. Used by the timed `Calculate` predicates.
fn timed_predicate(cond: bool, filter: f64, dt: f64, ctx: &mut EvalCtx, site: CallSite) -> bool {
    let slot = ctx.state.entry(site);
    let (candidate, held) = match slot {
        OpState::Timed { candidate, held, .. } => (*candidate, *held),
        _ => (false, 0.0),
    };
    let (output, candidate, held) = if cond != candidate {
        // Candidate changed: restart timing. Output is false unless cond already
        // satisfied long enough (it just started, so false).
        (false, cond, 0.0)
    } else if cond {
        let held = held + dt;
        (held >= filter, cond, held)
    } else {
        // cond false and stable: output false.
        (false, cond, held + dt)
    };
    *slot = OpState::Timed { output, candidate, held };
    output
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

    // ---- Task 18: Delay.* ----

    fn b(v: bool) -> Value {
        Value::Bool(v)
    }

    fn delay_rising(h: &mut Harness, cond: bool, delay: f64) -> bool {
        h.tick("Delay", "Rising", &[b(cond), Value::Float(delay)]).as_bool().unwrap()
    }

    fn delay_falling(h: &mut Harness, cond: bool, delay: f64) -> bool {
        h.tick("Delay", "Falling", &[b(cond), Value::Float(delay)]).as_bool().unwrap()
    }

    #[test]
    fn delay_rising_delays_only_the_rising_edge() {
        // dt = 0.1, delay = 0.3. Output goes true after cond held true 0.3 s,
        // and drops immediately on falling.
        let mut h = Harness::new(0.1);
        assert!(!delay_rising(&mut h, true, 0.3)); // T1 seed: false
        assert!(!delay_rising(&mut h, true, 0.3)); // held 0.1
        assert!(!delay_rising(&mut h, true, 0.3)); // held 0.2
        assert!(delay_rising(&mut h, true, 0.3)); //  held 0.3 -> true
        assert!(!delay_rising(&mut h, false, 0.3)); // immediate fall
        assert!(!delay_rising(&mut h, true, 0.3)); // re-arming, held 0.1
    }

    #[test]
    fn delay_falling_delays_only_the_falling_edge() {
        // dt = 0.1, delay = 0.2.
        let mut h = Harness::new(0.1);
        assert!(delay_falling(&mut h, true, 0.2)); // T1 seed: true (default high)
        assert!(delay_falling(&mut h, false, 0.2)); // held-false 0.1, still true
        assert!(!delay_falling(&mut h, false, 0.2)); // held 0.2 -> false
        assert!(!delay_falling(&mut h, false, 0.2)); // stays false
        assert!(delay_falling(&mut h, true, 0.2)); //  immediate rise
    }

    // ---- Task 18: Debounce.* ----

    #[test]
    fn debounce_stable_commits_after_filter_time() {
        // dt = 0.1, filter = 0.2.
        let mut h = Harness::new(0.1);
        let d = |h: &mut Harness, c: bool| {
            h.tick("Debounce", "Stable", &[b(c), Value::Float(0.2)]).as_bool().unwrap()
        };
        assert!(!d(&mut h, false)); // seed output false
        assert!(!d(&mut h, true)); //  new candidate true, held 0
        assert!(!d(&mut h, true)); //  held 0.1
        assert!(d(&mut h, true)); //   held 0.2 -> commit true
        assert!(d(&mut h, false)); //  new candidate false, output holds true
    }

    #[test]
    fn debounce_filter_applies_hysteresis() {
        // dt = 0.1, response tc = 0.1 -> a0 = 0.5. Filter level rises toward 1.
        // Seeded to the first input (false -> level 0).
        let mut h = Harness::new(0.1);
        let d = |h: &mut Harness, c: bool| {
            h.tick("Debounce", "Filter", &[b(c), Value::Float(0.1)]).as_bool().unwrap()
        };
        assert!(!d(&mut h, false)); // seed level 0, output false
        // level: 0.5, 0.75, 0.875 -> crosses 0.8 on the third true tick.
        assert!(!d(&mut h, true)); // level 0.5  (<0.8, holds false)
        assert!(!d(&mut h, true)); // level 0.75 (<0.8, holds false)
        assert!(d(&mut h, true)); //  level 0.875 (>=0.8 -> true)
        // Now drop: level 0.4375, 0.21875, 0.109 -> crosses 0.2 to go false.
        assert!(d(&mut h, false)); // level 0.4375 (>0.2, holds true)
        assert!(d(&mut h, false)); // level 0.21875 (>0.2, holds true)
        assert!(!d(&mut h, false)); // level ~0.109 (<=0.2 -> false)
    }

    // ---- Task 18: Change.* ----

    #[test]
    fn change_to_pulses_on_rising_edge() {
        let mut h = Harness::new(0.1);
        let c = |h: &mut Harness, v: bool| h.tick("Change", "To", &[b(v)]).as_bool().unwrap();
        assert!(!c(&mut h, false)); // seed
        assert!(c(&mut h, true)); //  rising -> pulse
        assert!(!c(&mut h, true)); // steady high
        assert!(!c(&mut h, false)); // falling -> no pulse for To
        assert!(c(&mut h, true)); //  rising -> pulse
    }

    #[test]
    fn change_from_pulses_on_falling_edge() {
        let mut h = Harness::new(0.1);
        let c = |h: &mut Harness, v: bool| h.tick("Change", "From", &[b(v)]).as_bool().unwrap();
        assert!(!c(&mut h, true)); // seed high
        assert!(c(&mut h, false)); // falling -> pulse
        assert!(!c(&mut h, true)); // rising -> no pulse for From
    }

    #[test]
    fn change_either_pulses_on_any_edge() {
        let mut h = Harness::new(0.1);
        let c = |h: &mut Harness, v: bool| h.tick("Change", "Either", &[b(v)]).as_bool().unwrap();
        assert!(!c(&mut h, false)); // seed
        assert!(c(&mut h, true)); //  edge
        assert!(c(&mut h, false)); // edge
        assert!(!c(&mut h, false)); // no edge
    }

    #[test]
    fn change_by_pulses_on_magnitude_change() {
        let mut h = Harness::new(0.1);
        let c = |h: &mut Harness, x: f64| {
            h.tick("Change", "By", &[Value::Float(x), Value::Float(5.0)]).as_bool().unwrap()
        };
        assert!(!c(&mut h, 0.0)); // seed
        assert!(!c(&mut h, 2.0)); // delta 2 < 5
        assert!(c(&mut h, 8.0)); //  delta 6 >= 5
        assert!(!c(&mut h, 8.0)); // delta 0
        assert!(c(&mut h, 1.0)); //  delta 7 >= 5
    }

    #[test]
    fn change_up_and_down_are_directional() {
        let mut h = Harness::new(0.1);
        let up = |h: &mut Harness, x: f64| {
            h.tick("Change", "Up", &[Value::Float(x), Value::Float(5.0)]).as_bool().unwrap()
        };
        assert!(!up(&mut h, 0.0)); // seed
        assert!(!up(&mut h, 2.0)); // +2 < 5
        assert!(up(&mut h, 8.0)); //  +6 >= 5
        assert!(!up(&mut h, 1.0)); // went down: not an Up

        let mut h2 = Harness::new(0.1);
        let down = |h: &mut Harness, x: f64| {
            h.tick("Change", "Down", &[Value::Float(x), Value::Float(5.0)]).as_bool().unwrap()
        };
        assert!(!down(&mut h2, 10.0)); // seed
        assert!(down(&mut h2, 3.0)); //  -7 >= 5
        assert!(!down(&mut h2, 8.0)); // went up: not a Down
    }

    #[test]
    fn change_by_filtered_requires_sustained_change() {
        // dt = 0.1, filter = 0.2: the change condition must hold 0.2 s, then one
        // pulse.
        let mut h = Harness::new(0.1);
        let c = |h: &mut Harness, x: f64| {
            h.tick("Change", "By", &[Value::Float(x), Value::Float(5.0), Value::Float(0.2)])
                .as_bool()
                .unwrap()
        };
        assert!(!c(&mut h, 0.0)); // seed (reference = 0)
        // Jump to 9 (delta 9 >= 5). It stays >= delta vs the held reference 0.
        assert!(!c(&mut h, 9.0)); // held 0.1 (< 0.2)
        assert!(c(&mut h, 9.0)); //  held 0.2 -> single pulse
        assert!(!c(&mut h, 9.0)); // already pulsed (armed-spent)
        assert!(!c(&mut h, 9.0)); // still spent
    }

    // ---- Task 18: Timer object ----

    #[test]
    fn timer_counts_down_on_read_and_stops_at_zero() {
        let mut h = Harness::new(0.1);
        let key = CallSite::new("Root.Demo.MyTimer", 0);
        let run = |h: &mut Harness, method: &str, args: &[Value]| -> Value {
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
            timer(method, args, key.clone(), &mut ctx).unwrap().unwrap()
        };
        // Start counting down from 0.25.
        run(&mut h, "Start", &[Value::Float(0.25)]);
        approx(run(&mut h, "Remaining", &[]).as_f64().unwrap(), 0.15);
        approx(run(&mut h, "Remaining", &[]).as_f64().unwrap(), 0.05);
        approx(run(&mut h, "Remaining", &[]).as_f64().unwrap(), 0.0); // clamps
        approx(run(&mut h, "Remaining", &[]).as_f64().unwrap(), 0.0); // stays
    }

    #[test]
    fn timer_stop_and_reset() {
        let mut h = Harness::new(0.1);
        let key = CallSite::new("Root.Demo.MyTimer", 0);
        let run = |h: &mut Harness, method: &str, args: &[Value]| -> Value {
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
            timer(method, args, key.clone(), &mut ctx).unwrap().unwrap()
        };
        run(&mut h, "Start", &[Value::Float(1.0)]);
        run(&mut h, "Stop", &[]);
        // Stopped: reading does not decrement.
        approx(run(&mut h, "Remaining", &[]).as_f64().unwrap(), 1.0);
        run(&mut h, "Reset", &[]);
        approx(run(&mut h, "Remaining", &[]).as_f64().unwrap(), 0.0);
    }

    // ---- Task 18: stateful Calculate predicates ----

    #[test]
    fn calculate_stable_true_after_unchanged_for_filter() {
        let mut h = Harness::new(0.1);
        let s = |h: &mut Harness, x: f64| {
            h.tick("Calculate", "Stable", &[Value::Float(x), Value::Float(0.2)]).as_bool().unwrap()
        };
        assert!(!s(&mut h, 5.0)); // seed
        assert!(!s(&mut h, 5.0)); // held 0.1
        assert!(s(&mut h, 5.0)); //  held 0.2 -> stable
        assert!(!s(&mut h, 7.0)); // changed -> restart
    }

    #[test]
    fn calculate_between_and_beyond_are_timed() {
        let mut h = Harness::new(0.1);
        let bet = |h: &mut Harness, x: f64| {
            h.tick(
                "Calculate",
                "Between",
                &[Value::Float(x), Value::Float(0.0), Value::Float(10.0), Value::Float(0.2)],
            )
            .as_bool()
            .unwrap()
        };
        assert!(!bet(&mut h, 5.0)); // in range, held 0
        assert!(!bet(&mut h, 5.0)); // held 0.1
        assert!(bet(&mut h, 5.0)); //  held 0.2 -> true
        assert!(!bet(&mut h, 50.0)); // out of range -> drops, restart

        let mut h2 = Harness::new(0.1);
        let bey = |h: &mut Harness, x: f64| {
            h.tick(
                "Calculate",
                "Beyond",
                &[Value::Float(x), Value::Float(0.0), Value::Float(10.0), Value::Float(0.2)],
            )
            .as_bool()
            .unwrap()
        };
        assert!(!bey(&mut h2, 50.0)); // out of range, held 0
        assert!(!bey(&mut h2, 50.0)); // held 0.1
        assert!(bey(&mut h2, 50.0)); //  held 0.2 -> true
    }

    #[test]
    fn calculate_hysteresis_schmitt_trigger() {
        // low = 2, high = 8, filter = 0.2, dt = 0.1.
        let mut h = Harness::new(0.1);
        let hy = |h: &mut Harness, x: f64| {
            h.tick(
                "Calculate",
                "Hysteresis",
                &[Value::Float(x), Value::Float(2.0), Value::Float(8.0), Value::Float(0.2)],
            )
            .as_bool()
            .unwrap()
        };
        assert!(!hy(&mut h, 0.0)); // below low, false
        assert!(!hy(&mut h, 10.0)); // above high, timing 0.1
        assert!(hy(&mut h, 10.0)); //  timing 0.2 -> true
        assert!(hy(&mut h, 10.0)); //  stays true
        assert!(hy(&mut h, 1.0)); //   below low, timing 0.1, holds true
        assert!(!hy(&mut h, 1.0)); //  timing 0.2 -> false
        assert!(!hy(&mut h, 1.0)); //  stays false
    }

    // ---- Task 18: static-local persistence ----

    #[test]
    fn static_locals_survive_enter_leave_function() {
        // The Env::statics mechanism backs `static local` persistence: a static
        // set in one function invocation is still present after leave/enter.
        let mut env = Env::new();
        env.enter_function();
        env.set_static("Root.Demo.Update", "accum", Value::Float(2.0));
        env.leave_function();
        env.enter_function();
        assert_eq!(
            env.get_static("Root.Demo.Update", "accum"),
            Some(&Value::Float(2.0))
        );
    }
}
