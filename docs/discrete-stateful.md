<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
# Discrete stateful evidence boundary

Issue #45 asks for captured M1 behavior for debounce modes, change predicates,
calculation-state predicates, and buffered signal delays. The repository does
not contain those transition captures. The pinned intrinsic catalogue records
method names, parameter families, and short descriptions, but it is not a
tick-by-tick execution contract.

## Current runtime contract

The existing `Debounce.Stable`, `Debounce.Filter`, boolean and value `Delay`
methods, `Change.*`, and stateful `Calculate.*` methods remain Assumed. Their
synthetic tests describe the evaluator's current model. They do not establish
M1 compatibility.

`Debounce.Fast` and `Debounce.Verify` are unsupported. The runtime no longer
routes either method through the `Debounce.Stable` state machine. That alias made
three catalogue entries behave identically even though issue #45 requires
distinct transition behavior.

The complete buffered family is recognized but unsupported:

- `Delay.Signal15`
- `Delay.Signal31`
- `Delay.Signal63`
- `Delay.Signal127`
- `Delay.Signal255`
- `Delay.Signal511`
- `Delay.Signal1023`

For these nine evidence-gated methods, dispatch validates the captured arity and
M1 value families. It then returns `UnsupportedBuiltinBehavior` with the missing
contract named. `--coverage` puts every method in the Unsupported bucket. A
`Library.`-qualified spelling follows the same route and remains visible in the
error.

The catalogue declares `Delay.Signal15` with a FloatingPoint signal and Integer
delay. It declares the other six signal delays with FloatingPoint arguments for
both parameters. Argument validation preserves that difference. It does not
interpret the delay, allocate a queue, or invent a startup value.

## Captures needed for implementation

A debounce capture needs separate output traces for Stable, Fast, and Verify
under the same input timeline. Include initial false and initial true, pulses
shorter than the filter, changes exactly on the filter boundary, reversals while
timing, and zero or negative filters. Record the calculation rate and output at
tick zero.

A buffered-delay capture needs delay values of zero, one, the method's maximum,
and one value outside the documented range. Include the first samples after
startup, a changing signal, and a delay that changes while the buffer contains
data. The FloatingPoint delay variants also need fractional, negative, NaN, and
infinite inputs. These traces must settle whether M1 rounds or clamps the delay,
whether zero delay returns the current sample, and what fills unread history.

Change and calculation-state captures need first-tick behavior, exact-threshold
transitions, filtered transitions that revert before acceptance, and the tick on
which a sustained change emits. Numeric captures should cover Integer,
UnsignedInteger, FloatingPoint, and FixedPoint7dps inputs where the catalogue
allows them.

Commit the traces as conformance fixtures with their project hashes,
calculation rate, initial state, and source provenance. Once those fixtures
exist, the implementation can use one state slot per resolved call site and can
move only the methods proven by the captures out of Unsupported or Assumed.
