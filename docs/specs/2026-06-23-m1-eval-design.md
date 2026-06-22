# m1-eval — Design Spec

**Date:** 2026-06-23
**Status:** Approved design, pre-implementation
**Companion tool:** `m1-visualiser` (separate spec — depends on this engine for its value-overlay feature)

## Summary

`m1-eval` is a new tool for the M1 toolchain: an **evaluator/interpreter for the MoTeC
M1 scripting language** (`.m1scr`). Given a *scenario* (input channel/parameter values,
optionally driven by a recorded log), it evaluates the project's scripts — expressions,
tables, and stateful/time-domain operators — to produce **real numeric channel values
over time**. The toolchain can already parse (`tree-sitter-m1`/`m1-core`) and type-check
(`m1-typecheck`) M1; it has no *evaluation* layer. `m1-eval` adds it.

It is built primarily as a **Rust library** (consumed by `m1-visualiser` and, later,
`m1-lsp`), with a thin CLI on top. The headline capability is **counterfactual replay**:
load a real MoTeC log, override an underlying channel, and re-evaluate everything
downstream to see the effect ripple through the dependency chain.

This spec covers `m1-eval` only. `m1-visualiser` (the graph/visualisation tool that was
the original motivating request) is specified separately and consumes this engine.

## Background & context

The work began as a single idea — a tool to graph and visualise dependencies and links
(lookup tables, etc.) in M1 projects. Scoping revealed that the "simulate input values"
requirement implies **real numeric evaluation**, which the toolchain lacks. That is a
substantial, independently-useful capability, so the work was decomposed into two tools:

```
m1-core + m1-typecheck   (existing: parse, symbols, resolve, taint, schedule)
        ^                         ^
     m1-eval  ----------------> m1-visualiser
   (this spec)   value overlay   (separate spec)
```

- `m1-visualiser`'s **structural** features need only `m1-typecheck` and can be built in
  parallel with this engine.
- The **numeric overlay** (sim-on-graph, counterfactual diff) is the one feature that
  needs `m1-eval`.
- `m1-eval` is independent and useful on its own (CLI + library + later LSP).

### What the language actually requires (key findings)

From surveying the grammar, `m1-typecheck`, the M1 manuals, and ~80 real EV-M1 scripts:

- **It cannot be a single-tick calculator.** Stateful/time-domain constructs are
  pervasive in real scripts (`Delay.Rising` in ~20 of 80 scripts, `Integral.Normal` ~15,
  `Debounce.Stable` ~8, `Filter.FirstOrder` ~5, plus `Derivative`, `Change`,
  `static local`, timers). Their values depend on `dt` and prior state, so the engine
  **must be stepped over time** with persistent per-construct state. This is the
  load-bearing architectural fact.
- **The builtin library is the bulk of the work** (~80+ functions: `Calculate`, `Filter`,
  `Integral`, `Derivative`, `Debounce`, `Delay`, `Limit`, `Convert`, `Change`,
  table lookup, plus IO-ish `CanComms`/`Serial`/`System`). Matching MoTeC's exact
  filter/integral/interpolation semantics is the main correctness risk.
- **Some builtins touch hardware** (CAN, Serial, System ticks) and cannot be truly
  evaluated; they are stubbed or fed from the scenario.
- **Table cell values live outside the scripts**, in `.m1cfg` calibration. Real lookups
  require that file as an input.
- **Scripts run as scheduled functions at fixed rates** (500/200/50/10/2 Hz); rate
  determines `dt` and cross-function ordering.
- Rough size of a faithful engine: **~3–5k lines of Rust**, medium-large, with correctness
  risk concentrated in the time-domain math.

The language itself (operators, `if/else`, `when/is`, `expand/to`, `local/static local`,
enums, member access) is small; the stateful builtin library is the hard part. Note that
M1 identifiers may contain spaces (e.g. `Filtering Cutoff Frequency`, `This.Debounce`),
and scripts have side effects (channel writes, `Output.SetState(...)`).

## Goals / Non-goals

### Goals
- Real numeric evaluation of M1 scripts, faithful enough to be trusted for analysis.
- Three execution granularities sharing one core: single-function, dependency-cone,
  whole-project ECU sim.
- Scenario-driven inputs (constants + time series) and **log-driven counterfactual replay**
  (CSV + `.ld`).
