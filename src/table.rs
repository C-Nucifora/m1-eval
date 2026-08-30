// SPDX-License-Identifier: GPL-3.0-or-later
//! N-dimensional clamped multilinear interpolation over a [`CalTable`].
//!
//! Given per-axis breakpoint vectors and a flat body, [`lookup`] returns the
//! multilinearly interpolated value at an arbitrary point. Inputs outside an
//! axis's breakpoint range are **clamped** to the nearest end (no
//! extrapolation).
//!
//! ## Body memory layout
//!
//! The body is row-major with **axis 0 (X) outermost** — matching
//! [`crate::calib::CalTable`]'s documented layout. For breakpoint indices
//! `(i0, i1, …, i_{n-1})` the flat offset is
//! `sum_k i_k * stride_k`, where `stride_k = prod_{j>k} len(axis_j)` (so the
//! last/innermost axis has stride 1). For a 2-D `nx*ny` table this is the
//! familiar `ix * ny + iy`.
//!
//! ## Clamp vs extend (assumption)
//!
//! Phase 1 **clamps** out-of-range inputs to the axis endpoints. MoTeC's exact
//! extrapolation behaviour (clamp vs linear extend past the last breakpoint) is
//! to be confirmed against M1 Sim during fidelity work; until then this is the
//! documented assumption, not silently-wrong output.

use crate::calib::CalTable;
use crate::error::EvalError;
use crate::value::{FixedPoint7dps, M1Scalar, M1ScalarKind};

