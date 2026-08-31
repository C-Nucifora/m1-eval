<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
# MPSE, TC, and Switch capability contract

The vendored `m1-typecheck` catalogue target `m1-build-2026-06` records seven
methods from M1 Build help-pane captures. Those captures establish names,
argument families, return families, and short descriptions. They do not contain
the formulas or state rules needed for an internal evaluator implementation.
The catalogue's generated `stateful` marker is not behavioral evidence and is
not used to infer any of those missing rules.

All seven methods are therefore adapter-backed. Coverage reports them under
`Adapter-backed`. At runtime, m1-eval validates the captured arity, argument
families, and documented ranges, then offers a normalized `HardwareCall` to an
attached `HardwareAdapter`. The name is historical. This route also carries
capture-only library behavior and does not imply that MPSE is physical
hardware.

The route does not read scenario `[[io]]` values and has no generic typed
fallback. This matters for `Switch.Set`: only the adapter can own its side
effect. If no adapter handles a call, evaluation returns
`UnsupportedBuiltinBehavior` with the missing contract named below.

| Method | Captured signature | Classification | Behavior boundary | Missing internal contract |
| --- | --- | --- | --- | --- |
| `MPSE.PressureRatioFactor` | `(FloatingPoint pr) -> FloatingPoint`; `pr` is documented as 0 to 1 | Adapter-backed, required | Its input is explicit and deterministic, but the computation is unknown | Pressure-ratio equation, constants, choked-flow boundary, and M1 rounding |
| `MPSE.Solve` | `(UnsignedInteger n, FloatingPoint dt, pup, tup, map, mat, taf, mvol, mafp, mafe, mafs, k) -> FloatingPoint` | Adapter-backed, required | Its inputs and time step are explicit, but the update law is unknown | Integration equation, step ordering, unit convention, and Kalman update |
| `MPSE.ThrottleMassFlow` | `(FloatingPoint map, pup, tup, taf) -> FloatingPoint` | Adapter-backed, required | Its inputs are explicit and deterministic, but the computation is unknown | Units, gas constants, throttle-area-factor definition, reverse-flow behavior, and choked-flow boundary |
| `TC.CO` | `() -> FloatingPoint` | Adapter-backed, required | The zero-argument call depends on hidden package or physical state | Implicit package state, calibration, and physical inputs used for crossover mass flow |
| `TC.TP` | `(FloatingPoint map, flow) -> FloatingPoint` | Adapter-backed, required | Its direct inputs are explicit, but its calibration and model are hidden | Inverse throttle model, calibration inputs, valid domain, and M1 rounding |
| `Switch.Get` | `(Integer idx) -> Integer`; `idx` is documented as 0 to 63 | Adapter-backed, required | The index is explicit, but the persistent bank and external state are hidden | Bank initial values, scope, lifetime, ordering, and external side effects |
| `Switch.Set` | `(Integer idx, Integer val) -> Void`; `idx` is documented as 0 to 63 | Adapter-backed, required | The arguments are explicit, but the persistent bank and side effect are hidden | Bank initial values, scope, lifetime, ordering, and external side effects |

## Adapter boundary

The evaluator converts accepted arguments to the captured M1 family before it
calls the adapter:

- `Integer` becomes signed 32-bit and `UnsignedInteger` becomes unsigned
  32-bit. Cross-signed conversions preserve the 32-bit pattern.
- `FloatingPoint` becomes binary32. The capture-backed conversion evidence
  permits signed and unsigned integer inputs to widen to `FloatingPoint`. It
  does not establish a Fixed Point 7dps conversion for these calls, so the
  evaluator rejects that family.
- `Switch.Get` restores the adapter reply to M1 `Integer`.
- MPSE and TC restore replies to M1 binary32 `FloatingPoint`.
- A handled `Switch.Set` always returns the evaluator's `Bool(true)` Void unit.
  An arbitrary adapter payload never enters script execution.

The adapter receives the canonical receiver, source spelling, exact call site,
normalized arguments, and evaluator time. Successful calls record adapter
provenance in the trace and mark the source-spelled call externally driven.

## Evidence needed for direct models

This catalogue is signature evidence, not behavioral verification. No method in
this group meets the README's `Verified` definition.

An internal MPSE or TC implementation needs either a published authoritative
formula with all unit and boundary conventions or committed M1 output vectors.
Vectors should cover domain boundaries, signed inputs where allowed, binary32
rounding, and repeated `MPSE.Solve` steps.

An internal Switch bank needs captured evidence for its initial contents,
whether state is ECU-global or scoped, visibility between call sites and
functions, same-tick ordering, reset lifetime, and any external side effect.
Until that evidence exists, m1-eval will not invent a bank or treat `Set` as a
no-op.
