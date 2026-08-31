<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
# m1-eval — CLI reference

`m1-eval` is a thin command-line shell over the evaluator library. It loads a
project and evaluates a scenario, replays a counterfactual log, reports static
coverage, or runs typed conformance fixtures.

```
m1-eval [--project P] [--config C]
        [--scenario S [--function F | --target CH | --whole-project]
                     [--out trace.json|trace.csv]]
        [--log L.csv|L.ld [--override CH=expr]... [--diff diff.json|diff.csv]
                          [--out trace.json|trace.csv]]
        [--coverage]
m1-eval --conformance FIXTURE [--conformance FIXTURE]...
        [--require-m1-sim-capture]
```

## Flags

| Flag | Meaning |
| --- | --- |
| `--conformance <PATH>` | Run a typed TOML or JSON golden-vector fixture. Repeatable. The fixture supplies its project paths and hashes, so this action conflicts with the ordinary project/run/report flags. |
| `--require-m1-sim-capture` | Fail a conformance suite that contains no passing fixture with `m1-sim` provenance. Requires `--conformance`. |
| `--project <PATH>` | The `Project.m1prj`. Defaults to the nearest one upward from the cwd, or `$M1_PROJECT`. |
| `--config <PATH>` | The calibration file (`.m1cfg`) supplying parameter values and table cells. Required for actual tunable and table values. Without it, parameters use externally-driven type defaults; table reads fail unless whole-project mode opts in to default inputs, which supplies an external `0.0`. |
| `--scenario <PATH>` | The scenario file (TOML or JSON; parser chosen by extension) describing how to drive the run. |
| `--function <NAME>` | Override the scenario's mode: run this single function each tick. Mutually exclusive with `--target` and `--whole-project`. |
| `--target <CHANNEL>` | Override the scenario's mode: run this target channel plus its upstream dependency cone. Mutually exclusive with `--function` and `--whole-project`. |
| `--whole-project` | Override the scenario's mode: run the whole-project multi-rate scheduler (every periodically-scheduled function at its own rate; `On Startup` functions run once first). Mutually exclusive with `--function` and `--target`. |
| `--allow-default-inputs` | Whole-project mode: substitute type-correct startup defaults for unseeded channel reads instead of failing loud. Ordinary channel substitutions are reported on stderr. Missing table cells instead produce an external `0.0` recorded only in trace metadata. Strict fail-loud is the default. |
| `--out <PATH>` | Where to write the trace. Format follows the extension: `.csv` writes CSV, anything else (including `.json`) writes JSON. Without `--out`, the trace prints to stdout as JSON. |
| `--log <PATH>` | Counterfactual replay: a recorded MoTeC log held as ground truth (`.csv`, or `.ld` with `--features ld`). Triggers a counterfactual run instead of a scenario run. |
| `--override <CH=expr>` | Pin a logged channel to a constant or expression for the counterfactual run, recomputing only its downstream cone. Repeatable (override several channels). Requires `--log`. |
| `--diff <PATH>` | Where to write the per-channel logged-vs-counterfactual delta. Format follows the extension (`.csv` / `.json`). Requires `--log`. |
| `--coverage` | Print the coverage report (supported / assumed / adapter-backed / stubbed / unsupported builtins and constructs, plus the per-function execution `Schedule:`) instead of, or alongside, a run. |
| `--version`, `-V` | Print the version and exit `0`. |
| `--help`, `-h` | Print usage and exit `0`. |

A normal project action requires `--scenario` (to evaluate), `--log` (to replay
a log), or `--coverage` (to report). A conformance suite is a separate action
and carries its own project bundle. With no action, the invocation is incomplete
and exits `2`.
`--function` / `--target` / `--whole-project` override the `mode`/`target`
declared in the scenario file; at most one may be given (combining two is a usage
error, exit `2`). `--override` and `--diff` require `--log`.