/// Multilinear interpolation of `t` at `inputs` (one coordinate per axis).
///
/// - Arity must equal `t.axes.len()`, else [`EvalError::BadCall`].
/// - Each input is clamped to its axis's `[first, last]` range.
/// - Empty/malformed tables fail loud rather than guessing.
pub fn lookup(t: &CalTable, inputs: &[M1Scalar]) -> Result<M1Scalar, EvalError> {
    if inputs.len() != t.axes.len() {
        return Err(EvalError::BadCall {
            detail: format!(
                "table lookup arity mismatch: {} input(s) for a {}-axis table",
                inputs.len(),
                t.axes.len()
            ),
        });
    }
    if inputs
        .iter()
        .any(|input| matches!(input, M1Scalar::FloatingPoint(value) if value.is_nan()))
    {
        return Err(EvalError::BadCall {
            detail: "table lookup coordinate is NaN".to_string(),
        });
    }

    // A scalar (0-axis) "table" is just its single body cell.
    if t.axes.is_empty() {
        return match t.body.as_slice() {
            [v] => Ok(*v),
            _ => Err(EvalError::MissingCalibration {
                path: "0-axis table without exactly one body cell".to_string(),
            }),
        };
    }

    // Expected body length is the product of axis lengths.
    let mut expected = 1usize;
    for axis in &t.axes {
        if axis.is_empty() {
            return Err(EvalError::MissingCalibration {
                path: "table axis has no breakpoints".to_string(),
            });
        }
        expected = expected.saturating_mul(axis.len());
    }
    if t.body.len() != expected {
        return Err(EvalError::MissingCalibration {
            path: format!(
                "table body has {} cells, expected {} for axis shape",
                t.body.len(),
                expected
            ),
        });
    }

    let body_kind = t.body[0].kind();
    if t.body.iter().any(|value| value.kind() != body_kind) {
        return Err(EvalError::MissingCalibration {
            path: "table body mixes M1 scalar types".to_string(),
        });
    }

    // Per-axis: lower bracket index and fractional position in [0, 1].
    let n = t.axes.len();
    let mut lo = vec![0usize; n];
    let mut frac = vec![0.0f64; n];
    for (k, axis) in t.axes.iter().enumerate() {
        let (i, f) = bracket(axis, inputs[k].as_f64());
        lo[k] = i;
        frac[k] = f;
    }

    // Row-major strides with axis 0 outermost (innermost stride = 1).
    let mut stride = vec![1usize; n];
    for k in (0..n - 1).rev() {
        stride[k] = stride[k + 1] * t.axes[k + 1].len();
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

/// For a sorted-ascending breakpoint axis, return the lower bracket index and
/// the fractional position of `x` between that breakpoint and the next.
///
/// Out-of-range inputs clamp: below `axis[0]` -> index 0, frac 0; at or above
/// `axis[last]` -> index `last-1`, frac 1 (so the upper corner is the last
/// breakpoint). A single-breakpoint axis returns index 0, frac 0.
fn bracket(axis: &[M1Scalar], x: f64) -> (usize, f64) {
    let last = axis.len() - 1;
    if last == 0 {
        return (0, 0.0);
    }
    if x <= axis[0].as_f64() {
        return (0, 0.0);
    }
    if x >= axis[last].as_f64() {
        return (last - 1, 1.0);
    }
    // Linear scan: axes are short (calibration tables are small), so a binary
    // search would not pay for itself and would complicate non-monotonic guards.
    for i in 0..last {
        let a = axis[i].as_f64();
        let b = axis[i + 1].as_f64();
        if x >= a && x <= b {
            // b > a here because x is strictly inside (a..last] and not <= a.
            let span = b - a;
            let f = if span > 0.0 { (x - a) / span } else { 0.0 };
            return (i, f);
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

    fn t() -> CalTable {
        CalTable {
            axes: vec![fs(&[0.0, 100.0]), fs(&[0.0, 1.0])],
            body: fs(&[10.0, 20.0, 30.0, 40.0]),
        }
    }

    #[test]
    fn corners_and_midpoint() {
        assert_eq!(lookup(&t(), &fs(&[0.0, 0.0])).unwrap(), f(10.0));
        assert_eq!(lookup(&t(), &fs(&[100.0, 1.0])).unwrap(), f(40.0));
        assert_eq!(lookup(&t(), &fs(&[0.0, 1.0])).unwrap(), f(20.0));
        assert_eq!(lookup(&t(), &fs(&[100.0, 0.0])).unwrap(), f(30.0));
        assert_eq!(lookup(&t(), &fs(&[50.0, 0.0])).unwrap(), f(20.0));
        assert_eq!(lookup(&t(), &fs(&[0.0, 0.5])).unwrap(), f(15.0));
        assert_eq!(lookup(&t(), &fs(&[50.0, 0.5])).unwrap(), f(25.0));
    }

    #[test]
    fn clamps_out_of_range() {
        assert_eq!(lookup(&t(), &fs(&[-5.0, 0.0])).unwrap(), f(10.0));
        assert_eq!(lookup(&t(), &fs(&[999.0, 2.0])).unwrap(), f(40.0));
        assert_eq!(lookup(&t(), &fs(&[-5.0, 0.5])).unwrap(), f(15.0));
        assert_eq!(lookup(&t(), &fs(&[999.0, 0.5])).unwrap(), f(35.0));
    }

    #[test]
    fn nan_coordinate_fails_loud_instead_of_clamping_high() {
        let table = CalTable {
            axes: vec![fs(&[0.0, 1.0])],
            body: fs(&[10.0, 20.0]),
        };
        assert!(matches!(
            lookup(&table, &[f(f32::NAN)]),
            Err(EvalError::BadCall { .. })
        ));
    }

    #[test]
    fn arity_mismatch_is_error() {
        assert!(matches!(
            lookup(&t(), &fs(&[1.0])),
            Err(EvalError::BadCall { .. })
        ));
        assert!(matches!(
            lookup(&t(), &fs(&[1.0, 2.0, 3.0])),
            Err(EvalError::BadCall { .. })
        ));
    }

    #[test]
    fn one_dimensional_interpolation() {
        let c = CalTable {
            axes: vec![fs(&[0.0, 10.0, 20.0])],
            body: fs(&[0.0, 100.0, 50.0]),
        };
        for (input, expected) in [
            (0.0, 0.0),
            (10.0, 100.0),
            (5.0, 50.0),
            (15.0, 75.0),
            (-1.0, 0.0),
            (99.0, 50.0),
        ] {
            assert_eq!(lookup(&c, &[f(input)]).unwrap(), f(expected));
        }
    }

    #[test]
    fn three_dimensional_corners() {
        let c = CalTable {
            axes: vec![fs(&[0.0, 1.0]), fs(&[0.0, 1.0]), fs(&[0.0, 1.0])],
            body: (0..8).map(|value| f(value as f32)).collect(),
        };
        assert_eq!(lookup(&c, &fs(&[0.0, 0.0, 0.0])).unwrap(), f(0.0));
        assert_eq!(lookup(&c, &fs(&[0.0, 0.0, 1.0])).unwrap(), f(1.0));
        assert_eq!(lookup(&c, &fs(&[0.0, 1.0, 0.0])).unwrap(), f(2.0));
        assert_eq!(lookup(&c, &fs(&[1.0, 0.0, 0.0])).unwrap(), f(4.0));
        assert_eq!(lookup(&c, &fs(&[1.0, 1.0, 1.0])).unwrap(), f(7.0));
        assert_eq!(lookup(&c, &fs(&[0.5, 0.5, 0.5])).unwrap(), f(3.5));
    }

    #[test]
    fn single_breakpoint_axis() {
        let c = CalTable {
            axes: vec![fs(&[5.0]), fs(&[0.0, 1.0])],
            body: fs(&[10.0, 20.0]),
        };
        assert_eq!(lookup(&c, &fs(&[5.0, 0.0])).unwrap(), f(10.0));
        assert_eq!(lookup(&c, &fs(&[999.0, 1.0])).unwrap(), f(20.0));
        assert_eq!(lookup(&c, &fs(&[0.0, 0.5])).unwrap(), f(15.0));
    }

    #[test]
    fn body_shape_mismatch_fails_loud() {
        let c = CalTable {
            axes: vec![fs(&[0.0, 1.0]), fs(&[0.0, 1.0])],
            body: fs(&[1.0, 2.0, 3.0]),
        };
        assert!(matches!(
            lookup(&c, &fs(&[0.5, 0.5])),
            Err(EvalError::MissingCalibration { .. })
        ));
    }

    #[test]
    fn integral_body_kind_is_preserved() {
        let table = CalTable {
            axes: vec![fs(&[0.0, 1.0])],
            body: vec![M1Scalar::UnsignedInteger(10), M1Scalar::UnsignedInteger(20)],
        };
        assert_eq!(
            lookup(&table, &[f(0.5)]).unwrap(),
            M1Scalar::UnsignedInteger(15)
        );
        assert!(lookup(&table, &[f(0.25)]).is_err());
    }

    #[test]
    fn integral_interpolation_does_not_round_through_binary32() {
        let signed = CalTable {
            axes: vec![fs(&[0.0, 1.0])],
            body: vec![M1Scalar::Integer(16_777_216), M1Scalar::Integer(16_777_218)],
        };
        assert_eq!(
            lookup(&signed, &[f(0.5)]).unwrap(),
            M1Scalar::Integer(16_777_217)
        );

        let unsigned = CalTable {
            axes: vec![fs(&[0.0, 1.0])],
            body: vec![
                M1Scalar::UnsignedInteger(u32::MAX - 2),
                M1Scalar::UnsignedInteger(u32::MAX),
            ],
        };
        assert_eq!(
            lookup(&unsigned, &[f(0.5)]).unwrap(),
            M1Scalar::UnsignedInteger(u32::MAX - 1)
        );

        let near_endpoint = CalTable {
            axes: vec![vec![
                M1Scalar::UnsignedInteger(0),
                M1Scalar::UnsignedInteger(33_554_432),
            ]],
            body: vec![M1Scalar::Integer(0), M1Scalar::Integer(33_554_432)],
        };
        assert_eq!(
            lookup(&near_endpoint, &[M1Scalar::UnsignedInteger(33_554_431)]).unwrap(),
            M1Scalar::Integer(33_554_431)
        );
    }

    #[test]
    fn large_unsigned_axis_does_not_collapse_through_binary32() {
        let table = CalTable {
            axes: vec![vec![
                M1Scalar::UnsignedInteger(u32::MAX - 2),
                M1Scalar::UnsignedInteger(u32::MAX),
            ]],
            body: fs(&[0.0, 10.0]),
        };
        assert_eq!(
            lookup(&table, &[M1Scalar::UnsignedInteger(u32::MAX - 1)]).unwrap(),
            f(5.0)
        );
    }

    #[test]
    fn fixed_point_body_interpolates_in_raw_storage() {
        let table = CalTable {
            axes: vec![fs(&[0.0, 1.0])],
            body: vec![
                M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(10)),
                M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(20)),
            ],
        };
        assert_eq!(
            lookup(&table, &[f(0.5)]).unwrap(),
            M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(15))
        );
        assert!(lookup(&table, &[f(0.25)]).is_err());

        let above_binary32_integer_precision = CalTable {
            axes: vec![fs(&[0.0, 1.0])],
            body: vec![
                M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(16_777_216)),
                M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(16_777_218)),
            ],
        };
        assert_eq!(
            lookup(&above_binary32_integer_precision, &[f(0.5)]).unwrap(),
            M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(16_777_217))
        );
    }
}
