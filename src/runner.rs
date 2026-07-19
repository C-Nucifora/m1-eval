// SPDX-License-Identifier: GPL-3.0-or-later
//! The runners: the deterministic tick loops that drive a [`Scenario`] over a
//! [`Loaded`] project and produce a [`Trace`].
//!
//! Three runners share one core, **rate-gated** tick loop:
//!
//! - **single-function** ([`RunMode::Function`]): one chosen function executes
//!   each tick;
//! - **dependency-cone** ([`RunMode::Cone`]): the target channel's upstream cone
//!   of functions executes each tick, in topological (writer-before-reader) order;
//! - **whole-project** ([`RunMode::WholeProject`]): every periodically-scheduled
//!   function executes at its **own** rate, in dependency-then-rate order — the
//!   faithful mini-ECU.
//!
//! The grid advances at `base_rate_hz` (tick step `1 / base_rate_hz`). For the
//! single-function and cone runners every function shares that base rate, so it
//! runs every tick with the base step. For the whole-project runner each function
//! runs only on the base ticks its **rate divisor** (the exact integer
//! `base / rate`; an inexact ratio is rejected) selects and is stepped by its
//! **own** period (`dt = 1 / rate`), so a 50 Hz function on
//! a 100 Hz base runs every other tick and integrates with `dt = 0.02`. Functions
//! not run on a tick hold their last-written channels (zero-order hold). When the
//! whole-project scenario pins no `base_rate_hz`, the base defaults to the least
//! common multiple of the scheduled rates, so every function has an exact integer
//! tick period.
//!
//! The loop, in order, each tick:
//!
//! 1. seeds the value store from the scenario inputs (each resampled at `t`),
//!    then layers the scenario overrides on top — both canonicalised to the
//!    target function's scope so `Speed` and `Root.Demo.Speed` address one key;
//! 2. opens the tick in the trace (extends the time axis);
//! 3. executes each scheduled function whose divisor selects this tick, through
//!    [`crate::stmt::exec_script`];
//! 4. holds any scheduled-write channel a function did not run by repeating its
//!    last value, then records the externally-driven inputs and any other
//!    unrecorded channel values, so each column stays aligned to the time axis.
//!
//! Determinism: the grid and per-function `dt` are fixed, inputs are pure
//! functions of `t`, and there is no wall-clock or RNG — the same scenario always
//! yields the same trace.

use crate::counterfactual::Override;
use crate::env::{Env, StateStore};
use crate::error::EvalError;
use crate::expr::EvalCtx;
use crate::ident::{Target, classify};
use crate::loader::Loaded;
use crate::log::Log;
use crate::scenario::{InputSeries, RunMode, Scenario};
use crate::stmt::exec_script;
use crate::summary::io_sets;
use crate::trace::Trace;
use crate::value::Value;
use m1_typecheck::parsed::ParsedScript;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// One scheduled function: the script that backs it plus its resolved scope.
struct Scheduled<'a> {
    /// The parsed script whose body executes.
    script: &'a ParsedScript,
    /// The enclosing group's canonical path (for group-relative resolution).
    group: Option<String>,
    /// The function symbol's canonical path (for `In.*` and static-local keys).
    fn_symbol: Option<String>,
}

/// Run a scenario against a loaded project, producing a [`Trace`].
///
/// Dispatches on the scenario's [`RunMode`]: a single function or a target
/// channel's dependency cone. Fails loud — an unknown target, an unresolved
/// input, or any evaluation error aborts the run rather than producing a
/// partially-guessed trace.
pub fn run(loaded: &Loaded, scenario: &Scenario) -> Result<Trace, EvalError> {
    match &scenario.mode {
        RunMode::Function(name) => {
            // A single function has no schedule rate of its own; it runs every
            // base tick (divisor 1) with the base `dt`. Wrap it as a rated
            // schedule at the scenario's base rate so the one generalised loop
            // handles all three modes uniformly.
            let base = require_base_rate(scenario)?;
            let scheduled = resolve_function(loaded, name)?;
            let rated = vec![ScheduledRated {
                sched: scheduled,
                rate_hz: base,
            }];
            tick_loop(loaded, scenario, &rated, base)
        }
        RunMode::Cone(target) => {
            let base = require_base_rate(scenario)?;
            let order = build_cone(loaded, target)?;
            let rated: Vec<ScheduledRated> = order
                .into_iter()
                .map(|sched| ScheduledRated {
                    sched,
                    rate_hz: base,
                })
                .collect();
            tick_loop(loaded, scenario, &rated, base)
        }
        RunMode::WholeProject => {
            // Enumerate every periodically-scheduled function, ordered
            // dependency-then-rate, then drive the rate-gated tick loop: each
            // function runs only on the base ticks its rate divisor selects, with
            // its own period as `dt`.
            let ordered = build_whole_project_schedule(loaded);
            let base = resolve_base_rate(scenario, &ordered)?;
            tick_loop(loaded, scenario, &ordered, base)
        }
    }
}

/// The base tick rate for a mode that pins one explicitly (`function`/`cone`).
/// These modes carry no schedule to derive a default from, so a positive
/// `base_rate_hz` is required (the scenario parser already enforces this; this is
/// a defensive belt-and-braces check).
fn require_base_rate(scenario: &Scenario) -> Result<f64, EvalError> {
    if scenario.base_rate_hz > 0.0 {
        Ok(scenario.base_rate_hz)
    } else {
        Err(EvalError::UnsupportedConstruct {
            kind: format!(
                "base_rate_hz must be positive, got {}",
                scenario.base_rate_hz
            ),
            at: 0,
        })
    }
}

/// Resolve the whole-project base tick rate. When the scenario pins a positive
/// `base_rate_hz` it is used verbatim (the tick loop then rejects it unless
/// every scheduled rate divides it exactly); when it is absent (0.0, the "auto"
/// sentinel) the base is the **least common multiple** of the scheduled rates,
/// so every function has an exact integer tick period — e.g. rates {500, 200}
/// yield a 1000 Hz base, never a rounded 2.5-tick period. An empty schedule
/// with no pinned base has no rate to derive — fail loud.
fn resolve_base_rate(scenario: &Scenario, schedule: &[ScheduledRated]) -> Result<f64, EvalError> {
    if scenario.base_rate_hz > 0.0 {
        return Ok(scenario.base_rate_hz);
    }
    if schedule.is_empty() {
        return Err(EvalError::UnsupportedConstruct {
            kind:
                "whole-project run has no scheduled functions and no base_rate_hz to default from"
                    .to_string(),
            at: 0,
        });
    }
    let mut lcm_mhz: u64 = 1;
    for r in schedule {
        let mhz = millihertz(r.rate_hz).ok_or_else(|| EvalError::UnsupportedConstruct {
            kind: format!(
                "cannot schedule rate {} Hz exactly (not representable in whole millihertz)",
                r.rate_hz
            ),
            at: 0,
        })?;
        lcm_mhz = lcm_mhz / gcd(lcm_mhz, mhz) * mhz;
        // 1 MHz cap: beyond this the tick grid explodes; ask for an explicit base.
        if lcm_mhz > 1_000_000_000 {
            return Err(EvalError::UnsupportedConstruct {
                kind: "no practical exact common base for the scheduled rates (lcm exceeds \
                       1 MHz); pin base_rate_hz to a rate every scheduled rate divides exactly"
                    .to_string(),
                at: 0,
            });
        }
    }
    Ok(lcm_mhz as f64 / 1000.0)
}

/// A rate as exact integer millihertz, or `None` when it is not representable
/// (non-positive, non-finite, or fractional below 1 mHz). Integer millihertz is
/// the exact arithmetic domain for divisor/LCM computation: project rates are
/// whole Hz in practice, and 1 mHz resolution covers slow event rates without
/// floating-point rounding.
fn millihertz(rate_hz: f64) -> Option<u64> {
    if !rate_hz.is_finite() || rate_hz <= 0.0 {
        return None;
    }
    let m = rate_hz * 1000.0;
    let r = m.round();
    if r >= 1.0 && (m - r).abs() < 1e-6 {
        Some(r as u64)
    } else {
        None
    }
}

/// Greatest common divisor (Euclid). `gcd(a, 0) = a`.
fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// The exact tick divisor for `rate_hz` on a `base_rate_hz` grid, or an error
/// when the base cannot represent the rate exactly. Replaces the old
/// `round(base/rate)` divisor, which silently ran a 200 Hz function every 3
/// ticks of a 500 Hz base (~166.7 Hz) while handing it dt = 5 ms. A rate
/// faster than the base is rejected by the same exactness rule (its divisor
/// would be fractional below 1), so a base below the fastest scheduled rate
/// fails loud instead of clamping to every-tick.
fn exact_divisor(
    base_rate_hz: f64,
    rate_hz: f64,
    fn_symbol: Option<&str>,
) -> Result<usize, EvalError> {
    let who = fn_symbol.unwrap_or("<function>");
    let (base_mhz, rate_mhz) = match (millihertz(base_rate_hz), millihertz(rate_hz)) {
        (Some(b), Some(r)) => (b, r),
        _ => {
            return Err(EvalError::UnsupportedConstruct {
                kind: format!(
                    "cannot schedule {who}: base {base_rate_hz} Hz / rate {rate_hz} Hz \
                     not representable in whole millihertz"
                ),
                at: 0,
            });
        }
    };
    if base_mhz % rate_mhz != 0 {
        return Err(EvalError::UnsupportedConstruct {
            kind: format!(
                "base_rate_hz {base_rate_hz} Hz cannot schedule {who} at {rate_hz} Hz exactly: \
                 {base_rate_hz}/{rate_hz} is not an integer tick period. Use a base every \
                 scheduled rate divides exactly (e.g. their least common multiple), or omit \
                 base_rate_hz to derive one automatically"
            ),
            at: 0,
        });
    }
    Ok((base_mhz / rate_mhz) as usize)
}

/// One scheduled function together with its periodic execution rate in Hz, as
/// derived from the function symbol's `call_rate_hz`. Only functions with a
/// resolvable periodic trigger (a `BuiltIn.EventKernel` clock like `On 100Hz`)
/// appear here; `On Startup` / untriggered functions (rate `None`) are excluded.
struct ScheduledRated<'a> {
    /// The scheduled function (script + resolved scope).
    sched: Scheduled<'a>,
    /// The function's execution rate in Hz.
    rate_hz: f64,
}

/// Build the whole-project schedule: every periodically-scheduled function, in
/// dependency-then-rate order.
///
/// 1. **Enumerate + rate** (Task 11): for each script, the backing function
///    symbol's `call_rate_hz` gives its periodic rate. Keep only the functions
///    with `Some(rate)` — exactly the pattern `m1-typecheck`'s `schedule.rs`
///    uses (`symbols().get(&fn_path).and_then(|s| s.call_rate_hz)`). Startup /
///    untriggered functions (`None`) are not periodically scheduled, so they are
///    excluded.
/// 2. **Dependency-then-rate order** (Task 12): within a single rate group,
///    writer-before-reader topological order (from [`io_sets`], reusing
///    [`topo_order`]); groups concatenated fastest-rate-first. There are no
///    cross-rate edges — a faster reader of a slower writer sees the previous
///    value (the same same-rate-only dependency rule `m1-typecheck`'s
///    `schedule.rs` applies).
fn build_whole_project_schedule(loaded: &Loaded) -> Vec<ScheduledRated<'_>> {
    let mut rated = enumerate_scheduled(loaded);
    order_by_dependency_then_rate(loaded, &mut rated);
    rated
}

