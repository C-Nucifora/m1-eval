// SPDX-License-Identifier: GPL-3.0-or-later
//! One-, two-, and three-dimensional table evaluation over a [`CalTable`].
//!
//! Numeric axes interpolate linearly. Enum axes select an exact member site.
//! Inputs outside a numeric axis clamp unless the project descriptor enables
//! extrapolation below, above, or at both ends.
//!
//! ## Body memory layout
//!
//! M1 `.m1cfg` cells carry `Site="x,y,z"` coordinates. X changes fastest, then
//! Y, then Z. The flat offset is `ix + nx * (iy + ny * iz)`. The calibration
//! reader sorts explicitly addressed cells into this order, so XML document
//! order cannot change a lookup result.

use crate::calib::{AxisExtrapolation, CalAxis, CalAxisValues, CalTable};
use crate::error::EvalError;
use crate::value::{FixedPoint7dps, M1Scalar, M1ScalarKind, Value};
use m1_typecheck::Project;
use std::collections::BTreeSet;

/// One resolved table coordinate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TableInput {
    /// M1-width numeric coordinate.
    Numeric(M1Scalar),
    /// Project enum identity and declared integer value of one member.
    Enum {
        /// Enum type id in the loaded project.
        enum_id: usize,
        /// Declared integer value of the member.
        value: i64,
    },
}

/// Evaluate a table from M1-width numeric coordinates.
///
/// Use [`lookup_values`] for runtime values that may include enum members.
pub fn lookup(t: &CalTable, inputs: &[M1Scalar]) -> Result<M1Scalar, EvalError> {
    let inputs: Vec<TableInput> = inputs.iter().copied().map(TableInput::Numeric).collect();
    lookup_inputs(t, &inputs)
}

/// Resolve runtime enum members through `project`, then evaluate the table.
pub fn lookup_values(
    table: &CalTable,
    inputs: &[Value],
    project: &Project,
) -> Result<M1Scalar, EvalError> {
    let inputs = inputs
        .iter()
        .map(|input| match input {
            Value::M1(value) => Ok(TableInput::Numeric(*value)),
            Value::Enum { id, .. } => input.as_enum_int(project).map(|value| TableInput::Enum {
                enum_id: *id,
                value,
            }),
            other => Err(EvalError::TypeError {
                detail: format!("{other:?} is not a numeric or enum table coordinate"),
            }),
        })
        .collect::<Result<Vec<_>, _>>()?;
    lookup_inputs(table, &inputs)
}

