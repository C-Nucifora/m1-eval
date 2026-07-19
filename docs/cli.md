<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
# m1-eval — CLI reference

`m1-eval` is a thin command-line shell over the evaluator library. It loads a
project (and optional `.m1cfg` calibration), then either evaluates a scenario
into a `Trace` or prints the static coverage report.

```
m1-eval [--project P] [--config C]
        [--scenario S [--function F | --target CH | --whole-project]
                     [--out trace.json|trace.csv]]
        [--log L.csv|L.ld [--override CH=expr]... [--diff diff.json|diff.csv]
                          [--out trace.json|trace.csv]]
        [--coverage]
```

## Flags

| Flag | Meaning |
| --- | --- |
| `--project <PATH>` | The `Project.m1prj`. Defaults to the nearest one upward from the cwd, or `$M1_PROJECT`. |
| `--config <PATH>` | The calibration file (`.m1cfg`) supplying parameter values and table cells. Required for any run that reads a parameter or does a table `.Lookup()`. |
| `--scenario <PATH>` | The scenario file (TOML or JSON; parser chosen by extension) describing how to drive the run. |
| `--function <NAME>` | Override the scenario's mode: run this single function each tick. Mutually exclusive with `--target` and `--whole-project`. |
| `--target <CHANNEL>` | Override the scenario's mode: run this target channel plus its upstream dependency cone. Mutually exclusive with `--function` and `--whole-project`. |
| `--whole-project` | Override the scenario's mode: run the whole-project multi-rate scheduler (every periodically-scheduled function at its own rate). Mutually exclusive with `--function` and `--target`. |
| `--out <PATH>` | Where to write the trace. Format follows the extension: `.csv` writes CSV, anything else (including `.json`) writes JSON. Without `--out`, the trace prints to stdout as JSON. |
| `--log <PATH>` | Counterfactual replay: a recorded MoTeC log held as ground truth (`.csv`, or `.ld` with `--features ld`). Triggers a counterfactual run instead of a scenario run. |
| `--override <CH=expr>` | Pin a logged channel to a constant or expression for the counterfactual run, recomputing only its downstream cone. Repeatable (override several channels). Requires `--log`. |
| `--diff <PATH>` | Where to write the per-channel logged-vs-counterfactual delta. Format follows the extension (`.csv` / `.json`). Requires `--log`. |
| `--coverage` | Print the coverage report (supported / stubbed / unsupported builtins and constructs, plus the per-function execution `Schedule:`) instead of, or alongside, a run. |
| `--version`, `-V` | Print the version and exit `0`. |
| `--help`, `-h` | Print usage and exit `0`. |

A run requires `--scenario` (to evaluate), `--log` (to replay a log), or
`--coverage` (to report); with none, the invocation is incomplete and exits `2`.
`--function` / `--target` / `--whole-project` override the `mode`/`target`
declared in the scenario file; at most one may be given (combining two is a usage
error, exit `2`). `--override` and `--diff` require `--log`.

## Scenario file

The primary format is TOML; JSON of the same shape is also accepted.

```toml
mode = "function"            # "function", "cone", or "whole-project"
target = "Engine.Update"     # function name (function mode) or channel (cone mode);
                             # omitted/ignored in whole-project mode
duration_s = 1.0             # run length in seconds; ticks span [0, duration_s)
base_rate_hz = 100.0         # base tick rate; dt = 1 / base_rate_hz. In
                             # whole-project mode this is the base grid and each
                             # function runs every base_rate_hz / rate_hz ticks
                             # (must divide exactly — an inexact ratio is rejected);
                             # when 0/absent it defaults to the lcm of the scheduled rates

# Inputs the engine is *given* rather than computes. Each entry is a constant
# or a (t_seconds, value) time series sampled by zero-order hold.
[[inputs]]
channel = "Root.Engine.Gain"
const = 2.5

[[inputs]]
channel = "Root.Engine.Speed"
series = [[0.0, 0.0], [0.5, 4000.0]]

# Overrides pin a channel over the top of inputs and any computed value.
[[overrides]]
channel = "Root.Engine.Output"
const = 0.0
```

Identifiers may contain spaces (e.g. `Cooling Fan.Output`); channel names are
used verbatim and never split on whitespace, only on `.` for path segments.

## Output

- **JSON** (`--out trace.json`, or no `--out`):
  `{ "time": [...], "channels": { path: [...] }, "external": [...] }`. The
  `external` list names channels whose values were externally driven (scenario-fed
  or a Tier-3 stub) rather than computed.
- **CSV** (`--out trace.csv`): a `time` header column followed by one column per
  channel in sorted-name order, one row per tick.

Both are deterministic: the same scenario always produces byte-identical output.

## Exit codes

These follow the shared toolchain contract (`m1-tools/docs/cli.md`):

| Code | Meaning |
| --- | --- |
| `0` | Success — the run produced a trace, or the coverage report printed. |
| `1` | The engine ran and **has something to report**: a project/calibration that would not load, a scenario that would not parse, or a fail-loud evaluation error (an unsupported builtin, a missing calibration value, an unresolved symbol, a missing input). |
| `2` | A **usage error** — an unrecognised flag, no resolvable project, or neither `--scenario` nor `--coverage` given. |