/// Enumerate every periodically-scheduled function with its rate (Task 11).
///
/// For each parsed script, resolve its backing function symbol and read that
/// symbol's `call_rate_hz`; keep only functions with a periodic rate. The result
/// is sorted by `(rate descending, fn_symbol)` as a deterministic baseline; the
/// dependency layer (Task 12) refines order *within* each rate group.
fn enumerate_scheduled(loaded: &Loaded) -> Vec<ScheduledRated<'_>> {
    let mut rated: Vec<ScheduledRated> = loaded
        .scripts
        .iter()
        .filter_map(|script| {
            let fn_symbol = loaded.project.function_symbol_for_script(&script.name)?;
            let rate_hz = loaded
                .project
                .symbols()
                .get(&fn_symbol)
                .and_then(|s| s.call_rate_hz)?;
            Some(ScheduledRated {
                sched: scheduled_for(loaded, script),
                rate_hz,
            })
        })
        .collect();
    // Deterministic baseline: fastest rate first, ties broken by function symbol
    // path (every scheduled function has a `fn_symbol`, so the key is total).
    rated.sort_by(|a, b| {
        b.rate_hz
            .partial_cmp(&a.rate_hz)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.sched.fn_symbol.cmp(&b.sched.fn_symbol))
    });
    rated
}

/// Reorder an already rate-sorted schedule into dependency-then-rate order
/// (Task 12).
///
/// Within each rate group, a writer runs before any reader of its output: build
/// writer→reader edges from [`io_sets`] (restricted to that rate group) and
/// [`topo_order`] them. Groups are then concatenated fastest-rate-first — the
/// conventional ECU order, fast loops before slow within a base tick.
///
/// Cross-rate dependencies deliberately add **no** edges: a faster function that
/// reads a slower function's channel sees the value from the slower function's
/// previous run (stale between writer ticks). This mirrors `m1-typecheck`'s
/// `schedule.rs`, whose dependency edges are same-rate only; forcing a fast loop
/// to wait on a slow one would misrepresent the real ECU schedule.
///
/// A dependency cycle within a rate group falls back to discovery order (the
/// existing [`topo_order`] behaviour) — acceptable, since ordering is best-effort
/// and values are never guessed.
fn order_by_dependency_then_rate(loaded: &Loaded, rated: &mut Vec<ScheduledRated<'_>>) {
    // Group indices by rate, preserving the fastest-first order of the first
    // appearance of each rate (the input is already rate-sorted descending).
    let mut groups: Vec<(f64, Vec<usize>)> = Vec::new();
    for (i, r) in rated.iter().enumerate() {
        match groups.iter_mut().find(|(rate, _)| *rate == r.rate_hz) {
            Some((_, idxs)) => idxs.push(i),
            None => groups.push((r.rate_hz, vec![i])),
        }
    }

    // Compute the final order of indices: for each rate group, topo-order its
    // members writer-before-reader using only that group's writers (no cross-rate
    // edges), then concatenate the groups fastest-first.
    let mut final_order: Vec<usize> = Vec::with_capacity(rated.len());
    for (_rate, idxs) in &groups {
        // Map each script name in this group to its index, its io sets, and the
        // group-local writer map (channel -> first script that writes it).
        let mut name_to_idx: BTreeMap<String, usize> = BTreeMap::new();
        let mut sets: HashMap<String, crate::summary::IoSets> = HashMap::new();
        let mut writer: HashMap<String, String> = HashMap::new();
        let mut nodes: BTreeSet<String> = BTreeSet::new();
        // Stable per-group ordering by script name for determinism.
        let mut group_names: Vec<(String, usize)> = idxs
            .iter()
            .map(|&i| (rated[i].sched.script.name.clone(), i))
            .collect();
        group_names.sort();
        for (name, i) in &group_names {
            let script = rated[*i].sched.script;
            let group = rated[*i].sched.group.as_deref();
            let io = io_sets(script, &loaded.project, group);
            for w in &io.writes {
                writer.entry(w.clone()).or_insert_with(|| name.clone());
            }
            sets.insert(name.clone(), io);
            name_to_idx.insert(name.clone(), *i);
            nodes.insert(name.clone());
        }

        // Writer→reader edges, restricted to this rate group (cross-rate reads
        // intentionally add no edge — see the doc comment).
        let mut edges: Vec<(String, String)> = Vec::new();
        for (name, io) in &sets {
            for r in &io.reads {
                if let Some(producer) = writer.get(r)
                    && producer != name
                {
                    edges.push((producer.clone(), name.clone()));
                }
            }
        }

        for name in topo_order(&nodes, &edges) {
            if let Some(&i) = name_to_idx.get(&name) {
                final_order.push(i);
            }
        }
    }

    // Apply `final_order` by draining `rated` into a lookup and re-pushing.
    let mut slots: Vec<Option<ScheduledRated>> = rated.drain(..).map(Some).collect();
    for i in final_order {
        if let Some(r) = slots[i].take() {
            rated.push(r);
        }
    }
}

/// The shared deterministic, **rate-gated** tick loop over an ordered list of
/// rated scheduled functions.
///
/// The grid advances at `base_rate_hz` (tick step `1 / base_rate_hz`). Each
/// scheduled function runs only on the base ticks its **rate divisor** selects —
/// the exact integer `base_rate_hz / rate_hz` (a base that cannot represent a
/// rate exactly is rejected, see [`exact_divisor`]) — and when it runs it is handed its
/// **own** period as `dt = 1 / rate_hz`, the time elapsed since *its* last run,
/// not the base step. So a 50 Hz function on a 100 Hz base runs every other tick
/// and its stateful operators (e.g. `Integral.Normal`) integrate with `dt = 0.02`.
///
/// Functions not run on a given tick leave their last-written channels untouched
/// in the shared [`Env`] — a zero-order hold — so the per-tick trace recording
/// naturally repeats the held value until the function next runs.
///
/// Single-function and cone modes pass a schedule whose `rate_hz == base_rate_hz`
/// (divisor 1, `dt` = the base step), so they fall through this loop unchanged.
fn tick_loop(
    loaded: &Loaded,
    scenario: &Scenario,
    schedule: &[ScheduledRated],
    base_rate_hz: f64,
) -> Result<Trace, EvalError> {
    let ticks = tick_count(scenario.duration_s, base_rate_hz);

    // Precompute each function's rate divisor (how many base ticks between runs)
    // and its per-run dt (its own period). Divisors are exact integers — a base
    // that cannot represent a scheduled rate exactly is rejected loudly rather
    // than rounded (the old `round(base/rate)` ran a 200 Hz function at
    // ~166.7 Hz on a 500 Hz base while handing it dt = 5 ms).
    let plans: Vec<RunPlan> = schedule
        .iter()
        .map(|r| {
            let divisor = exact_divisor(base_rate_hz, r.rate_hz, r.sched.fn_symbol.as_deref())?;
            Ok(RunPlan {
                divisor,
                dt: 1.0 / r.rate_hz,
            })
        })
        .collect::<Result<_, EvalError>>()?;

    let mut env = Env::new();
    let mut state = StateStore::new();
    let mut trace = Trace::new();

    // In **whole-project** mode there is no scenario driving the sensor/CAN inputs
    // and no calibration seeding the tunables, so an unseeded *channel* read falls
    // back to its type-correct startup default (flagged externally driven) rather
    // than aborting the run — the channel-side analogue of the Tier-3 IO stubs.
    // This covers every unseeded read uniformly: a hardware sensor channel, a CAN
    // signal, a table-output `.Value` the auto-`Lookup` would compute, and a state
    // channel read before its writer's first run. It propagates to inline
    // user-function callees (they share this env). In `function`/`cone` mode the
    // flag stays `false`, so a read of an unprovided input still fails loud — the
    // scenario must drive every channel a single function reads.
    env.default_unseeded_channels = matches!(scenario.mode, RunMode::WholeProject);

    // The union of every channel the schedule writes, computed once: on a tick a
    // function holds (does not run), we repeat its last value from `env` for these
    // channels so the trace stays a dense grid (zero-order hold).
    let scheduled_writes = schedule_writes(loaded, schedule);

    // Canonicalise each scenario input/override channel once, against the first
    // scheduled function's scope (all scheduled functions share the project; the
    // group only affects relative names, and inputs are normally absolute).
    let scope_group = schedule.first().and_then(|s| s.sched.group.as_deref());
    let scope_fn = schedule.first().and_then(|s| s.sched.fn_symbol.as_deref());
    let inputs = canonicalise(&scenario.inputs, loaded, scope_group, scope_fn);
    let overrides = canonicalise(&scenario.overrides, loaded, scope_group, scope_fn);

    for i in 0..ticks {
        let t = i as f64 / base_rate_hz;

        // 1. Seed inputs (resampled at t), then layer overrides on top.
        for (path, series) in &inputs {
            env.set(path.clone(), series.sample(t));
        }
        for (path, series) in &overrides {
            env.set(path.clone(), series.sample(t));
        }

        // 2. Open the tick.
        trace.push_tick(t);

        // 3. Run each scheduled function whose divisor selects this tick, in
        //    dependency-then-rate order, sharing env/state. A function not run
        //    holds its last-written channels in `env` (zero-order hold).
        for (rated, plan) in schedule.iter().zip(plans.iter()) {
            if i % plan.divisor != 0 {
                continue;
            }
            let sched = &rated.sched;
            let root = sched.script.cst.root();
            let mut ctx = EvalCtx {
                project: &loaded.project,
                calib: &loaded.calib,
                env: &mut env,
                state: &mut state,
                group: sched.group.as_deref(),
                fn_symbol: sched.fn_symbol.as_deref(),
                script_name: &sched.script.name,
                dt: plan.dt,
                scripts: &loaded.scripts,
                depth: 0,
                trace: Some(&mut trace),
            };
            exec_script(&root, &mut ctx)?;
        }

        // 4. Record any channel a scheduled function *holds* this tick (it did not
        //    run, so the executor wrote nothing) by repeating its current env
        //    value, plus the seeded inputs/overrides. This keeps every column
        //    aligned to the time axis with the zero-order-hold value. Two passes:
        //    the static scheduled-write set, then every channel that already has a
        //    trace column — the latter catches channels written only by *inline*
        //    user-function callees (e.g. a `Service Bits.Update` push, whose backing
        //    function carries no schedule rate), which are not in the static set but
        //    must still hold dense once they have appeared.
        hold_unwritten_channels(&scheduled_writes, &env, &mut trace);
        hold_trace_channels(&env, &mut trace);
        for (path, series) in inputs.iter().chain(overrides.iter()) {
            // Only record if the executor did not already record this channel this
            // tick (assignment targets are recorded by the statement executor).
            let already = trace
                .channels
                .get(path)
                .map(|c| c.len() == trace.time.len())
                .unwrap_or(false);
            if !already {
                let v = env.get(path).cloned().unwrap_or_else(|| series.sample(t));
                trace.record_channel(path.clone(), v);
                trace.mark_external(path.clone());
            }
        }
    }

    Ok(trace)
}