/// Evaluate a table from already-resolved numeric or enum coordinates.
///
/// Arity must match the axis count. Numeric inputs clamp or extrapolate from
/// their axis metadata. Enum inputs must equal one calibrated declared value.
/// Empty, repeated, descending, mixed-type, or misshapen tables fail loud.
pub fn lookup_inputs(t: &CalTable, inputs: &[TableInput]) -> Result<M1Scalar, EvalError> {
    if inputs.len() != t.axes.len() {
        return Err(EvalError::BadCall {
            detail: format!(
                "table lookup arity mismatch: {} input(s) for a {}-axis table",
                inputs.len(),
                t.axes.len()
            ),
        });
    }
    validate(t)?;
    let body_kind = t.body[0].kind();

    // Per-axis: lower bracket index and relative position. Clamped coordinates
    // stay in [0, 1]; enabled extrapolation can produce values outside it.
    let n = t.axes.len();
    let mut lo = vec![0usize; n];
    let mut frac = vec![0.0f64; n];
    for (k, axis) in t.axes.iter().enumerate() {
        let (i, f) = bracket(axis, &inputs[k], k)?;
        lo[k] = i;
        frac[k] = f;
    }

    // M1 site strides with axis 0 (X) innermost.
    let mut stride = vec![1usize; n];
    for k in 1..n {
        stride[k] = stride[k - 1] * t.axes[k - 1].len();
    }

    // Preserve the stored scalar bit-for-bit at exact breakpoints and clamps.
    // Fractions remain f64 until this decision, so a large integral coordinate
    // just below an endpoint cannot round to an endpoint through binary32.
    if frac.iter().all(|value| *value == 0.0 || *value == 1.0) {
        let offset: usize = (0..n)
            .map(|k| {
                let index = if frac[k] == 1.0 {
                    (lo[k] + 1).min(t.axes[k].len() - 1)
                } else {
                    lo[k]
                };
                index * stride[k]
            })
            .sum();
        return Ok(t.body[offset]);
    }

    // Blend over the 2^n corners of the bracketing hypercube. Each corner
    // chooses the lower or upper breakpoint per axis; its weight is the product
    // of (1 - frac) for "lower" axes and frac for "upper" axes.
    let mut float_acc = 0.0_f32;
    let mut exact_acc = 0.0_f64;
    for corner in 0..(1usize << n) {
        let mut float_weight = 1.0_f32;
        let mut exact_weight = 1.0_f64;
        let mut offset = 0usize;
        for k in 0..n {
            let upper = (corner >> k) & 1 == 1;
            let len = t.axes[k].len();
            // Clamp the upper index so a single-breakpoint axis (len 1) or a
            // top-end clamp can't index past the last cell.
            let idx = if upper {
                (lo[k] + 1).min(len - 1)
            } else {
                lo[k]
            };
            let factor = if upper { frac[k] } else { 1.0 - frac[k] };
            float_weight *= factor as f32;
            exact_weight *= factor;
            offset += idx * stride[k];
        }
        if exact_weight != 0.0 {
            match t.body[offset] {
                M1Scalar::FloatingPoint(value) => float_acc += float_weight * value,
                M1Scalar::Integer(value) => {
                    exact_acc += exact_weight * f64::from(value);
                }
                M1Scalar::UnsignedInteger(value) => {
                    exact_acc += exact_weight * f64::from(value);
                }
                M1Scalar::FixedPoint7dps(value) => {
                    exact_acc += exact_weight * f64::from(value.raw());
                }
            }
        }
    }
    match body_kind {
        M1ScalarKind::FloatingPoint => Ok(M1Scalar::FloatingPoint(float_acc)),
        _ => narrow_interpolated(body_kind, exact_acc),
    }
}

/// Restore an interpolated binary32 value to the table body's declared M1
/// storage family. Integral tables reject fractional results rather than
/// guessing a rounding rule. Fixed-point table bodies interpolate their raw
/// scaled integers and likewise require an exact, in-range raw result.
fn narrow_interpolated(kind: M1ScalarKind, value: f64) -> Result<M1Scalar, EvalError> {
    let incompatible = || EvalError::TypeError {
        detail: format!("table interpolation result {value} is not representable as M1 {kind:?}"),
    };
    match kind {
        M1ScalarKind::FloatingPoint => Ok(M1Scalar::FloatingPoint(value as f32)),
        M1ScalarKind::Integer
            if value.is_finite()
                && value.fract() == 0.0
                && value >= f64::from(i32::MIN)
                && value <= f64::from(i32::MAX) =>
        {
            Ok(M1Scalar::Integer(value as i32))
        }
        M1ScalarKind::UnsignedInteger
            if value.is_finite()
                && value.fract() == 0.0
                && value >= 0.0
                && value <= f64::from(u32::MAX) =>
        {
            Ok(M1Scalar::UnsignedInteger(value as u32))
        }
        M1ScalarKind::FixedPoint7dps
            if value.is_finite()
                && value.fract() == 0.0
                && value >= f64::from(i32::MIN)
                && value <= f64::from(i32::MAX) =>
        {
            Ok(M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(
                value as i32,
            )))
        }
        _ => Err(incompatible()),
    }
}

/// Validate an in-memory table before interpolation.
pub fn validate(table: &CalTable) -> Result<(), EvalError> {
    validate_named(table, "table")
}

