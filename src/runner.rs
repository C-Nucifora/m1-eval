// SPDX-License-Identifier: GPL-3.0-or-later
//! The runners: the deterministic tick loops that drive a [`Scenario`] over a
//! [`Loaded`] project and produce a [`Trace`].
//!
//! Two runners share one core tick loop:
//!
//! - **single-function** ([`RunMode::Function`]): one chosen function executes
//!   each tick;
//! - **dependency-cone** ([`RunMode::Cone`]): the target channel's upstream cone
//!   of functions executes each tick, in topological (writer-before-reader) order.
//!
//! Every tick `i` runs at instant `t = i / base_rate_hz`, with a fixed step
//! `dt = 1 / base_rate_hz`. The loop, in order, each tick:
//!
//! 1. seeds the value store from the scenario inputs (each resampled at `t`),
//!    then layers the scenario overrides on top — both canonicalised to the
//!    target function's scope so `Speed` and `Root.Demo.Speed` address one key;
//! 2. opens the tick in the trace (extends the time axis);
//! 3. executes the scheduled function(s) through [`crate::stmt::exec_script`];
//! 4. records the run's externally-driven inputs and any unrecorded channel
//!    values into the trace so each column stays aligned to the time axis.
//!
//! Determinism: the grid and `dt` are fixed, inputs are pure functions of `t`,
//! and there is no wall-clock or RNG — the same scenario always yields the same
//! trace.

use crate::env::{Env, StateStore};
use crate::error::EvalError;
use crate::expr::EvalCtx;
use crate::ident::{Target, classify};
use crate::loader::Loaded;
use crate::scenario::{InputSeries, RunMode, Scenario};
use crate::stmt::exec_script;
use crate::summary::io_sets;
use crate::trace::Trace;
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
            let scheduled = resolve_function(loaded, name)?;
            tick_loop(loaded, scenario, &[scheduled])
        }
        RunMode::Cone(target) => {
            let order = build_cone(loaded, target)?;
            tick_loop(loaded, scenario, &order)
        }
        RunMode::WholeProject => {
            // Enumerate every periodically-scheduled function and order it
            // dependency-then-rate. The rate-gated tick loop (per-function rate
            // divisors + rate-correct `dt`) lands in P2-B (Task 13); for now the
            // ordered schedule runs through the shared loop so the mode is
            // functional and the ordering is exercised end-to-end.
            let ordered = build_whole_project_schedule(loaded);
            let plain: Vec<Scheduled> = ordered.into_iter().map(|r| r.sched).collect();
            tick_loop(loaded, scenario, &plain)
        }
    }
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

/// The shared deterministic tick loop over an ordered list of scheduled
/// functions. Single-function mode passes one; cone mode passes the topological
/// order. Each tick seeds inputs, runs each function in order, and records.
fn tick_loop(
    loaded: &Loaded,
    scenario: &Scenario,
    schedule: &[Scheduled],
) -> Result<Trace, EvalError> {
    let dt = 1.0 / scenario.base_rate_hz;
    let ticks = tick_count(scenario.duration_s, scenario.base_rate_hz);

    let mut env = Env::new();
    let mut state = StateStore::new();
    let mut trace = Trace::new();

    // Canonicalise each scenario input/override channel once, against the first
    // scheduled function's scope (all scheduled functions share the project; the
    // group only affects relative names, and inputs are normally absolute).
    let scope_group = schedule.first().and_then(|s| s.group.as_deref());
    let scope_fn = schedule.first().and_then(|s| s.fn_symbol.as_deref());
    let inputs = canonicalise(&scenario.inputs, loaded, scope_group, scope_fn);
    let overrides = canonicalise(&scenario.overrides, loaded, scope_group, scope_fn);

    for i in 0..ticks {
        let t = i as f64 / scenario.base_rate_hz;

        // 1. Seed inputs (resampled at t), then layer overrides on top.
        for (path, series) in &inputs {
            env.set(path.clone(), series.sample(t));
        }
        for (path, series) in &overrides {
            env.set(path.clone(), series.sample(t));
        }

        // 2. Open the tick.
        trace.push_tick(t);

        // 3. Run each scheduled function in order, sharing env/state.
        for sched in schedule {
            let root = sched.script.cst.root();
            let mut ctx = EvalCtx {
                project: &loaded.project,
                calib: &loaded.calib,
                env: &mut env,
                state: &mut state,
                group: sched.group.as_deref(),
                fn_symbol: sched.fn_symbol.as_deref(),
                script_name: &sched.script.name,
                dt,
                scripts: &loaded.scripts,
                depth: 0,
                trace: Some(&mut trace),
            };
            exec_script(&root, &mut ctx)?;
        }

        // 4. Record the seeded inputs/overrides into the trace too, so they appear
        //    as columns aligned to the time axis (the executor only records
        //    assignment targets). Inputs are externally driven, so flag them.
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
    let target_canon =
        match classify(target, None, None, &loaded.project, &no_locals) {
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
            writer.entry(w.clone()).or_insert_with(|| script.name.clone());
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
    use crate::scenario::Scenario;
    use crate::value::Value;
    use std::path::Path;

    fn mini() -> Loaded {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini");
        load(&dir.join("Project.m1prj"), Some(&dir.join("parameters.m1cfg")))
            .expect("mini fixture loads")
    }

    fn multirate() -> Loaded {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/multirate");
        load(&dir.join("Project.m1prj"), None).expect("multirate fixture loads")
    }

    #[test]
    fn enumerate_scheduled_keeps_periodic_excludes_startup() {
        // The multirate fixture has three periodic functions (one 50 Hz, two
        // 100 Hz) plus an On-Startup function whose call_rate_hz is None. The
        // schedule must include exactly the three rated functions and exclude
        // the startup one.
        let loaded = multirate();
        let rated = enumerate_scheduled(&loaded);

        let by_fn: std::collections::HashMap<String, f64> = rated
            .iter()
            .map(|r| (r.sched.fn_symbol.clone().unwrap(), r.rate_hz))
            .collect();

        assert_eq!(by_fn.len(), 3, "exactly three periodic functions: {by_fn:?}");
        assert_eq!(by_fn.get("Root.MR.Slow Writer"), Some(&50.0));
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
        // precede the 50 Hz one.
        let loaded = multirate();
        let rated = enumerate_scheduled(&loaded);
        let rates: Vec<f64> = rated.iter().map(|r| r.rate_hz).collect();
        assert_eq!(rates, vec![100.0, 100.0, 50.0], "fastest-first baseline");
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
        let shared = trace.channels.get("Root.MR.Fast Shared").expect("Fast Shared");
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
        assert!(final_col.iter().all(|v| *v == Value::Float(50.0)), "{final_col:?}");
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
}