- A clean, stable **library API** with per-channel and per-expression value introspection
  (what the visualiser and LSP need).
- Deterministic, reproducible output (golden-testable).
- Transparency: report supported vs stubbed constructs; **fail loud** on the unsupported.

### Non-goals (v1, YAGNI)
- A real-time / hardware-in-the-loop simulator. This is offline, deterministic evaluation.
- Faithful CAN bus / Serial IO emulation. Those are stubbed or scenario-fed.
- Re-implementing or driving MoTeC's GUI tools. (M1 Sim is used only as a validation
  oracle, see Fidelity.)
- Editing/writing `.m1cfg` or `.m1prj`. The engine reads; it does not mutate the project.
- Visualisation. That is `m1-visualiser`'s job.

## Engine approach

A **tree-walking interpreter in Rust** (`m1-eval` crate) that walks `m1-core`'s CST and
uses `m1-typecheck`'s symbol model, name resolution, and value types. Chosen over
transpilation (loses per-node value introspection, which the overlay needs) and over
driving MoTeC's own simulator (GUI/Windows-only, no per-expression values, no arbitrary
overrides). MoTeC M1 Sim is retained only as a **validation oracle** for the time-domain
math.

## Architecture: one core, three runners

The hard part is built once:

```
            +------------------------ m1-eval core ------------------------+
 scenario ->| value store + state runtime + expr/stmt eval + table lookup  |-> Trace
   / log    +------^-----------------^-------------------------^-----------+   (values
                   |                 |                         |               over time)
             single-function   dependency-cone          whole-project
                runner       (topo upstream order)   multi-rate scheduler
```

**Core components:**
- **Value store** — channel/parameter/local -> typed value, using `m1-typecheck`'s
  `ValueType` system (Boolean, Integer, Unsigned, Float, Enum, String).
- **State runtime** — per-call-site state objects for stateful builtins (filter
  capacitors, integral accumulators, delay/debounce timers, `Change` previous-values,
  `static local`s), advanced each tick by `dt`. Keyed by a stable call-site identity.
- **Expression evaluator** — arithmetic / comparison / logical / bitwise / ternary /
  member-access / enum / function-call.
- **Statement executor** — assignment (incl. compound), `if/else`, `when/is`,
  `expand/to`, `local` / `static local` declarations.
- **Table lookup** — load axis metadata + cell values from `.m1cfg`; linear interpolation
  in 1/2/3-D, with documented clamp/extrapolation behaviour.