/// One function's per-tick execution plan: how many base ticks between runs and
/// the per-run time step (its own period).
struct RunPlan {
    /// The exact integer `base_rate_hz / rate_hz` (from [`exact_divisor`]) — the
    /// function runs on every base tick `i` where `i % divisor == 0`.
    divisor: usize,
    /// `1 / rate_hz` — the time since this function's previous run, handed to its
    /// stateful operators so accumulation is rate-correct.
    dt: f64,
}

/// The union of every channel any scheduled function writes (canonical paths),
/// from each function's [`io_sets`]. Used to hold a function's output at its last
/// value on the base ticks it does not run (zero-order hold).
fn schedule_writes(loaded: &Loaded, schedule: &[ScheduledRated]) -> BTreeSet<String> {
    let mut writes = BTreeSet::new();
    for rated in schedule {
        let io = io_sets(
            rated.sched.script,
            &loaded.project,
            rated.sched.group.as_deref(),
        );
        for w in io.writes {
            writes.insert(w);
        }
    }
    writes
}

/// For each scheduled-write channel not already recorded this tick (because the
/// owning function held rather than ran), repeat its current `env` value so the
/// trace column stays aligned to the time axis. A channel with no env value yet
/// (never written) is skipped — it simply has no column until first written.
fn hold_unwritten_channels(writes: &BTreeSet<String>, env: &Env, trace: &mut Trace) {
    for path in writes {
        let already = trace
            .channels
            .get(path)
            .map(|c| c.len() == trace.time.len())
            .unwrap_or(false);
        if already {
            continue;
        }
        if let Some(v) = env.get(path).cloned() {
            trace.record_channel(path.clone(), v);
        }
    }
}

/// Zero-order-hold every channel that already has a trace column but was not
/// updated this tick, repeating its current `env` value. Unlike
/// [`hold_unwritten_channels`] (the static scheduled-write set), this holds
/// channels written only by *inline* user-function callees — whose backing
/// functions carry no schedule rate, so they are not in the static set — once they
/// have first appeared, keeping every column dense over the tick grid.
fn hold_trace_channels(env: &Env, trace: &mut Trace) {
    let len = trace.time.len();
    // Collect the lagging channels first to avoid borrowing `trace.channels` while
    // recording into it.
    let lagging: Vec<String> = trace
        .channels
        .iter()
        .filter(|(_, col)| col.len() < len)
        .map(|(path, _)| path.clone())
        .collect();
    for path in lagging {
        if let Some(v) = env.get(&path).cloned() {
            trace.record_channel(path, v);
        }
    }
}

/// The number of ticks spanning `[0, duration_s)` at `base_rate_hz`. A
/// half-second run at 100 Hz is 50 ticks (t = 0.00 .. 0.49). Rounds the product
/// to the nearest integer first to absorb float error (e.g. `0.5 * 100`), so a
/// clean grid yields the expected count.
fn tick_count(duration_s: f64, base_rate_hz: f64) -> usize {
    let n = (duration_s * base_rate_hz).round();
    if n <= 0.0 { 0 } else { n as usize }
}

/// Canonicalise each scenario [`InputSeries`] channel to its project-symbol path
/// so `Speed` and `Root.Demo.Speed` seed the same value-store key. A channel that
/// does not resolve to a project symbol is kept verbatim (it may be a scenario-fed
/// IO key or a not-yet-declared channel), so nothing is silently dropped.
fn canonicalise<'a>(
    series: &'a [InputSeries],
    loaded: &Loaded,
    group: Option<&str>,
    fn_symbol: Option<&str>,
) -> Vec<(String, &'a InputSeries)> {
    let no_locals = HashMap::new();
    series
        .iter()
        .map(|s| {
            let canon = match classify(&s.channel, group, fn_symbol, &loaded.project, &no_locals) {
                Target::Symbol(p) => p,
                _ => s.channel.clone(),
            };
            (canon, s)
        })
        .collect()
}

/// Resolve a function-mode target name to its scheduled function. The name may be
/// a script basename (`Demo.Update.m1scr`), the `Foo.Update` stem, or the
/// canonical `Root.Foo.Update` symbol path.
fn resolve_function<'a>(loaded: &'a Loaded, name: &str) -> Result<Scheduled<'a>, EvalError> {
    // First try an exact script-basename match.
    let script = loaded
        .scripts
        .iter()
        .find(|s| s.name == name)
        // Then a `<stem>.m1scr` match (so `Demo.Update` finds `Demo.Update.m1scr`).
        .or_else(|| {
            let target = format!("{name}.m1scr");
            loaded.scripts.iter().find(|s| s.name == target)
        })
        // Then map a canonical function path back to its backing script via the
        // project's filename association.
        .or_else(|| {
            loaded.scripts.iter().find(|s| {
                loaded
                    .project
                    .function_symbol_for_script(&s.name)
                    .as_deref()
                    == Some(name)
            })
        })
        .ok_or_else(|| EvalError::UnresolvedSymbol {
            name: format!("function {name:?} (no backing script found)"),
        })?;

    Ok(scheduled_for(loaded, script))
}

/// Build a [`Scheduled`] for a script: resolve its group and function symbol from
/// the project by the script's file name.
fn scheduled_for<'a>(loaded: &'a Loaded, script: &'a ParsedScript) -> Scheduled<'a> {
    let group = loaded.project.group_for_script(&script.name);
    let fn_symbol = loaded.project.function_symbol_for_script(&script.name);
    Scheduled {
        script,
        group,
        fn_symbol,
    }
}

/// Build the dependency cone for a target channel: the set of functions needed to
/// compute it, in topological (writer-before-reader) order.
///
/// The writer map (`channel -> function script name`) comes from each script's
/// [`io_sets`] writes. Starting from the target channel, we walk upstream: the
/// function that writes the target, then the functions that write *that*
/// function's reads, transitively. The needed functions are then topologically
/// ordered so a writer runs before any reader of its output. A dependency cycle
/// (a writes b, b writes a) cannot be ordered cleanly; we then fall back to the
/// discovery order (and the run still proceeds — fail-soft on ordering only, not
/// on values).
fn build_cone<'a>(loaded: &'a Loaded, target: &str) -> Result<Vec<Scheduled<'a>>, EvalError> {
    // Canonicalise the target channel against the project (no scope: absolute).
    let no_locals = HashMap::new();
    let target_canon = match classify(target, None, None, &loaded.project, &no_locals) {
        Target::Symbol(p) => p,
        _ => target.to_string(),
    };

    // Per-script io sets, plus the writer map: channel -> script that writes it.
    // A script's group is resolved for relative-name canonicalisation.
    let mut sets: HashMap<String, crate::summary::IoSets> = HashMap::new();
    let mut writer: HashMap<String, String> = HashMap::new();
    for script in &loaded.scripts {
        let group = loaded.project.group_for_script(&script.name);
        let io = io_sets(script, &loaded.project, group.as_deref());
        for w in &io.writes {
            // First writer wins for determinism; scripts are in sorted order.
            writer
                .entry(w.clone())
                .or_insert_with(|| script.name.clone());
        }
        sets.insert(script.name.clone(), io);
    }

    // No in-project writer for the target: it is an external/leaf channel. There
    // is nothing to schedule — fail loud so the user knows the target is not a
    // computed channel in this project.
    let Some(root_writer) = writer.get(&target_canon).cloned() else {
        return Err(EvalError::UnresolvedSymbol {
            name: format!("no function writes target channel {target_canon:?}"),
        });
    };

    // Walk upstream from the root writer, collecting the needed scripts and the
    // dependency edges (dependency -> dependent) for the topological sort.
    let mut needed: BTreeSet<String> = BTreeSet::new();
    let mut edges: Vec<(String, String)> = Vec::new();
    let mut stack = vec![root_writer.clone()];
    while let Some(script_name) = stack.pop() {
        if !needed.insert(script_name.clone()) {
            continue;
        }
        let Some(io) = sets.get(&script_name) else {
            continue;
        };
        for r in &io.reads {
            if let Some(producer) = writer.get(r)
                && producer != &script_name
            {
                // producer must run before script_name.
                edges.push((producer.clone(), script_name.clone()));
                stack.push(producer.clone());
            }
        }
    }

    let ordered = topo_order(&needed, &edges);
    Ok(ordered
        .iter()
        .filter_map(|name| loaded.scripts.iter().find(|s| &s.name == name))
        .map(|s| scheduled_for(loaded, s))
        .collect())
}

