<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
# m1-eval

A stepped, deterministic offline **evaluator/interpreter for the MoTeC M1 scripting
language** (`.m1scr`). The rest of the toolchain can parse (`m1-core`) and
type-check (`m1-typecheck`) M1; `m1-eval` adds the missing layer — it actually
*runs* the scripts. Given a *scenario* (input channel/parameter values over
time) it evaluates a project's expressions, table lookups, and stateful
time-domain operators to produce numeric channel values over time.

It is built primarily as a **Rust library** (consumed by `m1-visualiser`, and
later `m1-lsp`), with a thin CLI on top. The same engine drives a per-channel and
per-expression value `Trace` that the visualiser overlays on a dependency graph.

## Maturity contract

These states describe evidence for compatibility with M1, not how much Rust test
coverage a module has.

| State | Meaning |
| --- | --- |
| **Verified** | A committed test compares the result with captured output from M1, M1 Sim, or an ECU. |
| **Assumed** | The behavior is implemented and tested with synthetic fixtures or hand-derived values, but has not been compared with captured M1 output. |
| **Stubbed** | Hardware behavior is not evaluated. An explicit scenario input may supply the value; otherwise the engine returns a documented offline value or a generic type-correct default and marks it externally driven. |
| **Unsupported** | The engine does not implement the behavior. It reports the item through `--coverage` when possible and fails the run if execution reaches it. |

No advertised evaluator area currently meets the **Verified** definition. The
real-project and real-`.ld` tests are completion and structural smoke tests; they
do not compare computed channel values with captured M1 results.

| Advertised area | State | Current contract |
| --- | --- | --- |
| Expressions, statements, enums, and inline user functions | **Assumed** | Covered by unit and synthetic-project tests. M1 evaluation results have not been captured for comparison. |
| `.m1cfg` parameters and 1/2/3-D table lookup | **Assumed** | Parsing, interpolation, and edge clamping use synthetic fixtures. Use the matching calibration file for actual values. Without it, parameters use typed external defaults. Table `.Lookup()` and `.Get()` fail in strict modes; whole-project mode may opt in to an external `0.0` fallback. |
| `Calculate.*`, `Limit.*`, `Convert.*`, enum conversions, core value-object methods, and table methods | **Assumed** | Implemented methods have hand-derived tests. `AsString`, `Validate`, `Constrain`, `GetUnscheduled`, and `Set` resolve against eligible receiver classes; channel `Set` applies its project validation range before writing. `--coverage` remains authoritative for the methods a project uses. |
| Filters, integrals, derivatives, debounce, delay, change detection, timers, and `static local` state | **Assumed** | Update laws and startup behavior are explicit implementation assumptions with hand-derived tests, not M1 value comparisons. |
| `CanComms.*`, `Serial.*`, `System.*`, `Logging.*`, DBC objects, and other hardware-backed calls | **Stubbed** | Scenario `[[io]]` values take precedence. Otherwise calls use documented or generic typed stubs; writes are offline no-ops. |
| Scenario parsing, tick grids, trace output, and `--coverage` | **Assumed** | These are deterministic m1-eval contracts tested with synthetic data. A `Supported` coverage entry means implemented, not M1-verified. |
| Single-function and upstream dependency-cone runners | **Assumed** | Selection, ordering, and zero-order hold are tested on synthetic projects. |
| Whole-project multi-rate scheduling | **Assumed** | Trigger rates come from `Project.m1prj`; same-rate dependency order, fastest-first rate groups, startup order, and cross-rate stale reads are the evaluator's model. |
| CSV log replay, overrides, downstream-cone recomputation, and diffs | **Assumed** | Synthetic tests cover import, resampling, source precedence, recomputation, and the no-op invariant. They do not establish M1 execution fidelity. |
| Binary `.ld` import | **Assumed** | Synthetic decode tests run in CI. An optional real-log test checks structure and numeric plausibility only, not values against an independent oracle. |
| Real-time/HIL execution, ECU budgets or preemption, watchdog behavior, live CAN/serial I/O, `.m1dbc` decoding, unlisted constructs or builtins, and LSP integration | **Unsupported** | These are outside the current evaluator. Unknown executable behavior fails loud rather than being inferred. |

