<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
# Conformance fixtures

A conformance fixture replays captured inputs on a fixed tick grid and compares
m1-eval's channel values with an independent result. The file carries the
project paths and SHA-256 hashes needed to reproduce the calculation. Ordinary
CI can therefore run a capture without installing M1 Sim.

The committed fixtures under `tests/fixtures/conformance` are synthetic. They
test the fixture parser and runner, but they are not evidence that m1-eval
matches M1. A fixture counts as M1 Sim evidence only when its provenance says
`kind = "m1-sim"` and its expected values were actually captured from M1 Sim.

## Running fixtures

Pass `--conformance` once per TOML or JSON file. The runner processes files and
steps in command-line order and stops at the first error or output mismatch.

```sh
m1-eval \
  --conformance tests/fixtures/conformance/synthetic-mini.toml \
  --conformance tests/fixtures/conformance/synthetic-initial-state.toml
```

Each fixture carries its own project and optional calibration path, so
`--conformance` cannot be combined with `--project`, `--config`, scenario,
counterfactual, trace-output, or coverage flags.

Use the stricter gate when a private or CI-only suite is expected to contain a
real capture:

```sh
m1-eval --conformance /secure/captures/filter.toml \
  --require-m1-sim-capture
```

The same gate is available to integration tests. Set
`M1_EVAL_M1_SIM_FIXTURES` to an operating-system path list:

```sh
M1_EVAL_M1_SIM_FIXTURES=/secure/captures/filter.toml \
  cargo test --test conformance configured_private_m1_sim_fixtures_form_an_optional_gate
```

Do not commit licensed projects, calibrations, logs, or customer data. Keep a
private fixture outside the repository, or reproduce the behavior in a small
project that you are allowed to publish.

## File layout

This shortened TOML example shows the schema. JSON accepts the same fields.

```toml
schema_version = 1
name = "first-order filter capture"
calculation_rate_hz = 100.0

[project]
root = "../filter-project"
project = "Project.m1prj"
config = "Filter.m1cfg"

# Replace each all-zero example with that file's lowercase SHA-256 digest.
[[project.files]]
path = "Project.m1prj"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"

[[project.files]]
path = "Scripts/Filter.Update.m1scr"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"

[[project.files]]
path = "Filter.m1cfg"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"

[provenance]
kind = "m1-sim"
source = "reduced Filter project, capture session FILTER-2026-08-30"
procedure = "docs/conformance.md#capturing-from-m1-sim"
tool_version = "M1 Build and M1 Sim version used for the capture"
captured_at_utc = "2026-08-30T12:00:00Z"
notes = "Optional setup details that affect the result."

[run]
mode = "function" # "function", "cone", or "whole-project"
target = "Filter.Update"

[[initial_state]]
channel = "Root.Filter.Output"
value = { type = "floating-point", value = "0" }

[[steps]]
time_s = 0.0

[[steps.inputs]]
channel = "Root.Filter.Input"
value = { type = "floating-point", value = "1" }

[[steps.expected]]
channel = "Root.Filter.Output"
value = { type = "floating-point", value = "0.095162585" }
tolerance = { type = "floating-point", absolute = "0.000001", relative = "0.000001" }

[[steps]]
time_s = 0.01

[[steps.expected]]
channel = "Root.Filter.Output"
value = { type = "floating-point", value = "0.18126924" }
tolerance = { type = "floating-point", absolute = "0.000001", relative = "0.000001" }
```

`project.files` must be the exact evaluator input set: the project descriptor,
the selected calibration, and every `.m1scr` below the descriptor's directory.
Paths in that manifest are normalized relative paths below `project.root`.
Symlinks are rejected because the same fixture must hash the same files on every
machine. Duplicate `.m1scr` basenames are rejected because the evaluator loader
identifies scripts by basename and could otherwise depend on directory order.

Initial-state, input, and expected-output paths must be canonical `Channel`
symbols in the loaded project, not parameters, constants, groups, or functions.
Their wire families must match the channel storage families;
the runner rejects a fixture that would rely on an implicit numeric or enum
conversion at the scenario boundary.

