<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
# m1-eval

A stepped, deterministic **evaluator/interpreter for the MoTeC M1 scripting
language** (`.m1scr`). The rest of the toolchain can parse (`m1-core`) and
type-check (`m1-typecheck`) M1; `m1-eval` adds the missing layer — it actually
*runs* the scripts. Given a *scenario* (input channel/parameter values over
time) it evaluates a project's expressions, table lookups, and stateful
time-domain operators to produce **real numeric channel values over time**.

It is built primarily as a **Rust library** (consumed by `m1-visualiser`, and
later `m1-lsp`), with a thin CLI on top. The same engine drives a per-channel and
per-expression value `Trace` that the visualiser overlays on a dependency graph.

## What it does (Phase 1)

This is **Phase 1** of the engine: the core plus the single-function and
dependency-cone runners.

- **Expression & statement evaluation** — operators (arithmetic, comparison,
  logical, bitwise), ternary, member access, enums, `if/else`, `when/is`,
  `expand/to`, `local` / `static local`.
- **Table lookup** — 1/2/3-D linear interpolation over `.m1cfg` calibration
  cells, with clamping at the axis edges.
- **Tier-1 pure builtins** — `Calculate.*`, `Limit.*`, `Convert.*`, table
  `.Lookup()`.
- **Tier-2 stateful builtins** (the hard core) — `Filter.FirstOrder`,
  `Filter.{Maximum,Minimum}`, `Integral.Normal`, `Derivative.*`, `Debounce.*`,
  `Delay.*`, `Change.*`, timers, and `static local` persistence. Each is a small
  state machine keyed by call-site and advanced by an explicit `dt`.
- **Tier-3 IO** — `CanComms.*`, `Serial.*`, `System.*`, `Logging.*` are
  **stubbed or scenario-fed**: they return a scenario-provided value or a
  documented stub, and the channel is flagged "externally driven" in the trace.
- **Two runners** — *single-function* (run one chosen function each tick over a
  time series) and *dependency-cone* (run a target channel plus its upstream
  cone, topologically ordered).
- **Scenarios** — TOML/JSON describing the run mode, time grid
  (`duration_s` + `base_rate_hz`), and input sources (constants or `(t, value)`
  time series), with an optional CSV time-series sidecar.

## What it adds (Phase 2 — the whole-project multi-rate scheduler)

**Phase 2** turns the engine into a faithful **mini-ECU**: instead of running one
function or one dependency cone, the `whole-project` mode runs *every*
periodically-scheduled function each tick at its own rate, over a fixed duration,
producing one deterministic `Trace`. Select it with `mode = "whole-project"` in
the scenario or the `--whole-project` CLI flag (which overrides the scenario's
mode and is mutually exclusive with `--function` / `--target`).

The multi-rate model:

- **Schedule from the project.** A function's execution rate is its
  `.m1prj` trigger — a `BuiltIn.EventKernel` clock such as `On 500Hz` /
  `On 50Hz` — surfaced by `m1-typecheck` as the symbol's `call_rate_hz`. Every
  function with a resolvable periodic rate (500 / 200 / 50 / 10 / 2 Hz) is
  scheduled; an `On Startup` or untriggered function (rate `None`) is **not** run
  by the scheduler and is flagged *unscheduled* in `--coverage`.
- **Base tick + rate divisors.** The run advances on one base tick grid
  (`base_rate_hz`, defaulting to the fastest scheduled rate when unset). Each
  function runs every `base_rate_hz / rate_hz` ticks: a 100 Hz function on a
  100 Hz base runs every tick, a 50 Hz function every other tick.
- **Rate-correct `dt`.** A function's stateful operators (`Integral.Normal`,
  filters, derivatives, timers) are stepped by *its own* period
  (`1 / rate_hz`) — a 50 Hz integrator accumulates with `dt = 0.02`, not the
  base `dt` — so time-domain results are faithful to the real schedule.