## What it does (Phase 1)

The **Phase 1** foundation: the core evaluator plus the single-function and
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

### Pure numeric builtin behavior

Pure `Calculate` and `Convert` calls use the M1-width scalar model throughout.
The implemented behavior is still **Assumed** under the maturity contract above:

- `Calculate.Bias(a, b, bias)` maps `-1` to the lower argument, `0` to their
  average, and `1` to the higher argument, using binary32 arithmetic.
- `Calculate.Average` returns the joined integral family for its integral
  overload and binary32 for its floating-point overload. An integral half unit
  is discarded toward zero.
- `Calculate.MaximumFloat()` returns the largest finite binary32 value.
- `Convert.ToInteger` and `Convert.ToUnsignedInteger` round to nearest, with
  halfway cases away from zero. Results clamp to the destination integer range;
  in particular, a negative value converted to unsigned becomes zero.
- `Convert.ToFixed7DP` follows the pinned integral signature and converts by
  numeric value, not by reinterpreting bits. An input of `1` becomes `1.0000000`.
  Whole-number inputs `-214..=214` fit the documented signed 32-bit, seven-place
  representation; values immediately outside that domain fail with a range
  error.

## What it adds (Phase 2 — the whole-project multi-rate scheduler)

**Phase 2** adds the whole-project multi-rate scheduler: instead of running one
function or one dependency cone, the `whole-project` mode runs *every*
periodically-scheduled function each tick at its own rate, over a fixed duration,
producing one deterministic `Trace`. It models the ECU's *schedule*, not the
ECU itself — execution budgets, stalls, preemption, and watchdog effects are
out of scope. Select it with `mode = "whole-project"` in
the scenario or the `--whole-project` CLI flag (which overrides the scenario's
mode and is mutually exclusive with `--function` / `--target`).

The multi-rate model:

- **Schedule from the project.** A function's execution rate is its
  `.m1prj` `SelectedTrigger`, which must resolve to a `BuiltIn.EventKernel`
  clock such as `On 500Hz` or `On 50Hz`. The resolver handles absolute paths,
  group-relative `Parent.` paths, and `$(<component>:SelectedTrigger)` attribute
  references. An `On Startup` function runs **exactly once** before the first
  periodic tick, and its outputs hold from tick 0. Parameterised user functions
  and calibration functions remain callable helpers. Functions with no trigger
  stay unscheduled. Invalid or dangling trigger references are excluded and
  reported with the failed path or attribute in `--coverage`.
- **Base tick + exact rate divisors.** The run advances on one base tick grid.
  When `base_rate_hz` is unset it defaults to the **least common multiple** of
  the scheduled rates, so every function has an exact integer tick period —
  rates {500, 200} Hz derive a 1000 Hz base, never a rounded 2.5-tick period. A
  pinned base that cannot represent every scheduled rate exactly (or is below
  the fastest rate) is **rejected loudly** rather than rounded. Each function
  then runs every `base_rate_hz / rate_hz` ticks: a 100 Hz function on a 100 Hz
  base runs every tick, a 50 Hz function every other tick.
- **Rate-correct `dt`.** A function's stateful operators (`Integral.Normal`,
  filters, derivatives, timers) are stepped by *its own* period
  (`1 / rate_hz`) — a 50 Hz integrator accumulates with `dt = 0.02`, not the
  base `dt`, so time-domain operators use the configured function period.
- **Zero-order hold between ticks.** A channel a function did not write this
  tick keeps its last value (the shared value store carries it forward), so a
  slow channel holds steady between its updates while fast channels move every
  tick.
- **Same-rate dependency ordering, cross-rate stale-tolerance.** Within one
  rate group, a writer runs before any reader of its output (topological order).
  Across rates, no ordering edge is added: a faster reader of a slower writer
  sees the slower function's *previous* value (stale between writer ticks).
  This ordering is an explicit evaluator assumption. Rate groups run
  fastest-first within a base tick.
- **Externally-driven IO still stubbed, still fail-loud.** CAN/sensor reads
  fall back to their documented stubs (flagged externally driven in the trace);
  any genuinely unsupported construct still aborts the run rather than guessing.

### Determinism & fail-loud