**Three runners** differ only in *which* functions execute each tick and in what order
(ordering taken from `m1-typecheck`'s dependency graph):
- **Single-function** — run one chosen function each tick over a supplied time series.
- **Dependency-cone** — run a chosen target channel plus its upstream cone, topologically
  ordered.
- **Whole-project** — multi-rate scheduler: a base tick plus per-function rate divisors
  (500/200/50/10/2 Hz), running all scheduled functions in dependency/rate order.

## Scenario & log input; the counterfactual model

Everything the engine does not *compute* it must be *given*. One unifying concept — the
**Scenario** — layers input sources, later overriding earlier:

1. **Project calibration (`.m1cfg`)** — table cells + parameter defaults. Always loaded;
   the baseline.
2. **Scenario file (TOML/JSON)** — the "script" the user writes to drive inputs: constants
   or piecewise/time-series values for chosen channels, plus run config (mode, target(s),
   duration, base rate, `dt`).
3. **Log import (CSV + `.ld`)** — a recorded run supplies real time series for logged
   channels, resampled onto the tick grid.
4. **Overrides** — pin specific channels to a constant or expression, replacing whatever
   sources 1–3 provided.

**Channel classification** comes from `m1-typecheck`'s graph: each channel is either
*computed* (some scheduled function writes it) or *external* (no in-project writer —
sensors, CAN-in, constants, calibration). Externals must resolve to a source above;
computed channels are evaluated unless pinned.

**Counterfactual replay (headline feature):** load a log as ground truth for all logged
channels, override one or more channels, and the engine recomputes only the **downstream
dependency cone** of each override, leaving everything upstream/unrelated at logged
values. Output is both the new series and a **diff vs the logged series**. This answers
"what if this sensor read 5% higher / this calibration value changed" against a real lap.
`m1-visualiser` paints the diff onto the graph.

**Determinism:** fixed tick grid, explicit `dt`; same inputs produce the same outputs.
**Invariant:** a no-op override must reproduce the logged series within tolerance — a
whole-pipeline sanity check.

## Builtin coverage & fidelity

Tiered coverage, with **fail-loud** behaviour on anything unimplemented (never emit a
silently-wrong number):

- **Tier 1 (v1, pure):** operators, literals, enums, member access, `if/else`, `when/is`,
  `expand/to`, `local` / `static local`, ternary; `Calculate.*`, `Limit.*`, `Convert.*`,
  and table `.Lookup()` interpolation. Deterministic, no time dependence.
- **Tier 2 (v1, stateful — the hard core):** `Filter.FirstOrder`, `Integral.Normal`,
  `Derivative.{Normal,Filtered,Adaptive}`, `Debounce.*`, `Delay.{Rising,Falling,...}`,
  `Change.*`, `Filter.{Maximum,Minimum}`, timers, `static local` persistence. Each is a
  small state object keyed by call-site and advanced by `dt`. Fidelity risk concentrates
  here.
- **Tier 3 (stub / scenario-fed):** `CanComms.*`, `Serial.*`, `System.*`, `Logging.*` —
  return scenario-provided or documented stub values, flagged as "externally driven."
  Real CAN can later be fed from logged CAN channels.

**Fidelity strategy:**
- Encode each stateful operator's update law explicitly, with **documented assumptions**
  (e.g. first-order filter coefficient derived from time constant and `dt`; trapezoidal
  integration with clamping plus reset/preset). Semantics are informed by the M1 manuals
  but **paraphrased, never reproduced** (the manuals are proprietary — see Risks).
- **Golden tests against MoTeC M1 Sim** for the Tier-2 operators and a few representative
  real EV-M1 scripts, using tolerance-based floating-point comparison; divergences
  documented.
- **`m1-eval --coverage <project>`** reports, before running, which builtins/constructs
  each script uses and whether the engine supports them faithfully or stubs them — so the
  user knows what is trustworthy versus externally driven.

## Interfaces & ecosystem integration

Priority order (the visualiser is the primary consumer — "package deal"):
**library API > CLI > LSP.**

- **Library crate `m1-eval`** (the product). Sketch:
  - `Engine::load(project_path, cfg_path) -> Engine`
  - `engine.apply_scenario(scenario)` / `engine.load_log(path)` /
    `engine.override(channel, value_or_expr)`
  - `engine.run(duration) -> Trace`
  - `Trace`: per-channel time series, per-tick value access, and **per-expression value
    introspection** (required by hover/inlay/overlay).
  - Clean data boundary: no `m1-typecheck`/`m1-core` types leak past the public API
    (mirrors `m1-doc`'s discipline). There is only ever **one engine**; the visualiser and
    LSP are thin views over `Trace`.
- **CLI `m1-eval`** (thin shell over the library):
  `m1-eval --project P --scenario S [--function F | --target CH | --whole-project]
  [--log L.csv|L.ld] [--override CH=expr] --out trace.{json,csv}`, plus `--coverage`.
  Exit-code and output-format conventions match `docs/cli.md` (shared CLI conventions).
- **LSP (m1-lsp), later phase:** hover-to-evaluate and inline value hints, both thin views
  over `Trace` at a chosen scrubber time. Reuse the library; no second engine.
- **Ecosystem wiring:** new repo `m1-eval` added to `m1-tools.repos`, the README tool
  table, and the architecture diagram. Depends on `m1-core` + `m1-typecheck` via versioned
  git tags (pin the latest releases; consumer-bump PRs cascade per toolchain convention).
  `m1-visualiser` depends on `m1-eval` (overlay) + `m1-typecheck` (structure).
- **Config:** reads the workspace `m1-tools.toml` (an `[eval]` section as needed) following
  the toolchain's defaults < `m1-tools.toml` < tool file < CLI flags precedence.

## Phase plan

Each phase is independently useful and shippable.

1. **Core + single-function/cone runner + CLI + library API.** Expression/statement eval,
   Tier-1 + Tier-2 builtins, table lookup from `.m1cfg`, scenario input (constants + time
   series). Tier-3 IO stubbed/scenario-fed. *This is what `m1-visualiser` hooks into first.*
2. **Whole-project multi-rate scheduler** — the faithful mini-ECU.
3. **Log-driven counterfactual** — CSV + `.ld` import, logged channels as ground truth,
   channel overrides, downstream re-evaluation, diff vs log. The headline feature.
4. **LSP integration** — hover-to-evaluate + inline value hints via `m1-lsp`, reusing the
   library API.

Validation (golden tests, fail-loud, `--coverage`) runs through all phases.

## Testing strategy

- **Unit tests per builtin**, especially Tier-2 time-domain math, with hand-derived
  expected values.
- **Golden tests vs MoTeC M1 Sim** for representative scripts (M1 Sim as validation
  oracle), tolerance-based.
- **Deterministic snapshot tests** on `Trace` output for the EV-M1 project.
- **No-op-override differential test:** counterfactual replay with an identity override
  must reproduce the logged series within tolerance.
- **`.ld` fixtures** are trimmed/synthetic to avoid committing proprietary or
  calibration-bearing data.

## Risks & mitigations

- **Fidelity drift vs MoTeC (highest).** Mitigate with golden tests against M1 Sim,
  explicitly documented assumptions, fail-loud on unsupported constructs, and `--coverage`
  transparency.
- **`.ld` format / EULA (legal).** See the licensing assessment below. Mitigations: CSV is
  the always-unencumbered path; the `.ld` reader is built clean-room on existing
  license-compatible implementations and lives behind a cargo feature flag; we never
  reverse-engineer MoTeC *software* and never redistribute MoTeC data; the user should
  confirm their specific MoTeC software-licence terms.
- **M1 manuals are proprietary.** Use them to inform semantics; **paraphrase, never copy**
  their text into spec, code, or comments.
- **Scope creep (whole-project sim is large).** Strict phasing; each phase shippable.
- **Calibration availability.** Table cells come from `.m1cfg`; if absent, lookups
  fail-loud rather than guess.
- **Scheduling fidelity (Phase 2).** The rate-divisor model may not match MoTeC's exact
  intra-tick ordering; validate against M1 Sim and document the ordering rules.

## Licensing assessment (`.ld` import)

The toolchain is **GPL-3.0-or-later**. Relevant findings:

- **M1 manuals are proprietary documentation** (`MoTeC (c) 2014`, "no part may be
  reproduced without prior express written permission"). This restricts copying the
  *manual text*, not the data format or the language's behaviour. Semantics are
  paraphrased from understanding, never pasted.
- **The `.ld` format is already independently reverse-engineered and published** under
  license-compatible terms:
  - [`motec-i2`](https://github.com/afonso360/motec-i2) — Rust, **MIT**, parses and writes
    `.ld`/`.ldx`. MIT is GPL-compatible, so it can be depended on or vendored.
  - [`ldparser`](https://github.com/gotzl/ldparser) — Python, GPL-3.0, explicitly
    "decoding based solely on reverse engineering the binary data" (clean-room provenance).
- The sample `.ld` files confirm the documented layout (offset table + metadata strings
  such as `project.name`, `Summary.Firmware`, `device.boardtype`).

**Assessment — low risk, with guardrails.** Building the `.ld` reader on these third-party,
clean-room, license-compatible implementations means we parse an independently-documented
*file format* operating on the user's *own* telemetry, and never reverse-engineer MoTeC's
*software* — sidestepping the usual EULA "no reverse engineering of our software" clause.
Guardrails: CSV always available and unencumbered; `.ld` behind a feature flag with
documented clean-room provenance; no redistribution of MoTeC calibration/firmware/manual
content or sample logs.

**Caveat:** the actual signed MoTeC software EULA was not available on disk (it resides
inside a Parallels Windows VM, not on the macOS filesystem), so this is "low risk +
verify," not a certified clearance. The user should confirm their specific MoTeC
software-licence terms before shipping the `.ld` reader.

## Open questions (resolve during planning)

- Exact scenario-file schema (TOML vs JSON; time-series encoding — keyframes vs CSV rows).
- Call-site identity scheme for keying stateful operator state across ticks.
- Whether to vendor `motec-i2` or depend on it as a crate (and its current maintenance
  state).
- `.m1cfg` calibration access path: reuse `m1-typecheck`'s `with_config` loading vs a
  dedicated reader.
- Precise `Trace` representation for memory at long durations / high rates (whole-project
  sim over a full lap could be large).