pub(crate) fn validate_named(table: &CalTable, label: &str) -> Result<(), EvalError> {
    validate_shape_named(table, label)?;
    let invalid = |detail: String| EvalError::MissingCalibration {
        path: format!("{label}: {detail}"),
    };
    for (axis_index, axis) in table.axes.iter().enumerate() {
        let axis_name = axis_name(axis_index);
        match &axis.values {
            CalAxisValues::Numeric(values) => {
                for (index, pair) in values.windows(2).enumerate() {
                    let lower = pair[0].as_f64();
                    let upper = pair[1].as_f64();
                    if upper == lower {
                        return Err(invalid(format!(
                            "axis {axis_name} repeats a breakpoint at sites {index} and {}",
                            index + 1
                        )));
                    }
                    if upper < lower {
                        return Err(invalid(format!(
                            "axis {axis_name} is not strictly ascending at sites {index} and {}",
                            index + 1
                        )));
                    }
                }
            }
            CalAxisValues::Enum { values, enum_id } => {
                if enum_id.is_none() {
                    return Err(invalid(format!(
                        "enum axis {axis_name} has no resolved project enum type"
                    )));
                }
                let mut seen = BTreeSet::new();
                for value in values {
                    if !seen.insert(value) {
                        return Err(invalid(format!(
                            "enum axis {axis_name} repeats value {value}"
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_shape_named(table: &CalTable, label: &str) -> Result<(), EvalError> {
    let invalid = |detail: String| EvalError::MissingCalibration {
        path: format!("{label}: {detail}"),
    };
    if !(1..=3).contains(&table.axes.len()) {
        return Err(invalid(format!(
            "has {} axes; M1 tables require one, two, or three",
            table.axes.len()
        )));
    }
    let mut expected = 1usize;
    for (axis_index, axis) in table.axes.iter().enumerate() {
        let axis_name = axis_name(axis_index);
        if axis.is_empty() {
            return Err(invalid(format!("axis {axis_name} has no calibrated sites")));
        }
        expected = expected.checked_mul(axis.len()).ok_or_else(|| {
            invalid(format!(
                "axis shape overflows while adding axis {axis_name}"
            ))
        })?;
        match &axis.values {
            CalAxisValues::Numeric(values) => {
                let kind = values[0].kind();
                for (index, value) in values.iter().copied().enumerate() {
                    if value.kind() != kind {
                        return Err(invalid(format!(
                            "axis {axis_name} mixes M1 scalar families at site {index}"
                        )));
                    }
                    if !value.as_f64().is_finite() {
                        return Err(invalid(format!(
                            "axis {axis_name} site {index} is not finite"
                        )));
                    }
                }
            }
            CalAxisValues::Enum { .. } => {
                if axis.extrapolation != AxisExtrapolation::Clamp {
                    return Err(invalid(format!("enum axis {axis_name} cannot extrapolate")));
                }
            }
        }
    }
    if table.body.len() != expected {
        return Err(invalid(format!(
            "body has {} cells, expected {expected} for the axis shape",
            table.body.len()
        )));
    }
    let body_kind = table.body[0].kind();
    for (index, value) in table.body.iter().copied().enumerate() {
        if value.kind() != body_kind {
            return Err(invalid(format!(
                "body mixes M1 scalar families at offset {index}"
            )));
        }
        if !value.as_f64().is_finite() {
            return Err(invalid(format!("body offset {index} is not finite")));
        }
    }
    Ok(())
}

fn axis_name(index: usize) -> &'static str {
    ["X", "Y", "Z"].get(index).copied().unwrap_or("?")
}

fn bracket(
    axis: &CalAxis,
    input: &TableInput,
    axis_index: usize,
) -> Result<(usize, f64), EvalError> {
    match (&axis.values, input) {
        (CalAxisValues::Numeric(values), TableInput::Numeric(value)) => {
            let x = value.as_f64();
            if !x.is_finite() {
                return Err(EvalError::BadCall {
                    detail: format!(
                        "table axis {} coordinate is not finite",
                        axis_name(axis_index)
                    ),
                });
            }
            Ok(bracket_numeric(values, x, axis.extrapolation))
        }
        (CalAxisValues::Numeric(_), other) => Err(EvalError::TypeError {
            detail: format!(
                "table axis {} is numeric, got {other:?}",
                axis_name(axis_index)
            ),
        }),
        (
            CalAxisValues::Enum {
                values,
                enum_id: Some(expected_enum_id),
            },
            TableInput::Enum { enum_id, value },
        ) if enum_id == expected_enum_id => {
            let index = values
                .iter()
                .position(|candidate| candidate == value)
                .ok_or_else(|| EvalError::BadCall {
                    detail: format!(
                        "table enum axis {} has no calibrated value {value}",
                        axis_name(axis_index)
                    ),
                })?;
            if index + 1 == values.len() && index != 0 {
                Ok((index - 1, 1.0))
            } else {
                Ok((index, 0.0))
            }
        }
        (
            CalAxisValues::Enum {
                enum_id: Some(expected_enum_id),
                ..
            },
            TableInput::Enum { enum_id, .. },
        ) => Err(EvalError::TypeError {
            detail: format!(
                "table enum axis {} expects enum type {expected_enum_id}, got enum type {enum_id}",
                axis_name(axis_index)
            ),
        }),
        (CalAxisValues::Enum { .. }, other) => Err(EvalError::TypeError {
            detail: format!(
                "table axis {} is enumerated, got {other:?}",
                axis_name(axis_index)
            ),
        }),
    }
}

/// For a validated ascending numeric axis, return the lower site and the
/// relative position between that site and the next.
fn bracket_numeric(axis: &[M1Scalar], x: f64, extrapolation: AxisExtrapolation) -> (usize, f64) {
    let last = axis.len() - 1;
    if last == 0 {
        return (0, 0.0);
    }
    let first = axis[0].as_f64();
    if x <= first {
        if x < first && extrapolation.below() {
            let next = axis[1].as_f64();
            return (0, (x - first) / (next - first));
        }
        return (0, 0.0);
    }
    let end = axis[last].as_f64();
    if x >= end {
        if x > end && extrapolation.above() {
            let previous = axis[last - 1].as_f64();
            return (last - 1, (x - previous) / (end - previous));
        }
        return (last - 1, 1.0);
    }
    // Linear scan: axes are short (calibration tables are small), so a binary
    // search would not pay for itself and would complicate non-monotonic guards.
    for i in 0..last {
        let a = axis[i].as_f64();
        let b = axis[i + 1].as_f64();
        if x >= a && x <= b {
            return (i, (x - a) / (b - a));
        }
    }
    // Unreachable for monotonic axes given the range checks above; clamp high.
    (last - 1, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calib::CalTable;

    fn f(value: f32) -> M1Scalar {
        M1Scalar::FloatingPoint(value)
    }

    fn fs(values: &[f32]) -> Vec<M1Scalar> {
        values.iter().copied().map(f).collect()
    }

    fn v(value: f32) -> M1Scalar {
        f(value)
    }

    fn vs(values: &[f32]) -> Vec<M1Scalar> {
        values.iter().copied().map(v).collect()
    }

    fn t() -> CalTable {
        CalTable::numeric(
            vec![fs(&[0.0, 100.0]), fs(&[0.0, 1.0])],
            fs(&[10.0, 30.0, 20.0, 40.0]),
        )
    }

    #[test]
    fn corners_and_midpoint() {
        assert_eq!(lookup(&t(), &vs(&[0.0, 0.0])).unwrap(), f(10.0));
        assert_eq!(lookup(&t(), &vs(&[100.0, 1.0])).unwrap(), f(40.0));
        assert_eq!(lookup(&t(), &vs(&[0.0, 1.0])).unwrap(), f(20.0));
        assert_eq!(lookup(&t(), &vs(&[100.0, 0.0])).unwrap(), f(30.0));
        assert_eq!(lookup(&t(), &vs(&[50.0, 0.0])).unwrap(), f(20.0));
        assert_eq!(lookup(&t(), &vs(&[0.0, 0.5])).unwrap(), f(15.0));
        assert_eq!(lookup(&t(), &vs(&[50.0, 0.5])).unwrap(), f(25.0));
    }

    #[test]
    fn clamps_out_of_range() {
        assert_eq!(lookup(&t(), &vs(&[-5.0, 0.0])).unwrap(), f(10.0));
        assert_eq!(lookup(&t(), &vs(&[999.0, 2.0])).unwrap(), f(40.0));
        assert_eq!(lookup(&t(), &vs(&[-5.0, 0.5])).unwrap(), f(15.0));
        assert_eq!(lookup(&t(), &vs(&[999.0, 0.5])).unwrap(), f(35.0));
    }

    #[test]
    fn nan_coordinate_fails_loud_instead_of_clamping_high() {
        let table = CalTable::numeric(vec![fs(&[0.0, 1.0])], fs(&[10.0, 20.0]));
        assert!(matches!(
            lookup(&table, &[v(f32::NAN)]),
            Err(EvalError::BadCall { .. })
        ));
    }

    #[test]
    fn arity_mismatch_is_error() {
        assert!(matches!(
            lookup(&t(), &vs(&[1.0])),
            Err(EvalError::BadCall { .. })
        ));
        assert!(matches!(
            lookup(&t(), &vs(&[1.0, 2.0, 3.0])),
            Err(EvalError::BadCall { .. })
        ));
    }

    #[test]
    fn one_dimensional_interpolation() {
        let c = CalTable::numeric(vec![fs(&[0.0, 10.0, 20.0])], fs(&[0.0, 100.0, 50.0]));
        for (input, expected) in [
            (0.0, 0.0),
            (10.0, 100.0),
            (5.0, 50.0),
            (15.0, 75.0),
            (-1.0, 0.0),
            (99.0, 50.0),
        ] {
            assert_eq!(lookup(&c, &[v(input)]).unwrap(), f(expected));
        }
    }

    #[test]
    fn three_dimensional_corners() {
        let c = CalTable::numeric(
            vec![fs(&[0.0, 1.0]), fs(&[0.0, 1.0]), fs(&[0.0, 1.0])],
            (0..8).map(|value| f(value as f32)).collect(),
        );
        assert_eq!(lookup(&c, &vs(&[0.0, 0.0, 0.0])).unwrap(), f(0.0));
        assert_eq!(lookup(&c, &vs(&[0.0, 0.0, 1.0])).unwrap(), f(4.0));
        assert_eq!(lookup(&c, &vs(&[0.0, 1.0, 0.0])).unwrap(), f(2.0));
        assert_eq!(lookup(&c, &vs(&[1.0, 0.0, 0.0])).unwrap(), f(1.0));
        assert_eq!(lookup(&c, &vs(&[1.0, 1.0, 1.0])).unwrap(), f(7.0));
        assert_eq!(lookup(&c, &vs(&[0.5, 0.5, 0.5])).unwrap(), f(3.5));
    }

    #[test]
    fn single_breakpoint_axis() {
        let c = CalTable::numeric(vec![fs(&[5.0]), fs(&[0.0, 1.0])], fs(&[10.0, 20.0]));
        assert_eq!(lookup(&c, &vs(&[5.0, 0.0])).unwrap(), f(10.0));
        assert_eq!(lookup(&c, &vs(&[999.0, 1.0])).unwrap(), f(20.0));
        assert_eq!(lookup(&c, &vs(&[0.0, 0.5])).unwrap(), f(15.0));
    }

    #[test]
    fn project_boundary_policy_controls_extrapolation_per_end() {
        let mut both = CalTable::numeric(vec![fs(&[0.0, 10.0])], fs(&[1.0, 21.0]));
        both.axes[0].extrapolation = AxisExtrapolation::Both;
        assert_eq!(lookup(&both, &[v(-5.0)]).unwrap(), f(-9.0));
        assert_eq!(lookup(&both, &[v(15.0)]).unwrap(), f(31.0));

        both.axes[0].extrapolation = AxisExtrapolation::Below;
        assert_eq!(lookup(&both, &[v(-5.0)]).unwrap(), f(-9.0));
        assert_eq!(lookup(&both, &[v(15.0)]).unwrap(), f(21.0));

        both.axes[0].extrapolation = AxisExtrapolation::Above;
        assert_eq!(lookup(&both, &[v(-5.0)]).unwrap(), f(1.0));
        assert_eq!(lookup(&both, &[v(15.0)]).unwrap(), f(31.0));
    }

    #[test]
    fn enum_axes_select_exact_sites_without_interpolation() {
        let table = CalTable {
            axes: vec![CalAxis::enumerated_for(4, vec![0, 2, 7])],
            body: fs(&[10.0, 20.0, 30.0]),
        };
        for (value, expected) in [(0, 10.0), (2, 20.0), (7, 30.0)] {
            assert_eq!(
                lookup_inputs(&table, &[TableInput::Enum { enum_id: 4, value }]).unwrap(),
                f(expected)
            );
        }
        assert!(matches!(
            lookup_inputs(
                &table,
                &[TableInput::Enum {
                    enum_id: 4,
                    value: 99,
                }]
            ),
            Err(EvalError::BadCall { .. })
        ));
        assert!(matches!(
            lookup_inputs(
                &table,
                &[TableInput::Enum {
                    enum_id: 5,
                    value: 0,
                }]
            ),
            Err(EvalError::TypeError { .. })
        ));
        assert!(matches!(
            lookup_inputs(&table, &[TableInput::Numeric(v(1.0))]),
            Err(EvalError::TypeError { .. })
        ));
    }

    #[test]
    fn invalid_axes_have_precise_diagnostics() {
        let repeated = CalTable::numeric(vec![fs(&[0.0, 0.0])], fs(&[1.0, 2.0]));
        let descending = CalTable::numeric(vec![fs(&[1.0, 0.0])], fs(&[1.0, 2.0]));
        let empty = CalTable::numeric(vec![Vec::new()], Vec::new());
        let duplicate_enum = CalTable {
            axes: vec![CalAxis::enumerated_for(0, vec![0, 0])],
            body: fs(&[1.0, 2.0]),
        };
        let mixed_family =
            CalTable::numeric(vec![vec![f(0.0), M1Scalar::Integer(1)]], fs(&[1.0, 2.0]));
        let non_finite = CalTable::numeric(vec![fs(&[0.0, f32::INFINITY])], fs(&[1.0, 2.0]));
        let mixed_body =
            CalTable::numeric(vec![fs(&[0.0, 1.0])], vec![f(1.0), M1Scalar::Integer(2)]);
        let non_finite_body =
            CalTable::numeric(vec![fs(&[0.0, 1.0])], fs(&[1.0, f32::NEG_INFINITY]));
        for (table, needle) in [
            (repeated, "repeats a breakpoint"),
            (descending, "not strictly ascending"),
            (empty, "has no calibrated sites"),
            (duplicate_enum, "repeats value"),
            (mixed_family, "mixes M1 scalar families"),
            (non_finite, "is not finite"),
            (mixed_body, "body mixes M1 scalar families"),
            (non_finite_body, "body offset 1 is not finite"),
        ] {
            let error = validate(&table).expect_err("invalid axis must fail");
            assert!(format!("{error}").contains(needle), "{error}");
        }
    }

    #[test]
    fn zero_and_four_axis_tables_are_not_m1_tables() {
        let zero = CalTable {
            axes: Vec::new(),
            body: fs(&[1.0]),
        };
        let four = CalTable::numeric(
            vec![fs(&[0.0]), fs(&[0.0]), fs(&[0.0]), fs(&[0.0])],
            fs(&[1.0]),
        );
        assert!(validate(&zero).is_err());
        assert!(validate(&four).is_err());
    }

    #[test]
    fn body_shape_mismatch_fails_loud() {
        let c = CalTable::numeric(vec![fs(&[0.0, 1.0]), fs(&[0.0, 1.0])], fs(&[1.0, 2.0, 3.0]));
        assert!(matches!(
            lookup(&c, &vs(&[0.5, 0.5])),
            Err(EvalError::MissingCalibration { .. })
        ));
    }

    #[test]
    fn integral_body_kind_is_preserved() {
        let table = CalTable::numeric(
            vec![fs(&[0.0, 1.0])],
            vec![M1Scalar::UnsignedInteger(10), M1Scalar::UnsignedInteger(20)],
        );
        assert_eq!(
            lookup(&table, &[v(0.5)]).unwrap(),
            M1Scalar::UnsignedInteger(15)
        );
        assert!(lookup(&table, &[v(0.25)]).is_err());
    }

    #[test]
    fn integral_interpolation_does_not_round_through_binary32() {
        let signed = CalTable::numeric(
            vec![fs(&[0.0, 1.0])],
            vec![M1Scalar::Integer(16_777_216), M1Scalar::Integer(16_777_218)],
        );
        assert_eq!(
            lookup(&signed, &[v(0.5)]).unwrap(),
            M1Scalar::Integer(16_777_217)
        );

        let unsigned = CalTable::numeric(
            vec![fs(&[0.0, 1.0])],
            vec![
                M1Scalar::UnsignedInteger(u32::MAX - 2),
                M1Scalar::UnsignedInteger(u32::MAX),
            ],
        );
        assert_eq!(
            lookup(&unsigned, &[v(0.5)]).unwrap(),
            M1Scalar::UnsignedInteger(u32::MAX - 1)
        );

        let near_endpoint = CalTable::numeric(
            vec![vec![
                M1Scalar::UnsignedInteger(0),
                M1Scalar::UnsignedInteger(33_554_432),
            ]],
            vec![M1Scalar::Integer(0), M1Scalar::Integer(33_554_432)],
        );
        assert_eq!(
            lookup(&near_endpoint, &[M1Scalar::UnsignedInteger(33_554_431)]).unwrap(),
            M1Scalar::Integer(33_554_431)
        );
    }

    #[test]
    fn large_unsigned_axis_does_not_collapse_through_binary32() {
        let table = CalTable::numeric(
            vec![vec![
                M1Scalar::UnsignedInteger(u32::MAX - 2),
                M1Scalar::UnsignedInteger(u32::MAX),
            ]],
            fs(&[0.0, 10.0]),
        );
        assert_eq!(
            lookup(&table, &[M1Scalar::UnsignedInteger(u32::MAX - 1)]).unwrap(),
            f(5.0)
        );
    }

    #[test]
    fn fixed_point_body_interpolates_in_raw_storage() {
        let table = CalTable::numeric(
            vec![fs(&[0.0, 1.0])],
            vec![
                M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(10)),
                M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(20)),
            ],
        );
        assert_eq!(
            lookup(&table, &[v(0.5)]).unwrap(),
            M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(15))
        );
        assert!(lookup(&table, &[v(0.25)]).is_err());

        let above_binary32_integer_precision = CalTable::numeric(
            vec![fs(&[0.0, 1.0])],
            vec![
                M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(16_777_216)),
                M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(16_777_218)),
            ],
        );
        assert_eq!(
            lookup(&above_binary32_integer_precision, &[v(0.5)]).unwrap(),
            M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(16_777_217))
        );
    }
}