- **Deterministic.** A fixed tick grid and explicit `dt`, no wall-clock and no
  RNG: the same scenario always produces the same `Trace`.
- **Strict channel inputs.** An unimplemented builtin, an unsupported construct,
  an unresolved symbol, or an unseeded ordinary channel aborts the run by
  default. Hardware-backed calls follow the **Stubbed** contract above, and an
  unseeded parameter uses its type-correct calibration default. Whole-project
  mode may also opt in to
  **`allow_default_inputs`** (scenario field or `--allow-default-inputs`):
  unseeded channel reads then fall back to their type-correct startup defaults,
  and every ordinary-channel substitution is reported (channel, substituted
  value, first reading script). Missing table calibration follows a separate
  fallback described in Quickstart. Arithmetic errors are not inputs: integer
  division/modulo by zero always fails loud, opt-in or not.

### `--coverage`

Before running, `m1-eval --coverage` reports, per project, which builtins and
constructs each script uses and whether the engine dispatches them through a
**direct implementation**, an explicit offline **model**, a hardware **stub**
(Tier-3 IO, externally driven), or no implementation (would fail loud at
runtime). The rendered labels are `Supported`, `Assumed`, `Stubbed`, and
`Unsupported`, respectively. These are execution-route labels, separate from the
evidence maturity contract above: both `Supported` and `Assumed` coverage entries
remain **Assumed** maturity until captured M1 output verifies them.

The report also prints a **`Schedule:`** section: every script-backed function
with its execution rate (`@ 500 Hz`, `@ 50 Hz`, …), `startup, runs once`, or
an explicit `helper`, `unscheduled`, or `unresolved trigger` status. The last
status includes the failed path or attribute, so a project author can repair the
selection instead of treating every excluded function as the same case.

Core value-object calls are receiver-aware. Enum values provide `AsInteger()`
and `AsString()`. Numeric channels, parameters, tables, and value compounds
provide `Validate(v)` and `Constrain(v)`. `MinMax` project validation is
inclusive; each endpoint is converted to the argument's M1 scalar family, and
the legacy `Positive` rule is treated as a lower bound of zero. `Constrain`
retains that family. `Set(v)` is available to channels and channel-backed value
compounds; it converts to the writable channel's family, then applies the same
range before it writes. Parameters remain calibration-owned. Channels and
tuning tables provide `GetUnscheduled()`, which performs an ordinary runtime
read but deliberately adds no scheduler dependency. Unknown validation kinds
fail if execution reaches one, and unsupported receiver/method pairs name the
exact call.

## Quickstart

`m1-eval` runs offline. It does not connect to an ECU, sample sensors, emulate a
CAN or serial bus, or reproduce firmware timing. A `Project.m1prj` is always
required because it supplies symbols, types, and trigger rates. Pass the matching
`.m1cfg` when a run needs real parameter values or table cells. Without it, an
unseeded parameter uses a type-correct externally-driven default. Table
`.Lookup()` and `.Get()` fail in strict modes. In whole-project mode,
`allow_default_inputs` also permits an externally-driven `0.0` table fallback.
The evaluator does not load `.m1dbc` files.

Seed an ordinary hardware channel with scenario `[[inputs]]`. Seed a
hardware-backed builtin call with `[[io]]`, using its exact `Object.Method` name.
Without those seeds, function and cone runs fail on an ordinary missing channel,
while Tier-3 calls use the **Stubbed** contract. Run `--coverage` first to see the
implemented, assumed, stubbed, and unsupported calls in a project.

Whole-project mode is strict about ordinary missing channels unless
`allow_default_inputs = true` or `--allow-default-inputs` is set. The CLI then
prints every substituted ordinary channel, value, and first-reading script to
stderr. Library callers receive the same records in `Trace::defaulted`; the
trace also marks those channels externally driven. Tier-3 stubs,
absent-calibration parameter defaults, and the opt-in table fallback are marked
externally driven, but they are not part of that `allow_default_inputs` stderr
summary. With that opt-in, `.Lookup()` and `.Get()` return the externally-driven
float value `0.0` and add the table path to `Trace::external`; the fallback is
not added to `Trace::defaulted`. Without the opt-in, missing table calibration
remains a fail-loud `MissingCalibration` error.

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