/// Build the **downstream** dependency cone for a set of override channels: the
/// set of functions that must be recomputed when those channels change, in
/// topological (writer-before-reader) order.
///
/// This is the forward mirror of [`build_cone`]. Where `build_cone` walks the
/// writer map *backward* (target channel → its writer → that writer's reads →
/// their writers, transitively) to gather everything needed to *produce* a
/// channel, `build_downstream_cone` walks *forward*: from each override channel,
/// find every function that **reads** it; each such function's **writes** are now
/// dirty, so every function reading *those* channels must recompute too,
/// transitively. The same [`io_sets`] and [`topo_order`] are reused — opposite
/// direction.
///
/// Fail loud when **no** function reads any override channel: there is nothing
/// downstream to recompute, so the override would have no effect. That is a user
/// error worth surfacing rather than silently returning an empty schedule.
///
/// Consumed by [`run_counterfactual`] (the headline counterfactual runner).
fn build_downstream_cone<'a>(
    loaded: &'a Loaded,
    overrides: &[String],
) -> Result<Vec<Scheduled<'a>>, EvalError> {
    // Canonicalise each override channel against the project (no scope: absolute).
    let no_locals = HashMap::new();
    let override_canon: BTreeSet<String> = overrides
        .iter()
        .map(
            |ch| match classify(ch, None, None, &loaded.project, &no_locals) {
                Target::Symbol(p) => p,
                _ => ch.clone(),
            },
        )
        .collect();

    // Per-script io sets, plus two indices over the writer/reader relation:
    //   writer:  channel -> the script that writes it (first writer wins, for
    //            determinism — scripts are iterated in sorted order);
    //   readers: channel -> every script that reads it (the forward edge we walk).
    let mut sets: HashMap<String, crate::summary::IoSets> = HashMap::new();
    let mut writer: HashMap<String, String> = HashMap::new();
    let mut readers: HashMap<String, Vec<String>> = HashMap::new();
    for script in &loaded.scripts {
        let group = loaded.project.group_for_script(&script.name);
        let io = io_sets(script, &loaded.project, group.as_deref());
        for w in &io.writes {
            writer
                .entry(w.clone())
                .or_insert_with(|| script.name.clone());
        }
        for r in &io.reads {
            readers
                .entry(r.clone())
                .or_default()
                .push(script.name.clone());
        }
        sets.insert(script.name.clone(), io);
    }

    // Seed the work-stack with every function that reads any override channel —
    // the first generation of the forward walk. If none read any override channel
    // there is nothing downstream to recompute: fail loud.
    let mut stack: Vec<String> = Vec::new();
    for ch in &override_canon {
        if let Some(rs) = readers.get(ch) {
            stack.extend(rs.iter().cloned());
        }
    }
    if stack.is_empty() {
        let chans: Vec<&str> = override_canon.iter().map(String::as_str).collect();
        return Err(EvalError::UnresolvedSymbol {
            name: format!("no function reads override channel(s) {chans:?}"),
        });
    }

    // Walk forward, collecting the dirty function set and the dependency edges
    // (writer -> reader) for the topological sort. For each dirty function, every
    // channel it writes is now dirty, so every reader of those channels joins the
    // cone — transitively. We deliberately do NOT cross the override channels
    // themselves: a function that writes an overridden channel is *not* pulled in
    // (its output is being replaced), only readers downstream of the override.
    let mut needed: BTreeSet<String> = BTreeSet::new();
    let mut edges: Vec<(String, String)> = Vec::new();
    while let Some(script_name) = stack.pop() {
        if !needed.insert(script_name.clone()) {
            continue;
        }
        let Some(io) = sets.get(&script_name) else {
            continue;
        };
        for w in &io.writes {
            // The override channels are ground truth; do not chase readers through
            // a channel that is itself being overridden (its writer was excluded).
            if override_canon.contains(w) {
                continue;
            }
            if let Some(rs) = readers.get(w) {
                for reader in rs {
                    if reader != &script_name {
                        // script_name (writes w) must run before reader (reads w).
                        edges.push((script_name.clone(), reader.clone()));
                        stack.push(reader.clone());
                    }
                }
            }
        }
    }

    let ordered = topo_order(&needed, &edges);
    Ok(ordered
        .iter()
        .filter_map(|name| loaded.scripts.iter().find(|s| &s.name == name))
        .map(|s| scheduled_for(loaded, s))
        .collect())
}

// ---- P3-B Task 6: counterfactual replay (log ground truth + cone recompute) ----

/// How a counterfactual run is gridded: the base tick rate and the duration.
///
/// `duration_s == 0.0` is the "auto" sentinel — the run then spans the log's own
/// `duration_s` (its latest keyframe time). The base rate must be positive; a
/// counterfactual has no schedule to derive a default from.
pub struct CounterfactualCfg {
    /// Base tick rate in Hz; the tick step is `dt = 1 / base_rate_hz`.
    pub base_rate_hz: f64,
    /// Total run duration in seconds. `0.0` means "use the log's duration".
    pub duration_s: f64,
}

/// Replay a recorded [`Log`] as ground truth, applying `overrides` and recomputing
/// only the **downstream cone** of the overridden channels.
///
/// The model (the headline Phase-3 feature): every logged channel is held at its
/// logged value (zero-order hold) each tick — that is the ground truth. The user
/// then replaces one or more channels with an [`Override`]; only the functions
/// **downstream** of those channels (their forward dependency cone, from
/// `build_downstream_cone`) recompute, so the override propagates while every
/// non-cone channel stays at its logged value. The result is a normal [`Trace`].
///
/// **Source precedence (lowest to highest): calibration < log < override.** A
/// calibration value seeds a tunable; the log overwrites any logged channel with
/// its recorded value; an override overwrites the channel it targets last of all.
/// Within a tick the order is: seed every logged channel from the log, then layer
/// the overrides (a constant directly; an expression evaluated against the
/// just-seeded *logged* snapshot, so `CH = CH * 1.05` reads the logged `CH`),
/// then run only the cone functions in writer-before-reader order.
///
/// Determinism: the grid and per-tick `dt` are fixed, the log is a pure function
/// of `t` (zero-order hold), and there is no wall-clock or RNG — the same log and
/// the same overrides always yield the same trace.
///
/// Fails loud: a positive base rate is required; an override whose channel has no
/// in-project reader has nothing to recompute (propagated from
/// `build_downstream_cone`); an unparsable or non-numeric expression override
/// surfaces its evaluation error.
pub fn run_counterfactual(
    loaded: &Loaded,
    log: &Log,
    overrides: &[Override],
    cfg: &CounterfactualCfg,
) -> Result<Trace, EvalError> {
    if cfg.base_rate_hz <= 0.0 {
        return Err(EvalError::UnsupportedConstruct {
            kind: format!(
                "counterfactual base_rate_hz must be positive, got {}",
                cfg.base_rate_hz
            ),
            at: 0,
        });
    }
    let base_rate_hz = cfg.base_rate_hz;
    let duration_s = if cfg.duration_s > 0.0 {
        cfg.duration_s
    } else {
        log.duration_s()
    };

    // Build the downstream cone of the override channels: the only functions that
    // recompute. Fails loud when a (non-empty) override targets a channel no
    // function reads. With NO overrides, however, there is nothing to recompute:
    // the cone is empty and the tick loop reproduces the log verbatim — the
    // documented no-op invariant (`--log` with no `--override`). Guard the empty
    // case so `build_downstream_cone`'s empty-seed fail-loud is reserved for a real
    // no-reader override.
    let override_channels: Vec<String> =
        overrides.iter().map(|o| o.channel().to_string()).collect();
    let cone = if override_channels.is_empty() {
        Vec::new()
    } else {
        build_downstream_cone(loaded, &override_channels)?
    };

    // Canonicalise the log series and each override channel against the project
    // scope (the cone's first function), so `Sensor` and `Root.CF.Sensor` address
    // one value-store key — the same canonicalisation the tick loop applies.
    let scope_group = cone.first().and_then(|s| s.group.as_deref());
    let scope_fn = cone.first().and_then(|s| s.fn_symbol.as_deref());
    let log_inputs = canonicalise(&log.channels, loaded, scope_group, scope_fn);
    let prepared = prepare_overrides(overrides, loaded, scope_group, scope_fn)?;

    // The union of channels the cone writes, so a cone function that does not run
    // on a tick holds its last value (zero-order hold) — reusing the schedule-write
    // hold machinery the tick loop already relies on.
    let rated: Vec<ScheduledRated> = cone
        .into_iter()
        .map(|sched| ScheduledRated {
            sched,
            rate_hz: base_rate_hz,
        })
        .collect();
    let scheduled_writes = schedule_writes(loaded, &rated);

    let ticks = tick_count(duration_s, base_rate_hz);
    let mut env = Env::new();
    let mut state = StateStore::new();
    let mut trace = Trace::new();
    // A counterfactual seeds every logged channel as ground truth, so an unseeded
    // *channel* read still fails loud (like function/cone mode): a cone function
    // reading a channel the log does not carry is a genuine error, not a guess.
    env.default_unseeded_channels = false;

    for i in 0..ticks {
        let t = i as f64 / base_rate_hz;

        // 1. Seed every logged channel from the log (zero-order hold) — the ground
        //    truth (precedence: log over calibration).
        for (path, series) in &log_inputs {
            env.set(path.clone(), series.sample(t));
        }

        // 2. Layer the overrides on top (precedence: override over log). A constant
        //    is written directly; an expression is evaluated against the *logged*
        //    snapshot just seeded (so `CH = CH * k` reads the logged `CH`). Every
        //    expression is evaluated first, against the same logged snapshot, then
        //    the results are written together — so two overrides cannot observe one
        //    another's freshly-written value within the tick.
        let mut pending: Vec<(String, Value)> = Vec::with_capacity(prepared.len());
        for ov in &prepared {
            let value = match ov {
                PreparedOverride::Const { value, .. } => value.clone(),
                PreparedOverride::Expr {
                    wrapped,
                    group,
                    fn_symbol,
                    ..
                } => {
                    // Re-parse the (pre-validated) wrapped snippet and evaluate its
                    // value node — the `Cst` and its borrowed node live together on
                    // this stack frame, so no self-referential storage is needed.
                    let cst = m1_core::parse(wrapped);
                    let value_node = override_value_node(&cst).ok_or_else(|| {
                        EvalError::UnsupportedConstruct {
                            kind: format!("override expression {wrapped:?} did not parse"),
                            at: 0,
                        }
                    })?;
                    let mut ctx = EvalCtx {
                        project: &loaded.project,
                        calib: &loaded.calib,
                        env: &mut env,
                        state: &mut state,
                        group: group.as_deref(),
                        fn_symbol: fn_symbol.as_deref(),
                        script_name: CF_OVERRIDE_SCRIPT,
                        dt: 1.0 / base_rate_hz,
                        scripts: &loaded.scripts,
                        depth: 0,
                        trace: None,
                    };
                    crate::expr::eval(&value_node, &mut ctx)?
                }
            };
            pending.push((ov.channel().to_string(), value));
        }
        for (path, value) in pending {
            env.set(path, value);
        }

        // 3. Open the tick.
        trace.push_tick(t);

        // 4. Run only the cone functions, writer-before-reader. Each recomputes its
        //    downstream channel from the overridden inputs; everything else holds
        //    its logged (or overridden) value.
        for rated in &rated {
            let sched = &rated.sched;
            let root = sched.script.cst.root();
            let mut ctx = EvalCtx {
                project: &loaded.project,
                calib: &loaded.calib,
                env: &mut env,
                state: &mut state,
                group: sched.group.as_deref(),
                fn_symbol: sched.fn_symbol.as_deref(),
                script_name: &sched.script.name,
                dt: 1.0 / base_rate_hz,
                scripts: &loaded.scripts,
                depth: 0,
                trace: Some(&mut trace),
            };
            exec_script(&root, &mut ctx)?;
        }

        // 5. Record every channel that did not record this tick by holding its env
        //    value: the cone's held writes, any channel with a column already, then
        //    the logged channels and the overrides — so the trace is a dense grid
        //    with the logged value on every pass-through channel and the recomputed
        //    value on every cone channel.
        hold_unwritten_channels(&scheduled_writes, &env, &mut trace);
        hold_trace_channels(&env, &mut trace);
        for (path, series) in &log_inputs {
            let already = trace
                .channels
                .get(path)
                .map(|c| c.len() == trace.time.len())
                .unwrap_or(false);
            if !already {
                let v = env.get(path).cloned().unwrap_or_else(|| series.sample(t));
                trace.record_channel(path.clone(), v);
                trace.mark_external(path.clone());
            }
        }
        for ov in &prepared {
            let path = ov.channel();
            let already = trace
                .channels
                .get(path)
                .map(|c| c.len() == trace.time.len())
                .unwrap_or(false);
            if !already && let Some(v) = env.get(path).cloned() {
                trace.record_channel(path.to_string(), v);
                trace.mark_external(path.to_string());
            }
        }
    }

    Ok(trace)
}