The `Supported`, `Assumed`, `Adapter-backed`, and `Stubbed` buckets distinguish a
direct implementation, an explicit offline model, a typed adapter route, and a
typed offline fallback. An adapter route may use a user `HardwareAdapter` or an
evaluator-owned adapter such as virtual CAN or virtual serial. Required external hardware
metadata is only a subset of `Adapter-backed`. These are execution-route labels,
not evidence maturity. `Supported` and `Assumed` remain **Assumed** maturity
until compared with captured M1 output. See the
[README maturity contract](../README.md#maturity-contract) for the evidence
labels and current status of each evaluator area.

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
allow_default_inputs = false # whole-project only: opt-in default substitution
                             # for unseeded channel reads (reported; strict
                             # fail-loud when absent/false)

# Seed a channel once before startup code and tick zero. Unlike an input, this
# is not written back every tick, so a script may advance it.
[[initial_state]]
channel = "Root.Engine.Counter"
value = { integer = 10 }

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

# IO overrides drive a hardware-backed builtin call directly: the value the
# call returns, keyed by its "Object.Method" spelling. The entry below is a
# wildcard for every matching call site and is resampled every tick.
[[io]]
call = "DBC PC.Dash Switches.Receive"
const = true

[[io]]
call = "System.FlashSize"
const = 8388608

# Add both `script` and `offset` to select one exact CallSite. An exact entry
# wins over the wildcard for the same call. The offset is the call expression's
# zero-based UTF-8 byte offset in the named script.
[[io]]
call = "System.FlashSize"
script = "Engine.Update.m1scr"
offset = 418
const = 4194304

# FlashSize and FlashFree are required metadata. Supply them in the scenario;
# unlike ordinary hardware reads, neither falls back to zero.
[[io]]
call = "System.FlashFree"
const = 1048576

# One-shot virtual RS232 input. Chunks become available when evaluator time
# reaches time_s. Equal-time chunks retain declaration order.
[[serial.rx]]
time_s = 0.125
port = 0
bytes = [0x1b, 0x30, 0x35, 0x0d]

# One classic-CAN frame in wire-byte order. `extended` defaults to false.
[[can.rx]]
time_s = 0.125
bus = 0
id = 0x123
extended = false
bytes = [0x12, 0x34]
```

Whole-project mode shares one virtual adapter between `On Startup` and periodic
functions. Function and cone modes skip startup, so their selected call chain
must initialize the port before virtual receive or transmit calls. A serial RX
declaration schedules bytes but does not initialize a port. Counterfactual
replay has neither scenario RX declarations nor a startup pass. See
[`virtual-serial.md`](virtual-serial.md) for state ownership and mixed-route
failure rules.

The virtual CAN model follows the same run ownership boundary. Function and cone
modes must initialize a raw bus or DBC module in their selected call chain.
Counterfactual replay has fresh CAN state, no startup pass, and no scenario CAN
frames. See [`virtual-can.md`](virtual-can.md) for exact methods, bit addressing,
DBC layouts, and route ownership.

Identifiers may contain spaces (e.g. `Cooling Fan.Output`); channel names are
used verbatim and never split on whitespace, only on `.` for path segments.

## Conformance fixtures

Conformance fixtures use a stricter typed wire format than scenarios. They
record project hashes, provenance, the calculation rate, initial state, every
tick's input changes, expected outputs, and type-specific tolerances. The runner
starts with fresh evaluator state for every fixture and reports the first
meaningful mismatch.

See [`conformance.md`](conformance.md) for the schema, comparison rules, M1 Sim
capture procedure, synthetic examples, and the optional private-capture gate.
Table captures also follow the stricter vector and corpus checks in
[`table-conformance.md`](table-conformance.md).

## Output

- **JSON** (`--out trace.json`, or no `--out`):
  `{ "time": [...], "channels": { path: [...] }, "external": [...], "hardware": [...], "serial": [...], "can": [...] }`.
  Each `hardware` record names the resolved receiver, source spelling, method,
  script, byte offset, and selected route (`scenario-exact`,
  `scenario-wildcard`, `adapter`, `virtual-can`, `virtual-can-rx`, `virtual-serial`, `virtual-serial-rx`,
  `system-model`, or `generic-stub`). Each ordered `serial` record contains the
  RX/TX direction, evaluator time and phase, base tick, port, stable handle,
  bytes, script, and call offset. Each ordered `can` record contains RX/TX
  direction, evaluator time and phase, base tick, bus, ID and format, wire
  bytes, optional handle, optional exact DBC message identity, script, and call
  offset. The
  `external` list names channels whose values were externally driven rather than
  computed, including scenario inputs, held initial state, Tier-3 stubs,
  parameter defaults, opt-in table fallbacks, and opt-in defaults for unseeded
  channels. JSON has no non-finite numeric values, so NaN and positive or
  negative infinity are written as `null`.
- **CSV** (`--out trace.csv`): a `time` header column followed by one column per
  channel in sorted-name order, one row per tick. Serial byte and CAN frame
  events are JSON metadata and are deliberately absent from CSV.

The virtual serial model is fresh for each run. Its complete method and error
contract is documented in [`virtual-serial.md`](virtual-serial.md).
The virtual CAN model is also fresh per run; see
[`virtual-can.md`](virtual-can.md).

Both are deterministic: the same scenario always produces byte-identical output.

## Exit codes

These follow the shared toolchain contract (`m1-tools/docs/cli.md`):

| Code | Meaning |
| --- | --- |
| `0` | Success. The run produced a trace, the coverage report printed, or every conformance fixture passed. |
| `1` | The engine has something to report: a load, parse, integrity, evaluation, or conformance mismatch, including missing hardware metadata. |
| `2` | A usage error: an unrecognised or conflicting flag, no resolvable project for a normal action, or no action. |

So `$? != 0` means "do not trust the output." Unsupported behavior fails loud.
The documented hardware stubs, parameter defaults, opt-in table fallbacks, and
opt-in unseeded-channel defaults are exceptions. The trace marks them externally
driven. The CLI reports each ordinary unseeded-channel substitution on stderr;
the table fallback appears only in the trace's external metadata.

A fail-loud evaluation error also says **where** it happened — the failing
script and the tick instant (`in ECU.Update.m1scr at t = 0.125 s: type error:
division or modulo by zero`, or `at startup` for the once-only initialisation
pass) — so a multi-script whole-project abort points at the script to inspect.

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
the diff's changed-channel set is empty. Synthetic regression tests enforce this
evaluator invariant; it is not a comparison with an M1 execution result.

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
