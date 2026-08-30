<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
# Table conformance

Table behavior remains **Assumed** under the repository maturity contract. The
committed vectors are synthetic and the real-project test checks only
determinism. Neither is M1 Sim output.

## Runtime contract

M1 tables have one, two, or three axes. The calibration reader treats
`.m1cfg` `Site="x,y,z"` coordinates as authoritative. It sorts cells by those
coordinates before evaluation, so XML element order cannot change a result.
The flat body offset is:

```text
ix + nx * (iy + ny * iz)
```

X changes fastest, followed by Y and Z. A hand-written calibration with no
`Site` attributes uses that same X-fastest sequence. Mixing addressed and
unaddressed cells is an error.

Numeric axes retain the cell's declared M1 scalar family. Breakpoints must be
finite, use one family per axis, and increase strictly. A single-site axis is a
constant axis and accepts every finite numeric input. Repeated or descending
breakpoints return a `MissingCalibration` diagnostic from `.Lookup()`. The
evaluator does not choose one of two repeated sites; `.Get()` remains a raw
flat-body read and does not inspect breakpoints.

Enum axis cells store exact declared enum values. At a call site, the evaluator
resolves the runtime member through the loaded project's enum definition and
selects the matching site. The axis `Source` attribute binds the axis to that
enum type; a member from another enum is an error even when both members have
the same declared integer value. It does not interpolate enum sites. A missing
source, missing value, or numeric value supplied to an enum axis is an error.
Every calibrated value on a closed project enum must be one of that enum's
declared member values. Firmware enums whose membership is explicitly open are
exempt because their complete member set is unavailable offline.

Body length must equal the product of the enabled axis lengths. Every body cell
must use the same finite M1 scalar family. Exact breakpoint and clamped lookups
return the stored body cell without a floating-point round trip. Interpolated
floating-point results use binary32 arithmetic. Signed, unsigned, and
fixed-point bodies retain their family only when the interpolated result is
exactly representable in that storage.

An axis clamps at both ends unless its table's `<Axis>` entry in `Project.m1prj`
sets `Extrapolate` to `Below`, `Above`, or `Both`. The loader accepts both the
direct `Component/Axis` shape used by the pinned project model and the
`Component/Props/Axis` shape found in M1 project exports. Extrapolation uses the
first two or last two sites at the enabled end. A single-site axis still returns
its only body plane because it has no interval to extend. Boundary metadata
follows the project's resolved `SymbolKind::Table` classification, including
`BuiltIn.Table*` and `BuiltIn.CalibrationTable` components.

## Synthetic vectors

`tests/fixtures/conformance/synthetic-tables.toml` runs through the strict
conformance API from issue #42. Its project and calibration hashes are part of
the fixture.

The fixture uses affine tables whose expected values can be checked by hand:

| Table | Shape | Coverage |
| --- | --- | --- |
| Curve | 3 | Every breakpoint, a quarter-interval value, and both clamped ends |
| Grid | 3 by 3 | X-fastest corner identity, quarter-interval bilinear interpolation, and isolated X/Y boundaries |
| Cube | 2 by 3 by 2 | X/Y/Z layout, quarter-interval trilinear interpolation, and isolated X/Y/Z boundaries |
| Single | 1 | Degenerate single-site behavior below, at, and above the site |
| Extend | 2 by 2 | Bilinear interpolation and project-enabled extrapolation at both ends |
| Enum Map | 3 enum sites | Exact selection for non-consecutive declared enum values |

All 84 expected outputs use typed binary32 wire values with zero tolerance.
That catches a changed body stride or boundary rule immediately. It does not
claim that M1 Sim uses the same arithmetic order.

## Private M1 Sim capture gate

A genuine table capture must include all of these cases:

1. Every site on a one-dimensional table and every corner on two- and
   three-dimensional tables with unequal axis lengths.
2. At least one non-midpoint interior coordinate per numeric axis. Non-midpoint
   values can reveal operation-order rounding that midpoint-only data misses.
3. A coordinate below and above every numeric axis while the other axes remain
   inside their ranges.
4. Separate tables for the default clamped policy and every enabled
   extrapolation end used by the project.
5. A single-site numeric axis and an enum axis with non-consecutive declared
   values.
6. The exact exported `.m1cfg` and `Project.m1prj`, including `Site` and
   `Extrapolate` attributes. Hash every consumed file through the conformance
   fixture manifest.

Capture inputs before each calculation tick and outputs after it, starting from
a reset M1 Sim session. Record the M1 Build and M1 Sim versions and capture time
in `provenance`. Do not calculate the expected column with m1-eval. The general
procedure in [`conformance.md`](conformance.md#capturing-from-m1-sim) still
applies.

Keep licensed or team-private material outside this repository. Point the
table-specific gate at one or more approved fixtures using an operating-system
path list:

```sh
M1_EVAL_TABLE_M1_SIM_FIXTURES=/secure/captures/tables.toml \
  cargo test --test table_conformance \
  configured_private_table_captures_form_an_optional_gate
```

The test requires at least one passing fixture with `m1-sim` provenance. A
synthetic fixture cannot satisfy it.

## Real-project determinism gate

The optional project test loads a private project and its matching calibration.
It evaluates the first site of every valid table twice and compares the exact
typed result. Tables with repeated or descending axes must return the same
diagnostic on both validations.

```sh
M1_EVAL_TABLE_PROJECT=/private/project/Project.m1prj \
M1_EVAL_TABLE_CONFIG=/private/project/parameters.m1cfg \
  cargo test --test table_conformance configured_real_project_tables_are_deterministic
```

This gate reads private files but never copies their names, values, or bytes
into the repository. It is a regression test for loading and determinism, not
evidence of numerical agreement with M1.