/// The synthetic script-name identity used for the [`CallSite`](crate::env::CallSite)
/// of an expression override's stateful operators (overrides rarely use stateful
/// operators, but the call-site key needs a stable, collision-free name).
const CF_OVERRIDE_SCRIPT: &str = "<counterfactual-override>";

/// A counterfactual override compiled for the tick loop: a canonical channel path
/// plus either a constant value or a pre-validated expression snippet re-parsed and
/// evaluated each tick against the logged snapshot.
enum PreparedOverride {
    /// A constant pinned every tick.
    Const { channel: String, value: Value },
    /// An expression evaluated each tick. `wrapped` is the snippet wrapped as an
    /// assignment (`__cf__ = <source>;`), validated to parse at preparation time
    /// and re-parsed per tick (the `Cst` + its borrowed node then live together on
    /// the tick stack frame — no self-referential storage). `group`/`fn_symbol` are
    /// the scope the expression resolves names in.
    Expr {
        channel: String,
        wrapped: String,
        group: Option<String>,
        fn_symbol: Option<String>,
    },
}

impl PreparedOverride {
    /// The canonical channel path this override writes.
    fn channel(&self) -> &str {
        match self {
            PreparedOverride::Const { channel, .. } | PreparedOverride::Expr { channel, .. } => {
                channel
            }
        }
    }
}

/// Compile each [`Override`] for the counterfactual tick loop: canonicalise its
/// channel against the project scope, and for an expression override wrap the
/// source as an assignment and validate that it parses (failing loud now rather
/// than mid-run).
///
/// The expression source is wrapped as an assignment (`__cf__ = <source>;`) — the
/// same re-parse trick [`crate::stmt`]'s expand statement uses — so the existing
/// assignment-value extraction reaches the bare expression node. The wrapped string
/// is kept and re-parsed each tick; an expression that does not parse to a single
/// assignment with a value fails loud here.
fn prepare_overrides(
    overrides: &[Override],
    loaded: &Loaded,
    group: Option<&str>,
    fn_symbol: Option<&str>,
) -> Result<Vec<PreparedOverride>, EvalError> {
    let no_locals = HashMap::new();
    overrides
        .iter()
        .map(|ov| {
            let canon = match classify(ov.channel(), group, fn_symbol, &loaded.project, &no_locals)
            {
                Target::Symbol(p) => p,
                _ => ov.channel().to_string(),
            };
            match ov {
                Override::Const { value, .. } => Ok(PreparedOverride::Const {
                    channel: canon,
                    value: value.clone(),
                }),
                Override::Expr { source, .. } => {
                    let wrapped = format!("__cf__ = {source};\n");
                    // Validate at preparation time: a snippet that does not parse to
                    // an assignment with a value node fails loud now, not mid-run.
                    let cst = m1_core::parse(&wrapped);
                    if override_value_node(&cst).is_none() {
                        return Err(EvalError::UnsupportedConstruct {
                            kind: format!(
                                "override expression {source:?} did not parse to an expression"
                            ),
                            at: 0,
                        });
                    }
                    Ok(PreparedOverride::Expr {
                        channel: canon,
                        wrapped,
                        group: group.map(str::to_string),
                        fn_symbol: fn_symbol.map(str::to_string),
                    })
                }
            }
        })
        .collect()
}

/// The value (right-hand-side) node of the single wrapping assignment in a parsed
/// override snippet, if present. The override source was wrapped as
/// `__cf__ = <source>;`, so the assignment's `Value` field is the bare expression
/// to evaluate. Returns `None` when the snippet does not parse to an assignment with
/// a value (a fail-loud signal for the caller).
fn override_value_node(cst: &m1_core::Cst) -> Option<m1_core::Node<'_>> {
    use m1_core::{Field, Kind};
    cst.root()
        .children()
        .into_iter()
        .find(|c| c.kind() == Kind::AssignmentStatement)
        .and_then(|stmt| stmt.child_by_field(Field::Value))
}