# Counterfactual replay: hold a recorded log as ground truth, override a channel,
# recompute only its downstream cone, and diff against the log.
m1-eval --project Project.m1prj --log run.csv \
        --override "Root.CF.Sensor=5" --out trace.csv --diff diff.csv

# A binary .ld log needs the `ld` feature built in.
cargo run --features ld -- --project Project.m1prj --log run.ld --out trace.csv

# Static coverage report — what the engine can and cannot evaluate, plus the
# per-function execution schedule.
m1-eval --project Project.m1prj --coverage
```

`--project` defaults to the nearest `Project.m1prj` upward (or `$M1_PROJECT`).
See [`docs/cli.md`](docs/cli.md) for the full flag list, the scenario file
format, and the exit-code contract.

## What it adds (Phase 3 — log-driven counterfactual replay)

**Phase 3** is the headline feature. Import a recorded MoTeC run, treat every
logged channel as **ground truth**, **override** one or more channels (a constant
or an expression), re-evaluate **only the downstream dependency cone** of each
override, leave everything else at its logged value, and emit both the new
`Trace` and a **per-channel `Diff` vs the logged series**.

- **Log import.** A `Log` is a set of per-channel time series plus provenance.
  Import is `--log <PATH>`: a `.csv` (always available) or a `.ld` binary log
  (behind the `ld` feature). Each tick samples every logged channel by zero-order
  hold — the same deterministic rule the rest of the engine uses.
- **CSV log schema.** A `time`-first table; column headers are M1 channel paths
  verbatim (spaces allowed); an optional i2-style units row (a non-numeric second
  row) is captured as provenance, not read as values; data rows are
  `t_seconds,value,…`. A non-numeric value cell fails loud. (Full schema in
  [`docs/cli.md`](docs/cli.md).)
- **Override + downstream cone.** `--override CH=expr` (repeatable) pins a channel
  to a constant or an expression. Only the channels *downstream* of an override —
  the forward dependency cone, the mirror of the upstream cone runner — recompute;
  unrelated channels pass through at their logged value. An override expression
  may read the channel's **logged** value (`CH=CH*1.05` means "5% above the log").
- **Cone functions keep their declared rates.** Each cone function runs at its
  project rate (its trigger's Hz) on the replay grid, with its own period as
  `dt` — a 10 Hz state machine in the cone of a 100 Hz replay still runs every
  10th tick with `dt = 0.1 s` under the evaluator's scheduling model. The default
  replay base is the lcm of the project's rates (the smallest grid every rate
  divides exactly); a pinned base that cannot represent a cone rate exactly is
  rejected loudly.
- **Diff.** `--diff <PATH>` writes the per-channel logged-vs-counterfactual delta:
  which channels moved, by how much, and which are unchanged.
- **Source precedence.** calibration < scenario < **log** < **override**.
- **The no-op invariant.** A no-op override (or `--log` with no `--override`)
  reproduces the logged series within tolerance, and the changed-channel set is
  empty. Synthetic regression tests enforce this evaluator invariant.

### The `ld` feature (clean-room `.ld` import)

Binary `.ld` import is gated behind the `ld` cargo feature
(`cargo build --features ld`); without it, an `.ld` log fails loud naming the
feature, and CSV import always works. The `.ld` reader is **clean-room**: built on
the MIT [`motec-i2`](https://crates.io/crates/motec-i2) crate (an independent
reverse-engineering of the `.ld` *file format*) plus public format documentation.
We parse an independently-documented file format operating on the user's own
telemetry — we **never reverse-engineer MoTeC software**, never decompile it, and
**never redistribute MoTeC data**, calibrations, firmware, or sample logs. All
committed fixtures are synthetic (a hand-written CSV and a tiny `.ld` written by
`motec-i2` at test time); real `.ld` testing is env-gated (`M1_EVAL_LOG_DIR`) and
off the default path. Confirm your MoTeC software-licence (EULA) terms before
distributing the `.ld` reader.

## Not yet — later phases

`m1-eval` is phased; each phase is independently shippable. Phase 1 (the core +
single-function / dependency-cone runners), Phase 2 (the whole-project multi-rate
scheduler), and Phase 3 (log-driven counterfactual replay, above) are built. Still
to come:

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