- **Zero-order hold between ticks.** A channel a function did not write this
  tick keeps its last value (the shared value store carries it forward), so a
  slow channel holds steady between its updates while fast channels move every
  tick.
- **Same-rate dependency ordering, cross-rate stale-tolerance.** Within one
  rate group, a writer runs before any reader of its output (topological order).
  Across rates, no ordering edge is added: a faster reader of a slower writer
  sees the slower function's *previous* value (stale between writer ticks),
  matching how the real ECU schedule interleaves rate groups. Rate groups are
  run fastest-first within a base tick.
- **Externally-driven IO still stubbed, still fail-loud.** CAN/sensor reads
  fall back to their documented stubs (flagged externally driven in the trace);
  any genuinely unsupported construct still aborts the run rather than guessing.

### Determinism & fail-loud

- **Deterministic.** A fixed tick grid and explicit `dt`, no wall-clock and no
  RNG: the same scenario always produces the same `Trace`.
- **Fail-loud.** The evaluator never substitutes a guessed or default number. An
  unimplemented builtin, an unsupported construct, a missing calibration value,
  an unresolved symbol, or a missing scenario input all surface as an error and
  abort the run — never a silently-wrong value.

### `--coverage`

Before running, `m1-eval --coverage` reports, per project, which builtins and
constructs each script uses and whether the engine **supports** them faithfully,
**stubs** them (Tier-3 IO, externally driven), or does **not support** them
(would fail loud at runtime). This tells you up front what is trustworthy versus
externally driven.

The report also prints a **`Schedule:`** section: every script-backed function
with its execution rate (`@ 500 Hz`, `@ 50 Hz`, …), or *unscheduled* for a
function with no periodic trigger. This makes a `whole-project` run transparent —
you see exactly which functions the scheduler will run, at what rate, and which
are excluded — before you run it.

## Usage

```sh
# Evaluate a scenario and write the trace as JSON (or .csv — format follows the
# extension; omit --out to print JSON to stdout).
m1-eval --project Project.m1prj --config parameters.m1cfg \
        --scenario scenario.toml --out trace.json

# Override the scenario's mode from the CLI (mutually exclusive with each other).
m1-eval --project Project.m1prj --scenario scenario.toml --function Engine.Update
m1-eval --project Project.m1prj --scenario scenario.toml --target  Root.Engine.Power

# Whole-project multi-rate run: every scheduled function at its own rate.
m1-eval --project Project.m1prj --scenario scenario.toml --whole-project --out trace.csv

# Static coverage report — what the engine can and cannot evaluate, plus the
# per-function execution schedule.
m1-eval --project Project.m1prj --coverage
```

`--project` defaults to the nearest `Project.m1prj` upward (or `$M1_PROJECT`).
See [`docs/cli.md`](docs/cli.md) for the full flag list, the scenario file
format, and the exit-code contract.

## Not yet — later phases

`m1-eval` is phased; each phase is independently shippable. Phase 1 (the core +
single-function / dependency-cone runners) and Phase 2 (the whole-project
multi-rate scheduler, above) are built. The following are **not** implemented yet:

- **Phase 3** — **log-driven counterfactual replay**: import a recorded run
  (CSV / `.ld`), treat logged channels as ground truth, override one or more
  channels, re-evaluate only the downstream dependency cone, and diff against the
  log. This is the headline feature, and it is **not yet** built — `.ld` /
  CSV log import and counterfactual `--override` remain Phase 3.
- **Phase 4** — LSP hover-to-evaluate and inline value hints, reusing this
  library.

## License & ecosystem

`m1-eval` is licensed **GPL-3.0-or-later** and is part of the M1 toolchain — see
https://github.com/C-Nucifora/m1-tools. It depends on `m1-core` and
`m1-typecheck` (pinned by git tag) and is consumed by `m1-visualiser` (for its
numeric value overlay).

Semantics for the M1 builtin operators are paraphrased from understanding of how
the language behaves; no proprietary MoTeC manual text is reproduced here, and
all committed fixtures are synthetic.