So `$? != 0` means "do not trust the output." The engine **fails loud**: it never
emits a guessed or default number in place of something it cannot evaluate.

## Counterfactual replay (`--log` / `--override` / `--diff`)

`--log` imports a recorded run as **ground truth**: every logged channel is held
at its logged value, sampled onto the tick grid by zero-order hold. `--override`
then pins one or more channels to a constant or an expression, and the engine
re-evaluates **only the downstream dependency cone** of the overridden channels —
everything else passes through at its logged value. `--diff` writes the
per-channel logged-vs-counterfactual delta.

```sh
# Replay a CSV log, push Sensor to 5, recompute its downstream cone, write the
# counterfactual trace and the per-channel diff.
m1-eval --project Project.m1prj --log run.csv \
        --override "Root.CF.Sensor=5" --out trace.csv --diff diff.csv

# An override may be an expression that reads the *logged* value of the channel:
# "5% above the logged Sensor". --override is repeatable.
m1-eval --project Project.m1prj --log run.csv \
        --override "Root.CF.Sensor=Sensor * 1.05" --override "Root.CF.Gain=2.0"

# A binary .ld log needs the `ld` feature at build time.
cargo run --features ld -- --project Project.m1prj --log run.ld --out trace.csv
```

**Source precedence** (lowest to highest): calibration < scenario < **log** <
**override**. A logged channel overrides any scenario input; an `--override`
overrides the log.

**The no-op invariant.** `--log` with no `--override` (or an identity override
like `CH=CH`) reproduces the logged series within floating-point tolerance, and
the diff's changed-channel set is empty. This is the load-bearing correctness
guarantee of the whole pipeline.

**Fail-loud.** A malformed log, a non-numeric value cell, an override of a
channel that no in-project function reads (nothing downstream to recompute), or an
`.ld` log without the `ld` feature each surface a fail-loud error and exit `1` —
never a guessed value.

### CSV log schema

A log CSV is a `time`-first table, the same shape the scenario CSV sidecar uses,
with one documented extension (a units row):

- **Row 1 (header):** `time,<channel name>,<channel name>,…`. The first column
  header MUST be `time` (case-insensitive). Channel headers are M1 channel paths
  verbatim — identifiers may contain spaces (`Engine Speed`), so names are split
  only on `.`, never on whitespace. RFC-4180 quoting applies.
- **Optional row 2 (units):** if the second row's first cell is *non-numeric*
  (e.g. `s,rpm,km/h`), it is treated as a units header and recorded as
  provenance — not as a value row (matching real i2 exports). A numeric first cell
  means there is no units row.
- **Data rows:** `t_seconds,value,value,…`. `time` is ascending seconds; numeric
  cells are values; an empty cell adds no keyframe (the zero-order hold keeps the
  prior value). A non-numeric value cell (outside the units row) fails loud.
- **Resampling:** at each tick the channel is sampled by zero-order hold — the
  deterministic rule used throughout the engine.

### The `ld` binary-log feature

`.ld` import is gated behind the `ld` cargo feature (`cargo build --features ld`).
Without the feature, `--log run.ld` fails loud naming the feature to rebuild with;
CSV import always works with no feature.

The `.ld` reader is **clean-room**: it is built on the MIT `motec-i2` crate (an
independent reverse-engineering of the `.ld` *file format*) plus public format
documentation. We parse an independently-documented file format operating on the
user's own telemetry — we never reverse-engineer MoTeC *software*, never decompile
it, and never redistribute MoTeC data, calibrations, firmware, or sample logs. The
committed CI fixtures are synthetic (a hand-written CSV and a tiny `.ld` generated
by the `motec-i2` writer at test time); no proprietary bytes enter the tree.

> **EULA caveat.** MoTeC's software EULA may restrict reverse-engineering of its
> *software*; this feature reverse-engineers neither the software nor your data —
> only the file format, via an independent third-party crate. Even so, confirm
> your specific MoTeC software-licence terms before distributing the `.ld` reader.

### Testing against real telemetry (`M1_EVAL_LOG_DIR`)

The committed tests run entirely on synthetic fixtures. A real-`.ld` smoke test
(`tests/ld_smoke.rs`) is **env-gated and `#[ignore]`-by-default**, mirroring the
EV-M1 project smoke (`M1_EVAL_EVM1_DIR`). Point `M1_EVAL_LOG_DIR` at a directory
of real `.ld` files and run it explicitly:

```sh
M1_EVAL_LOG_DIR=/path/to/logs \
  cargo test --features ld --test ld_smoke -- --ignored
```

It loads the first `.ld` found and asserts only on *shape*: the header parses (a
MoTeC M1/M150-class device, channel count `> 0`) and at least one channel decodes
to a finite engineering value over a non-empty time grid. No channel name, unit,
or value is hard-coded, so nothing about the proprietary log enters the tree.