Steps start at zero and include every calculation tick. For step index `i`, the
runner requires `time_s = i / calculation_rate_hz`. An input change is held
until its next change. If an input first changes after step zero, its channel
must have an `initial_state` entry. Initial state is seeded once, before startup
code and the first tick. It is not pinned on later ticks, so scripts can advance
captured counters, accumulators, and state channels. Every step declares the
same expected-channel set, which keeps each captured output dense and aligned
with the tick grid. An expected channel must be evaluator-computed. A channel
that only repeats an input, an initial seed, or another external value is
rejected instead of producing a vacuous conformance pass. Expected channels
must be disjoint from fixture input channels. A run that consults an external
stub or source not declared as fixture input or initial state is also rejected.

## Typed values and comparisons

Every value names its M1 family:

| `type` | Fields | Comparison |
| --- | --- | --- |
| `boolean` | `value = true` | Exact |
| `integer` | signed 32-bit `value` | Exact |
| `unsigned-integer` | unsigned 32-bit `value` | Exact |
| `floating-point` | binary32 decimal string in `value` | Declared absolute or relative tolerance |
| `fixed-point-7dps` | signed 32-bit `raw` storage | Declared `raw` tolerance |
| `enum` | `enum_type` and `member` | Exact type and member |
| `string` | `value` | Exact |

Floating-point strings may use `NaN`, `Infinity`, or `-Infinity`. NaN matches
only NaN. Infinity matches only infinity with the same sign. A finite expected
value never matches a non-finite actual value, regardless of tolerance.

For finite floating-point values, the allowed error is:

```text
max(absolute, relative * max(abs(expected), abs(actual)))
```

At least one floating bound must be present. Omitted bounds act as zero.
Fixed-point values compare their signed raw storage and use an integer raw-unit
tolerance. A tolerance on Boolean, integer, unsigned integer, enum, or string
data is an error rather than a silent approximation.

## Capturing from M1 Sim

1. Use a project and calibration you may inspect and share. Reduce it to the
   calculation under test when the original contains private material.
2. Record the exact M1 Build and M1 Sim version, UTC time, project purpose,
   calculation rate, selected function or whole-project schedule, and any setup
   that changes the result.
3. Hash the project descriptor, selected calibration, and all loaded scripts.
   On systems with `sha256sum`, run it on each manifest file and copy the
   lowercase digest into `project.files`.
4. Reset M1 Sim before the capture so filters, integrals, timers, static locals,
   and other hidden operator state start fresh, like the runner's state store.
   If the calculation needs a warm-up, include those warm-up ticks in the
   fixture. `initial_state` seeds channel storage only; it cannot restore hidden
   operator state from a running session.
5. Set the simulator's initial channel state before tick zero. For each tick,
   apply that step's input changes before executing the calculation, then capture
   expected outputs after the calculation completes. Capture tick zero and every
   following calculation tick at the declared rate. Use M1 Sim's logging or
   export facility from the licensed installation. Do not derive the expected
   column with m1-eval.
6. Preserve the source types. Record signed and unsigned integers without
   widening, Boolean values as Boolean, fixed-point values as signed raw storage,
   and floating-point values as binary32 round-trip decimal strings. Declare
   tolerances from the capture's precision and the behavior under test.
7. Run the fixture without the strict flag first. Resolve hash, schema, or value
   errors. Then run it with `--require-m1-sim-capture`.
8. Review the fixture for private names and values before committing it. If it
   cannot be published, keep it in the private gate described above.

The runner reloads the project and creates a fresh evaluator state for each
fixture, including repeated paths in one suite. A failure reports the first
expected channel that differs, with its step index, time, typed expected value,
tolerance, and typed actual value.

## Synthetic runner fixtures

The committed mini fixture checks calibrated arithmetic, typed floating-point
input, tolerances, and bundle hashes. The typed-value fixture carries Boolean,
signed, unsigned, binary32, fixed-point, enum, and string values through the
full parser, evaluator, and comparator. The initial-state fixture starts two
scheduled counters at non-zero values and runs the same fixture twice in one
test. It catches state leakage and accidental per-tick reseeding.

These files use `kind = "synthetic"` and cannot satisfy
`--require-m1-sim-capture`.

## Independent-reference fixtures

`kind = "independent"` records expected values checked against a named public
standard or separate implementation. This is stronger evidence than a
hand-derived runner fixture, but it is not M1 output and cannot satisfy
`--require-m1-sim-capture`. The committed UnixTime fixture uses POSIX epoch and
Gregorian calendar rules and is differentially checked against the Rust `time`
crate; its M1-specific GPS pivot, leap-second normalization, and timezone
encoding remain documented assumptions pending a genuine M1 Sim capture.
