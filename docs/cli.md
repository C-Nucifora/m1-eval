<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
# m1-eval — CLI reference

`m1-eval` is a thin command-line shell over the evaluator library. It loads a
project (and optional `.m1cfg` calibration), then either evaluates a scenario
into a `Trace` or prints the static coverage report.

```
m1-eval [--project P] [--config C]
        [--scenario S [--function F | --target CH] [--out trace.json|trace.csv]]
        [--coverage]
```

## Flags

| Flag | Meaning |
| --- | --- |
| `--project <PATH>` | The `Project.m1prj`. Defaults to the nearest one upward from the cwd, or `$M1_PROJECT`. |
| `--config <PATH>` | The calibration file (`.m1cfg`) supplying parameter values and table cells. Required for any run that reads a parameter or does a table `.Lookup()`. |
| `--scenario <PATH>` | The scenario file (TOML or JSON; parser chosen by extension) describing how to drive the run. |
| `--function <NAME>` | Override the scenario's mode: run this single function each tick. Mutually exclusive with `--target`. |
| `--target <CHANNEL>` | Override the scenario's mode: run this target channel plus its upstream dependency cone. Mutually exclusive with `--function`. |
| `--out <PATH>` | Where to write the trace. Format follows the extension: `.csv` writes CSV, anything else (including `.json`) writes JSON. Without `--out`, the trace prints to stdout as JSON. |
| `--coverage` | Print the coverage report (supported / stubbed / unsupported builtins and constructs) instead of, or alongside, a run. |
| `--version`, `-V` | Print the version and exit `0`. |
| `--help`, `-h` | Print usage and exit `0`. |

A run requires either `--scenario` (to evaluate) or `--coverage` (to report);
with neither, the invocation is incomplete and exits `2`. `--function` /
`--target` override the `mode`/`target` declared in the scenario file.

## Scenario file

The primary format is TOML; JSON of the same shape is also accepted.

```toml
mode = "function"            # or "cone"
target = "Engine.Update"     # function name (function mode) or channel (cone mode)
duration_s = 1.0             # run length in seconds; ticks span [0, duration_s)
base_rate_hz = 100.0         # tick rate; dt = 1 / base_rate_hz

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

## Not yet — Phase 3

Log-driven **counterfactual replay** — `--log <L.csv|L.ld>` to import a recorded
run as ground truth and `--override CH=expr` to pin a channel and re-evaluate only
its downstream cone, diffing against the log — is **Phase 3 and not implemented
yet**. CSV/`.ld` log import and the override/diff machinery are not wired into the
CLI in Phase 1.