/// Topologically order `nodes` by the `edges` (`from` must precede `to`) using
/// Kahn's algorithm. Ties break by name for determinism. A cycle leaves some
/// nodes unscheduled; those are appended in sorted order so the run still covers
/// every needed function (ordering is best-effort, values are not).
fn topo_order(nodes: &BTreeSet<String>, edges: &[(String, String)]) -> Vec<String> {
    let mut indeg: BTreeMap<&str, usize> = nodes.iter().map(|n| (n.as_str(), 0)).collect();
    let mut adj: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (from, to) in edges {
        if nodes.contains(from) && nodes.contains(to) && from != to {
            // Avoid double-counting a duplicate edge.
            if adj.entry(from.as_str()).or_default().insert(to.as_str()) {
                *indeg.get_mut(to.as_str()).unwrap() += 1;
            }
        }
    }

    // Ready set: indegree 0, ordered by name (BTreeSet gives sorted pop).
    let mut ready: BTreeSet<&str> = indeg
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(n, _)| *n)
        .collect();
    let mut out = Vec::with_capacity(nodes.len());
    while let Some(&n) = ready.iter().next() {
        ready.remove(n);
        out.push(n.to_string());
        if let Some(succs) = adj.get(n) {
            for &m in succs {
                let d = indeg.get_mut(m).unwrap();
                *d -= 1;
                if *d == 0 {
                    ready.insert(m);
                }
            }
        }
    }

    // Any node left out is part of a cycle; append in sorted order.
    if out.len() < nodes.len() {
        for n in nodes {
            if !out.iter().any(|o| o == n) {
                out.push(n.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::load;
    use crate::scenario::{InputKind, Scenario};
    use std::path::Path;

    fn mini() -> Loaded {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini");
        load(
            &dir.join("Project.m1prj"),
            Some(&dir.join("parameters.m1cfg")),
        )
        .expect("mini fixture loads")
    }

    fn multirate() -> Loaded {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/multirate");
        load(&dir.join("Project.m1prj"), None).expect("multirate fixture loads")
    }

    fn ratemix() -> Loaded {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ratemix");
        load(&dir.join("Project.m1prj"), None).expect("ratemix fixture loads")
    }

    #[test]
    fn auto_base_is_lcm_of_scheduled_rates() {
        // 500 Hz and 200 Hz do not divide each other: neither rate can serve as
        // the base without a fractional divisor. The auto base must be their
        // least common multiple (1000 Hz) so both have exact integer periods.
        // A 0.01 s run therefore spans 10 ticks (not 5 at a 500 Hz base).
        let loaded = ratemix();
        let toml = r#"
mode = "whole-project"
duration_s = 0.01

[[inputs]]
channel = "Root.RX.Seed"
const = 3.0
"#;
        let scenario = Scenario::from_toml_str(toml).unwrap();
        let trace = run(&loaded, &scenario).expect("auto-base ratemix run succeeds");
        assert_eq!(trace.time.len(), 10, "auto base = lcm(500, 200) = 1000 Hz");
    }

    #[test]
    fn exact_invocation_counts_and_dt_at_500_and_200_hz() {
        // Over exactly one second the 500 Hz counter must run exactly 500 times
        // and the 200 Hz counter exactly 200 times — the rounded-divisor
        // scheduler ran the 200 Hz function 167 times (round(500/200) = 3) on a
        // 500 Hz base. Each function counts its own invocations.
        //
        // Exact dt: trapezoidal Integral.Normal of a constant Seed = 3 advances
        // by Seed*dt per run from the second run on (run k holds 3*dt*k), so
        // after 200 runs Mid Total = 3.0 * 0.005 * 199 = 2.985 exactly — only
        // when dt is exactly 5 ms AND the invocation count is exactly 200.
        let loaded = ratemix();
        let toml = r#"
mode = "whole-project"
duration_s = 1.0

[[inputs]]
channel = "Root.RX.Seed"
const = 3.0
"#;
        let scenario = Scenario::from_toml_str(toml).unwrap();
        let trace = run(&loaded, &scenario).expect("ratemix run succeeds");

        let last_f64 = |name: &str| -> f64 {
            match trace.channels.get(name).expect(name).last().expect(name) {
                Value::Float(x) => *x,
                other => panic!("expected float for {name}, got {other:?}"),
            }
        };
        assert_eq!(last_f64("Root.RX.Fast Count"), 500.0, "500 Hz runs/second");
        assert_eq!(last_f64("Root.RX.Mid Count"), 200.0, "200 Hz runs/second");
        let total = last_f64("Root.RX.Mid Total");
        assert!(
            (total - 2.985).abs() < 1e-9,
            "exact dt=5 ms trapezoidal accumulation, got {total}"
        );
    }

    #[test]
    fn pinned_base_not_exactly_divisible_is_rejected() {
        // A pinned 500 Hz base cannot represent a 200 Hz function exactly
        // (500/200 = 2.5): the run must fail loud rather than round the divisor
        // and silently run the function at ~166.7 Hz.
        let loaded = ratemix();
        let toml = r#"
mode = "whole-project"
duration_s = 0.01
base_rate_hz = 500.0

[[inputs]]
channel = "Root.RX.Seed"
const = 3.0
"#;
        let scenario = Scenario::from_toml_str(toml).unwrap();
        let err = run(&loaded, &scenario).expect_err("500 Hz base with a 200 Hz rate must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("200") && msg.contains("500"),
            "error names the incompatible rate and base: {msg}"
        );
    }

    #[test]
    fn pinned_base_below_fastest_rate_is_rejected() {
        // A 50 Hz base cannot schedule the multirate fixture's 100 Hz functions:
        // the old scheduler clamped the divisor to 1 and silently ran them at
        // 50 Hz. It must fail loud instead.
        let loaded = multirate();
        let toml = r#"
mode = "whole-project"
duration_s = 0.1
base_rate_hz = 50.0

[[inputs]]
channel = "Root.MR.Seed"
const = 3.0
"#;
        let scenario = Scenario::from_toml_str(toml).unwrap();
        let err = run(&loaded, &scenario).expect_err("base below fastest rate must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("100") && msg.contains("50"),
            "error names the too-fast rate and the base: {msg}"
        );
    }

    #[test]
    fn enumerate_scheduled_keeps_periodic_excludes_startup() {
        // The multirate fixture has four periodic functions (two 50 Hz, two
        // 100 Hz) plus an On-Startup function whose call_rate_hz is None. The
        // schedule must include exactly the four rated functions and exclude
        // the startup one.
        let loaded = multirate();
        let rated = enumerate_scheduled(&loaded);

        let by_fn: std::collections::HashMap<String, f64> = rated
            .iter()
            .map(|r| (r.sched.fn_symbol.clone().unwrap(), r.rate_hz))
            .collect();

        assert_eq!(by_fn.len(), 4, "exactly four periodic functions: {by_fn:?}");
        assert_eq!(by_fn.get("Root.MR.Slow Writer"), Some(&50.0));
        assert_eq!(by_fn.get("Root.MR.Slow Integrator"), Some(&50.0));
        assert_eq!(by_fn.get("Root.MR.Fast Writer"), Some(&100.0));
        assert_eq!(by_fn.get("Root.MR.Fast Reader"), Some(&100.0));
        // The On-Startup function (call_rate_hz = None) is excluded entirely.
        assert!(
            !by_fn.contains_key("Root.MR.Init"),
            "startup function must not be scheduled: {by_fn:?}"
        );
    }

    #[test]
    fn enumerate_scheduled_is_rate_sorted_fastest_first() {
        // The deterministic baseline ordering is fastest-rate-first (the
        // dependency layer refines within a rate). So both 100 Hz functions
        // precede the two 50 Hz ones.
        let loaded = multirate();
        let rated = enumerate_scheduled(&loaded);
        let rates: Vec<f64> = rated.iter().map(|r| r.rate_hz).collect();
        assert_eq!(
            rates,
            vec![100.0, 100.0, 50.0, 50.0],
            "fastest-first baseline"
        );
    }

    #[test]
    fn dependency_then_rate_orders_writer_before_reader_within_a_rate() {
        // Within the 100 Hz group, Fast Writer (writes Fast Shared) must precede
        // Fast Reader (reads Fast Shared) even though "Root.MR.Fast Reader" sorts
        // before "Root.MR.Fast Writer" by name. Across rates, the 100 Hz group
        // runs before the 50 Hz Slow Writer (fastest-first); no cross-rate edge
        // forces Slow Writer ahead of its 100 Hz reader.
        let loaded = multirate();
        let ordered = build_whole_project_schedule(&loaded);
        let order: Vec<String> = ordered
            .iter()
            .map(|r| r.sched.fn_symbol.clone().unwrap())
            .collect();
        assert_eq!(
            order,
            vec![
                "Root.MR.Fast Writer".to_string(),
                "Root.MR.Fast Reader".to_string(),
                // 50 Hz group: no inter-dependency, so name order — "Integrator"
                // sorts before "Writer".
                "Root.MR.Slow Integrator".to_string(),
                "Root.MR.Slow Writer".to_string(),
            ],
            "writer-before-reader within rate, fastest group first: {order:?}"
        );
    }

    #[test]
    fn whole_project_run_computes_every_scheduled_channel() {
        // End-to-end whole-project run over the multirate fixture. With Seed = 3
        // (constant input), Slow Writer computes Slow Out = 6. The 100 Hz Fast
        // Writer reads Slow Out *across rates*: the fast group runs before the
        // slow writer each tick, so it reads the previous value (stale-tolerant —
        // no cross-rate edge). We seed Slow Out's steady-state (6) so the very
        // first tick has a value to read; from then on Slow Writer holds it at 6.
        // Therefore Fast Shared = 7 and Fast Out = 70 on every tick. Within the
        // 100 Hz group the order runs Fast Writer before Fast Reader, so Fast Out
        // sees the freshly-written Fast Shared the same tick. (Per-divisor
        // rate-gating is P2-B; here every function runs every tick.)
        let loaded = multirate();
        let toml = r#"
mode = "whole-project"
duration_s = 0.05
base_rate_hz = 100.0

[[inputs]]
channel = "Root.MR.Seed"
const = 3.0

[[inputs]]
channel = "Root.MR.Slow Out"
const = 6.0
"#;
        let scenario = Scenario::from_toml_str(toml).unwrap();
        let trace = run(&loaded, &scenario).expect("whole-project run succeeds");

        assert_eq!(trace.time.len(), 5);
        let slow = trace.channels.get("Root.MR.Slow Out").expect("Slow Out");
        let shared = trace
            .channels
            .get("Root.MR.Fast Shared")
            .expect("Fast Shared");
        let fast = trace.channels.get("Root.MR.Fast Out").expect("Fast Out");
        assert!(slow.iter().all(|v| *v == Value::Float(6.0)), "{slow:?}");
        assert!(shared.iter().all(|v| *v == Value::Float(7.0)), "{shared:?}");
        assert!(fast.iter().all(|v| *v == Value::Float(70.0)), "{fast:?}");

        // The startup function never runs in whole-project mode, so its channel
        // is never written by the schedule.
        assert!(
            !trace.channels.contains_key("Root.MR.Started"),
            "startup channel must not be produced"
        );
    }

    #[test]
    fn whole_project_run_is_deterministic() {
        // Same scenario twice over the multirate fixture must produce identical
        // traces — the strongest determinism check for the scheduler.
        let loaded = multirate();
        let toml = r#"
mode = "whole-project"
duration_s = 0.05
base_rate_hz = 100.0

[[inputs]]
channel = "Root.MR.Seed"
const = 3.0

[[inputs]]
channel = "Root.MR.Slow Out"
const = 6.0
"#;
        let scenario = Scenario::from_toml_str(toml).unwrap();
        let a = run(&loaded, &scenario).expect("run a");
        let b = run(&loaded, &scenario).expect("run b");
        assert_eq!(a.time, b.time);
        assert_eq!(a.channels, b.channels);
    }

    #[test]
    fn rate_gated_slow_function_updates_on_its_ticks_and_holds_between() {
        // base_rate = 100 Hz. The 50 Hz Slow Writer has divisor 2, so it runs on
        // even base ticks (0, 2, 4, …) and holds its outputs between. We observe
        // `Slow Echo` (= Seed*2) — a channel nothing reads, so there is no
        // cross-rate first-tick dependency to seed and the column shows the pure
        // zero-order-hold. Drive Seed with a per-tick-distinct series so a fresh
        // run produces a fresh value; the held ticks must repeat the previous
        // run's value.
        //
        // Seed series (t in s): 0.00->1, 0.01->2, 0.02->3, 0.03->4, 0.04->5, …
        // Slow Echo = Seed*2 computed only on even ticks:
        //   tick 0 (t=0.00, Seed=1): Slow Echo = 2     (run)
        //   tick 1 (t=0.01):         Slow Echo = 2     (held — NOT 4)
        //   tick 2 (t=0.02, Seed=3): Slow Echo = 6     (run)
        //   tick 3 (t=0.03):         Slow Echo = 6     (held — NOT 8)
        //   tick 4 (t=0.04, Seed=5): Slow Echo = 10    (run)
        //   tick 5 (t=0.05):         Slow Echo = 10    (held)
        //
        // `Slow Out` is seeded to its steady value so the cross-rate Fast Writer
        // read on tick 0 succeeds (the schedule runs the fast group first); the
        // seed is constant so it never masks the held-value check on Slow Echo.
        let loaded = multirate();
        let toml = r#"
mode = "whole-project"
duration_s = 0.06
base_rate_hz = 100.0

[[inputs]]
channel = "Root.MR.Seed"
series = [[0.0, 1.0], [0.01, 2.0], [0.02, 3.0], [0.03, 4.0], [0.04, 5.0], [0.05, 6.0]]

[[inputs]]
channel = "Root.MR.Slow Out"
const = 2.0
"#;
        let scenario = Scenario::from_toml_str(toml).unwrap();
        let trace = run(&loaded, &scenario).expect("rate-gated run succeeds");

        assert_eq!(trace.time.len(), 6);
        let slow = trace.channels.get("Root.MR.Slow Echo").expect("Slow Echo");
        let got: Vec<f64> = slow
            .iter()
            .map(|v| match v {
                Value::Float(x) => *x,
                other => panic!("expected float, got {other:?}"),
            })
            .collect();
        assert_eq!(
            got,
            vec![2.0, 2.0, 6.0, 6.0, 10.0, 10.0],
            "50 Hz Slow Echo updates on even ticks and holds between"
        );
    }

    #[test]
    fn rate_gated_integral_accumulates_with_its_own_dt_not_base_dt() {
        // The 50 Hz Slow Integrator integrates Seed (constant 2.0) into Slow Total.
        // It runs once every 2 base ticks (divisor 2 at a 100 Hz base) and must
        // accumulate with dt = 1/50 = 0.02 s — its OWN period — not the 0.01 base
        // dt. Trapezoidal Integral.Normal with a constant rate r = 2.0 and dt:
        //   run 0: 0
        //   run 1: 0 + (2+2)/2 * dt = 2*dt
        //   run k: 2*dt*k
        // With the correct dt = 0.02 the accumulator advances by 0.04 each run; a
        // bug that fed the base dt = 0.01 would advance by 0.02 (half) — the test
        // distinguishes the two.
        //
        // Base ticks (100 Hz) over 0.12 s = 12 ticks. The integrator runs on the
        // even ticks (0,2,4,6,8,10) = 6 runs, holding Slow Total between:
        //   tick 0  run0: 0.00
        //   tick 1  held: 0.00
        //   tick 2  run1: 0.04
        //   tick 3  held: 0.04
        //   tick 4  run2: 0.08
        //   tick 5  held: 0.08
        //   tick 6  run3: 0.12
        //   …
        let loaded = multirate();
        let toml = r#"
mode = "whole-project"
duration_s = 0.12
base_rate_hz = 100.0

[[inputs]]
channel = "Root.MR.Seed"
const = 2.0

[[inputs]]
channel = "Root.MR.Slow Out"
const = 4.0
"#;
        let scenario = Scenario::from_toml_str(toml).unwrap();
        let trace = run(&loaded, &scenario).expect("rate-gated integral run succeeds");

        assert_eq!(trace.time.len(), 12);
        let total = trace
            .channels
            .get("Root.MR.Slow Total")
            .expect("Slow Total");
        let got: Vec<f64> = total
            .iter()
            .map(|v| match v {
                Value::Float(x) => *x,
                other => panic!("expected float, got {other:?}"),
            })
            .collect();
        let expected = [
            0.00, 0.00, 0.04, 0.04, 0.08, 0.08, 0.12, 0.12, 0.16, 0.16, 0.20, 0.20,
        ];
        for (g, e) in got.iter().zip(expected.iter()) {
            assert!(
                (g - e).abs() < 1e-9,
                "rate-correct dt=0.02 accumulation: got {got:?}, expected {expected:?}"
            );
        }
    }

    #[test]
    fn base_rate_defaults_to_lcm_of_scheduled_rates_when_unset() {
        // With base_rate_hz omitted (0.0 = "auto"), the whole-project runner uses
        // the least common multiple of the scheduled rates as the base tick —
        // lcm(100, 50) = 100 Hz here, so a 0.05 s run produces 5 ticks, the
        // 100 Hz functions run every tick, and the 50 Hz ones run every other.
        let loaded = multirate();
        let toml = r#"
mode = "whole-project"
duration_s = 0.05

[[inputs]]
channel = "Root.MR.Seed"
const = 3.0

[[inputs]]
channel = "Root.MR.Slow Out"
const = 6.0
"#;
        let scenario = Scenario::from_toml_str(toml).unwrap();
        let trace = run(&loaded, &scenario).expect("auto-base-rate run succeeds");

        // 0.05 s at the auto base of 100 Hz = 5 ticks.
        assert_eq!(
            trace.time.len(),
            5,
            "auto base = lcm of scheduled rates (100 Hz)"
        );
        // The fast group ran every tick: Slow Out = Seed*2 = 6 on the even ticks
        // it ran; Fast Writer reads it (stale-tolerant) and writes Fast Shared.
        let fast = trace.channels.get("Root.MR.Fast Out").expect("Fast Out");
        assert_eq!(fast.len(), 5, "fast channel present every tick");
    }

    #[test]
    fn tick_count_spans_half_open_interval() {
        assert_eq!(tick_count(1.0, 100.0), 100);
        assert_eq!(tick_count(0.5, 100.0), 50);
        assert_eq!(tick_count(0.0, 100.0), 0);
    }

    #[test]
    fn single_function_run_computes_output_each_tick() {
        let loaded = mini();
        // Demo.Update: Output = Speed * Gain. Speed=20, Gain=2.5 -> 50 each tick.
        let toml = r#"
mode = "function"
target = "Demo.Update"
duration_s = 0.05
base_rate_hz = 100.0

[[inputs]]
channel = "Root.Demo.Speed"
const = 20.0

[[inputs]]
channel = "Root.Demo.Gain"
const = 2.5
"#;
        let scenario = Scenario::from_toml_str(toml).unwrap();
        let trace = run(&loaded, &scenario).expect("run succeeds");

        // 0.05s at 100Hz = 5 ticks.
        assert_eq!(trace.time.len(), 5);
        let out = trace
            .channels
            .get("Root.Demo.Output")
            .expect("Output recorded");
        assert_eq!(out.len(), 5);
        assert!(out.iter().all(|v| *v == Value::Float(50.0)), "{out:?}");
    }

    #[test]
    fn single_function_integral_accumulates_over_ticks() {
        // The integral fixture: Total = Integral.Normal(Rate, ...). With a
        // constant Rate = 2.0 and dt = 0.1 s, trapezoidal accumulation gives
        // 0, 0.2, 0.4, 0.6, 0.8 over the first five ticks (first tick seeds 0).
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/integral");
        let loaded = load(&dir.join("Project.m1prj"), None).expect("integral fixture loads");

        let toml = r#"
mode = "function"
target = "Acc.Update"
duration_s = 0.5
base_rate_hz = 10.0

[[inputs]]
channel = "Root.Acc.Rate"
const = 2.0
"#;
        let scenario = Scenario::from_toml_str(toml).unwrap();
        let trace = run(&loaded, &scenario).expect("integral run succeeds");

        let total = trace
            .channels
            .get("Root.Acc.Total")
            .expect("Total recorded");
        assert_eq!(total.len(), 5);
        let got: Vec<f64> = total
            .iter()
            .map(|v| match v {
                Value::Float(x) => *x,
                other => panic!("expected float, got {other:?}"),
            })
            .collect();
        let expected = [0.0, 0.2, 0.4, 0.6, 0.8];
        for (g, e) in got.iter().zip(expected.iter()) {
            assert!((g - e).abs() < 1e-9, "got {got:?}, expected {expected:?}");
        }
    }

    #[test]
    fn single_function_calls_user_helper_inline() {
        // The userfn fixture: Caller.Update runs `Output = Helper.Compute(Input)`,
        // and the helper (a FuncUserParam, `<Param x>`, body `Out = In.x * 2.0`)
        // is executed inline. With Input = 4, Output = 8 each tick — proving the
        // call binds In.x, runs the helper body, and reads its Out return.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/userfn");
        let loaded = load(&dir.join("Project.m1prj"), None).expect("userfn fixture loads");

        let toml = r#"
mode = "function"
target = "Caller.Update"
duration_s = 0.03
base_rate_hz = 100.0

[[inputs]]
channel = "Root.Caller.Input"
const = 4.0
"#;
        let scenario = Scenario::from_toml_str(toml).unwrap();
        let trace = run(&loaded, &scenario).expect("userfn run succeeds");

        assert_eq!(trace.time.len(), 3);
        let out = trace
            .channels
            .get("Root.Caller.Output")
            .expect("Output recorded");
        assert!(out.iter().all(|v| *v == Value::Float(8.0)), "{out:?}");
    }

    #[test]
    fn cone_runs_upstream_chain_in_topological_order() {
        // Cone fixture: Producer (Z.Producer.m1scr) writes Mid = Raw + 1; Consumer
        // (B.Consumer.m1scr) writes Final = Mid * 10. Targeting Final must pull in
        // both and run the producer first — even though it sorts AFTER the consumer
        // by filename. With Raw = 4: Mid = 5, Final = 50.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cone");
        let loaded = load(&dir.join("Project.m1prj"), None).expect("cone fixture loads");

        let toml = r#"
mode = "cone"
target = "Root.Chain.Final"
duration_s = 0.02
base_rate_hz = 100.0

[[inputs]]
channel = "Root.Chain.Raw"
const = 4.0
"#;
        let scenario = Scenario::from_toml_str(toml).unwrap();
        let trace = run(&loaded, &scenario).expect("cone run succeeds");

        assert_eq!(trace.time.len(), 2);
        let final_col = trace
            .channels
            .get("Root.Chain.Final")
            .expect("Final recorded");
        assert!(
            final_col.iter().all(|v| *v == Value::Float(50.0)),
            "{final_col:?}"
        );
        // The intermediate channel is computed and recorded too.
        let mid = trace.channels.get("Root.Chain.Mid").expect("Mid recorded");
        assert!(mid.iter().all(|v| *v == Value::Float(5.0)), "{mid:?}");
    }

    #[test]
    fn cone_target_with_no_writer_fails_loud() {
        // Raw has no in-project writer; targeting it as a computed channel must
        // fail loud rather than silently produce an empty schedule.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cone");
        let loaded = load(&dir.join("Project.m1prj"), None).expect("cone fixture loads");
        let toml = r#"
mode = "cone"
target = "Root.Chain.Raw"
duration_s = 0.01
base_rate_hz = 100.0
"#;
        let scenario = Scenario::from_toml_str(toml).unwrap();
        match run(&loaded, &scenario) {
            Err(EvalError::UnresolvedSymbol { .. }) => {}
            other => panic!("expected UnresolvedSymbol, got {other:?}"),
        }
    }

    #[test]
    fn topo_order_orders_dependency_before_dependent() {
        let mut nodes = BTreeSet::new();
        nodes.insert("consumer".to_string());
        nodes.insert("producer".to_string());
        // producer must precede consumer.
        let edges = vec![("producer".to_string(), "consumer".to_string())];
        let order = topo_order(&nodes, &edges);
        assert_eq!(order, vec!["producer".to_string(), "consumer".to_string()]);
    }

    #[test]
    fn topo_order_handles_a_cycle_by_appending_unscheduled() {
        let mut nodes = BTreeSet::new();
        nodes.insert("a".to_string());
        nodes.insert("b".to_string());
        // a -> b and b -> a: a true cycle, nothing has indegree 0.
        let edges = vec![
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "a".to_string()),
        ];
        let order = topo_order(&nodes, &edges);
        // Both still appear (best-effort ordering, fail-soft), in sorted order.
        assert_eq!(order.len(), 2);
        assert!(order.contains(&"a".to_string()));
        assert!(order.contains(&"b".to_string()));
    }

    #[test]
    fn unknown_function_target_fails_loud() {
        let loaded = mini();
        let toml = r#"
mode = "function"
target = "No.Such.Function"
duration_s = 0.01
base_rate_hz = 100.0
"#;
        let scenario = Scenario::from_toml_str(toml).unwrap();
        match run(&loaded, &scenario) {
            Err(EvalError::UnresolvedSymbol { .. }) => {}
            other => panic!("expected UnresolvedSymbol, got {other:?}"),
        }
    }

    fn counterfactual() -> Loaded {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/counterfactual");
        load(&dir.join("Project.m1prj"), None).expect("counterfactual fixture loads")
    }

    /// The `fn_symbol` path of each scheduled function in a cone, in order — the
    /// load-bearing observable for the downstream-cone tests.
    fn cone_fn_symbols(cone: &[Scheduled<'_>]) -> Vec<String> {
        cone.iter()
            .map(|s| s.fn_symbol.clone().expect("scheduled fn has a symbol"))
            .collect()
    }

    #[test]
    fn downstream_cone_from_sensor_recomputes_a_then_b_excludes_unrelated() {
        // Override Sensor: stage A reads Sensor (writes Mid), stage B reads Mid
        // (writes Out). The forward cone is therefore [A, B] in that order — even
        // though A's filename (Z.A) sorts AFTER B's (B.B). The unrelated stage C
        // (writes Other, reads nothing in the chain) is excluded.
        let loaded = counterfactual();
        let cone =
            build_downstream_cone(&loaded, &["Root.CF.Sensor".to_string()]).expect("cone builds");
        assert_eq!(
            cone_fn_symbols(&cone),
            vec!["Root.CF.A".to_string(), "Root.CF.B".to_string()],
            "Sensor override recomputes A then B, never C",
        );
    }

    #[test]
    fn downstream_cone_from_mid_recomputes_only_b() {
        // Overriding the intermediate channel Mid recomputes only its readers'
        // chain: B (reads Mid) and nothing further. A (which *writes* Mid) is not
        // in the cone — its output is being overridden, so it must not run.
        let loaded = counterfactual();
        let cone =
            build_downstream_cone(&loaded, &["Root.CF.Mid".to_string()]).expect("cone builds");
        assert_eq!(
            cone_fn_symbols(&cone),
            vec!["Root.CF.B".to_string()],
            "Mid override recomputes only B",
        );
    }

    #[test]
    fn downstream_cone_of_a_leaf_with_no_reader_fails_loud() {
        // Result is a leaf: no function in the project reads it, so overriding it
        // has nothing downstream to recompute. That is a user error worth surfacing
        // — fail loud rather than return an empty (silently no-op) cone.
        let loaded = counterfactual();
        match build_downstream_cone(&loaded, &["Root.CF.Result".to_string()]) {
            Err(EvalError::UnresolvedSymbol { .. }) => {}
            Err(other) => {
                panic!("expected UnresolvedSymbol for a no-reader override, got {other:?}")
            }
            Ok(cone) => panic!(
                "expected fail-loud for a no-reader override, got cone {:?}",
                cone_fn_symbols(&cone)
            ),
        }
    }

    #[test]
    fn downstream_cone_multiple_overrides_union_their_readers() {
        // Two overrides at once (Sensor and the unrelated Other). Sensor pulls in
        // A then B; Other has a writer (C) but no in-project reader, so it
        // contributes nothing — the union is still exactly [A, B], proving the
        // multi-override seed unions readers without spuriously dragging in C.
        let loaded = counterfactual();
        let cone = build_downstream_cone(
            &loaded,
            &["Root.CF.Sensor".to_string(), "Root.CF.Other".to_string()],
        )
        .expect("cone builds");
        assert_eq!(
            cone_fn_symbols(&cone),
            vec!["Root.CF.A".to_string(), "Root.CF.B".to_string()],
        );
    }

    // ---- P3-B Task 6: counterfactual run (log ground truth + cone recompute) ----

    use crate::log::LogMeta;

    /// A synthetic [`Log`] over the counterfactual fixture whose `Sensor`/`Mid`/
    /// `Result`/`Other` series are *mutually consistent*: `Mid = Sensor*2`,
    /// `Result = Mid+1`, `Other = 42`. `Sensor` ramps 10 → 20 → 30 across three
    /// keyframes so a downstream recompute is observably different from the logged
    /// pass-through. This is the ground truth a counterfactual replays against.
    fn consistent_log() -> Log {
        let series = |channel: &str, vals: &[f64]| InputSeries {
            channel: channel.to_string(),
            kind: InputKind::Series(
                vals.iter()
                    .enumerate()
                    .map(|(i, v)| (i as f64 * 0.01, Value::Float(*v)))
                    .collect(),
            ),
        };
        let sensor = [10.0, 20.0, 30.0];
        let mid: Vec<f64> = sensor.iter().map(|s| s * 2.0).collect();
        let result: Vec<f64> = mid.iter().map(|m| m + 1.0).collect();
        let channels = vec![
            series("Root.CF.Sensor", &sensor),
            series("Root.CF.Mid", &mid),
            series("Root.CF.Result", &result),
            series("Root.CF.Other", &[42.0, 42.0, 42.0]),
        ];
        let channel_count = channels.len();
        Log {
            channels,
            meta: LogMeta {
                source: "synthetic-consistent".to_string(),
                duration_s: 0.02,
                channel_count,
                units: BTreeMap::new(),
            },
        }
    }

    fn floats(trace: &Trace, channel: &str) -> Vec<f64> {
        trace
            .channels
            .get(channel)
            .unwrap_or_else(|| panic!("channel {channel:?} present"))
            .iter()
            .map(|v| match v {
                Value::Float(x) => *x,
                other => panic!("expected float in {channel:?}, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn counterfactual_const_override_recomputes_cone_and_holds_the_rest() {
        // Override Sensor to a constant 100. Its downstream cone is [A, B], so
        // Mid (= Sensor*2) and Result (= Mid+1) recompute from the overridden
        // Sensor; the unrelated Other and every non-cone logged channel pass
        // through at their logged values.
        let loaded = counterfactual();
        let log = consistent_log();
        let overrides = vec![Override::parse("Root.CF.Sensor=100.0").expect("parses")];
        let cfg = CounterfactualCfg {
            base_rate_hz: 100.0,
            duration_s: 0.03,
        };
        let trace = run_counterfactual(&loaded, &log, &overrides, &cfg).expect("cf runs");

        // (a) the time grid matches the requested duration at the base rate.
        assert_eq!(trace.time.len(), 3, "0.03 s @ 100 Hz = 3 ticks");

        // (b) Mid and Result are recomputed from the OVERRIDDEN Sensor (100), not
        //     the logged Mid/Result: Mid = 100*2 = 200, Result = 200+1 = 201 every
        //     tick — proving the cone recomputes from the override, not the log.
        assert_eq!(floats(&trace, "Root.CF.Sensor"), vec![100.0, 100.0, 100.0]);
        assert_eq!(floats(&trace, "Root.CF.Mid"), vec![200.0, 200.0, 200.0]);
        assert_eq!(floats(&trace, "Root.CF.Result"), vec![201.0, 201.0, 201.0]);

        // (c) Other is unrelated to the override cone: it passes through at its
        //     logged value (42) — C never runs in the cone.
        assert_eq!(floats(&trace, "Root.CF.Other"), vec![42.0, 42.0, 42.0]);
    }

    #[test]
    fn counterfactual_no_op_const_reproduces_the_logged_cone() {
        // Pinning Sensor to its first logged value (10) with a consistent log: the
        // cone recomputes Mid/Result from 10, matching the first logged keyframe.
        // The load-bearing no-op invariant is exercised in full at the integration
        // level (Task 9); here we sanity-check that a constant equal to the logged
        // value reproduces the logged downstream series for the held region.
        let loaded = counterfactual();
        let log = consistent_log();
        // Override Sensor to a constant 10 (its t=0 logged value).
        let overrides = vec![Override::parse("Root.CF.Sensor=10.0").expect("parses")];
        let cfg = CounterfactualCfg {
            base_rate_hz: 100.0,
            duration_s: 0.01,
        };
        let trace = run_counterfactual(&loaded, &log, &overrides, &cfg).expect("cf runs");
        // At t=0 the logged Sensor is 10, so Mid=20, Result=21 — the logged values.
        assert_eq!(floats(&trace, "Root.CF.Mid"), vec![20.0]);
        assert_eq!(floats(&trace, "Root.CF.Result"), vec![21.0]);
    }

    #[test]
    fn counterfactual_seeds_non_cone_channels_from_the_log() {
        // Override the intermediate Mid: its cone is just [B] (Result = Mid+1).
        // With Mid pinned to 7, Result = 8. Sensor (upstream of the override, NOT
        // in the cone) and Other both pass through at their logged values — proving
        // every non-cone logged channel is seeded as ground truth.
        let loaded = counterfactual();
        let log = consistent_log();
        let overrides = vec![Override::parse("Root.CF.Mid=7.0").expect("parses")];
        let cfg = CounterfactualCfg {
            base_rate_hz: 100.0,
            duration_s: 0.01,
        };
        let trace = run_counterfactual(&loaded, &log, &overrides, &cfg).expect("cf runs");
        // Result recomputed from the overridden Mid.
        assert_eq!(floats(&trace, "Root.CF.Result"), vec![8.0]);
        // Mid holds the override; Sensor and Other hold their logged t=0 values.
        assert_eq!(floats(&trace, "Root.CF.Mid"), vec![7.0]);
        assert_eq!(floats(&trace, "Root.CF.Sensor"), vec![10.0]);
        assert_eq!(floats(&trace, "Root.CF.Other"), vec![42.0]);
    }

    #[test]
    fn counterfactual_default_duration_is_the_log_duration() {
        // With duration_s = 0 (the "auto" sentinel) the run spans the log's own
        // duration (0.02 s here -> at 100 Hz, ticks at t = 0.00 and 0.01, i.e. the
        // half-open [0, 0.02) interval = 2 ticks).
        let loaded = counterfactual();
        let log = consistent_log();
        let overrides = vec![Override::parse("Root.CF.Sensor=5").expect("parses")];
        let cfg = CounterfactualCfg {
            base_rate_hz: 100.0,
            duration_s: 0.0,
        };
        let trace = run_counterfactual(&loaded, &log, &overrides, &cfg).expect("cf runs");
        assert_eq!(
            trace.time.len(),
            2,
            "default duration = log.duration_s (0.02)"
        );
    }

    #[test]
    fn counterfactual_no_override_reproduces_log() {
        // The documented no-op invariant: `--log` with NO `--override` reproduces
        // the logged series verbatim and the changed-channel set is empty. With no
        // overrides there is no downstream cone to build, so nothing recomputes —
        // every logged channel passes through at its logged value. (Regression:
        // this used to fail loud with `no function reads override channel(s) []`.)
        let loaded = counterfactual();
        let log = consistent_log();
        let cfg = CounterfactualCfg {
            base_rate_hz: 100.0,
            duration_s: 0.03,
        };
        let trace = run_counterfactual(&loaded, &log, &[], &cfg).expect("no-op cf runs");

        // Every logged channel is reproduced verbatim at its logged keyframes.
        assert_eq!(floats(&trace, "Root.CF.Sensor"), vec![10.0, 20.0, 30.0]);
        assert_eq!(floats(&trace, "Root.CF.Mid"), vec![20.0, 40.0, 60.0]);
        assert_eq!(floats(&trace, "Root.CF.Result"), vec![21.0, 41.0, 61.0]);
        assert_eq!(floats(&trace, "Root.CF.Other"), vec![42.0, 42.0, 42.0]);

        // The changed-channel set is empty: the counterfactual trace equals the log
        // sampled on the same grid, so no channel moved.
        let log_inputs = canonicalise(&log.channels, &loaded, None, None);
        for (path, series) in &log_inputs {
            let cf = floats(&trace, path);
            for (i, value) in cf.iter().enumerate() {
                let t = i as f64 / cfg.base_rate_hz;
                let logged = match series.sample(t) {
                    Value::Float(x) => x,
                    other => panic!("expected float in log {path:?}, got {other:?}"),
                };
                assert_eq!(*value, logged, "channel {path:?} unchanged at tick {i}");
            }
        }
    }

    #[test]
    fn counterfactual_no_reader_override_fails_loud() {
        // Overriding a leaf channel nothing reads (Result) has no downstream cone
        // to recompute — the override would have no effect. Fail loud (mirrors the
        // build_downstream_cone fail-loud).
        let loaded = counterfactual();
        let log = consistent_log();
        let overrides = vec![Override::parse("Root.CF.Result=999.0").expect("parses")];
        let cfg = CounterfactualCfg {
            base_rate_hz: 100.0,
            duration_s: 0.01,
        };
        match run_counterfactual(&loaded, &log, &overrides, &cfg) {
            Err(EvalError::UnresolvedSymbol { .. }) => {}
            other => panic!("expected fail-loud for a no-reader override, got {other:?}"),
        }
    }

    #[test]
    fn counterfactual_expr_override_reads_the_logged_value() {
        // An expression override `Sensor = Sensor * 1.05` reads the LOGGED Sensor at
        // each tick (seeded just before the override is applied), not a circular
        // reference to the override in progress. At t=0 the logged Sensor is 10, so
        // the override pins Sensor = 10.5; the cone recomputes Mid = 21, Result = 22.
        let loaded = counterfactual();
        let log = consistent_log();
        let overrides =
            vec![Override::parse("Root.CF.Sensor=Root.CF.Sensor * 1.05").expect("parses")];
        let cfg = CounterfactualCfg {
            base_rate_hz: 100.0,
            duration_s: 0.01,
        };
        let trace = run_counterfactual(&loaded, &log, &overrides, &cfg).expect("expr cf runs");
        assert_eq!(floats(&trace, "Root.CF.Sensor"), vec![10.5]);
        assert_eq!(floats(&trace, "Root.CF.Mid"), vec![21.0]);
        assert_eq!(floats(&trace, "Root.CF.Result"), vec![22.0]);
    }

    #[test]
    fn counterfactual_is_deterministic() {
        // Same log + same overrides -> identical trace (no wall-clock, no RNG).
        let loaded = counterfactual();
        let log = consistent_log();
        let overrides = vec![Override::parse("Root.CF.Sensor=12.5").expect("parses")];
        let cfg = CounterfactualCfg {
            base_rate_hz: 100.0,
            duration_s: 0.03,
        };
        let a = run_counterfactual(&loaded, &log, &overrides, &cfg).expect("run a");
        let b = run_counterfactual(&loaded, &log, &overrides, &cfg).expect("run b");
        assert_eq!(a.time, b.time);
        assert_eq!(a.channels, b.channels);
    }
}
