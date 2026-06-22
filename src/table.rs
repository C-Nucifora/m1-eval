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

/// Multilinear interpolation of `t` at `inputs` (one coordinate per axis).
///
/// - Arity must equal `t.axes.len()`, else [`EvalError::BadCall`].
/// - Each input is clamped to its axis's `[first, last]` range.
/// - Empty/malformed tables fail loud rather than guessing.
pub fn lookup(t: &CalTable, inputs: &[f64]) -> Result<f64, EvalError> {
    if inputs.len() != t.axes.len() {
        return Err(EvalError::BadCall {
            detail: format!(
                "table lookup arity mismatch: {} input(s) for a {}-axis table",
                inputs.len(),
                t.axes.len()
            ),
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

    // Per-axis: lower bracket index and fractional position in [0, 1].
    let n = t.axes.len();
    let mut lo = vec![0usize; n];
    let mut frac = vec![0.0f64; n];
    for (k, axis) in t.axes.iter().enumerate() {
        let (i, f) = bracket(axis, inputs[k]);
        lo[k] = i;
        frac[k] = f;
    }

    // Row-major strides with axis 0 outermost (innermost stride = 1).
    let mut stride = vec![1usize; n];
    for k in (0..n - 1).rev() {
        stride[k] = stride[k + 1] * t.axes[k + 1].len();
    }

    // Blend over the 2^n corners of the bracketing hypercube. Each corner
    // chooses the lower or upper breakpoint per axis; its weight is the product
    // of (1 - frac) for "lower" axes and frac for "upper" axes.
    let mut acc = 0.0;
    for corner in 0..(1usize << n) {
        let mut weight = 1.0;
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
            weight *= if upper { frac[k] } else { 1.0 - frac[k] };
            offset += idx * stride[k];
        }
        if weight != 0.0 {
            acc += weight * t.body[offset];
        }
    }
    Ok(acc)
}

/// For a sorted-ascending breakpoint axis, return the lower bracket index and
/// the fractional position of `x` between that breakpoint and the next.
///
/// Out-of-range inputs clamp: below `axis[0]` -> index 0, frac 0; at or above
/// `axis[last]` -> index `last-1`, frac 1 (so the upper corner is the last
/// breakpoint). A single-breakpoint axis returns index 0, frac 0.
fn bracket(axis: &[f64], x: f64) -> (usize, f64) {
    let last = axis.len() - 1;
    if last == 0 {
        return (0, 0.0);
    }
    if x <= axis[0] {
        return (0, 0.0);
    }
    if x >= axis[last] {
        return (last - 1, 1.0);
    }
    // Linear scan: axes are short (calibration tables are small), so a binary
    // search would not pay for itself and would complicate non-monotonic guards.
    for i in 0..last {
        let a = axis[i];
        let b = axis[i + 1];
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

    /// 2-D table; body laid out row-major over (x then y): index = ix*ny + iy.
    /// axes: x in {0,100}, y in {0,1}; body (ix,iy) ->
    ///   (0,0)=10 (0,1)=20 (1,0)=30 (1,1)=40
    fn t() -> CalTable {
        CalTable {
            axes: vec![vec![0.0, 100.0], vec![0.0, 1.0]],
            body: vec![10.0, 20.0, 30.0, 40.0],
        }
    }

    #[test]
    fn corners_and_midpoint() {
        assert_eq!(lookup(&t(), &[0.0, 0.0]).unwrap(), 10.0);
        assert_eq!(lookup(&t(), &[100.0, 1.0]).unwrap(), 40.0);
        assert_eq!(lookup(&t(), &[0.0, 1.0]).unwrap(), 20.0);
        assert_eq!(lookup(&t(), &[100.0, 0.0]).unwrap(), 30.0);
        // halfway in x at y=0: between (10, 30) -> 20.
        assert_eq!(lookup(&t(), &[50.0, 0.0]).unwrap(), 20.0);
        // halfway in y at x=0: between (10, 20) -> 15.
        assert_eq!(lookup(&t(), &[0.0, 0.5]).unwrap(), 15.0);
        // centre of the cell: mean of the four corners = 25.
        assert_eq!(lookup(&t(), &[50.0, 0.5]).unwrap(), 25.0);
    }

    #[test]
    fn clamps_out_of_range() {
        assert_eq!(lookup(&t(), &[-5.0, 0.0]).unwrap(), 10.0);
        assert_eq!(lookup(&t(), &[999.0, 2.0]).unwrap(), 40.0);
        // Clamp on one axis only.
        assert_eq!(lookup(&t(), &[-5.0, 0.5]).unwrap(), 15.0);
        assert_eq!(lookup(&t(), &[999.0, 0.5]).unwrap(), 35.0);
    }

    #[test]
    fn arity_mismatch_is_error() {
        assert!(matches!(
            lookup(&t(), &[1.0]),
            Err(EvalError::BadCall { .. })
        ));
        assert!(matches!(
            lookup(&t(), &[1.0, 2.0, 3.0]),
            Err(EvalError::BadCall { .. })
        ));
    }

    #[test]
    fn one_dimensional_interpolation() {
        let c = CalTable {
            axes: vec![vec![0.0, 10.0, 20.0]],
            body: vec![0.0, 100.0, 50.0],
        };
        assert_eq!(lookup(&c, &[0.0]).unwrap(), 0.0);
        assert_eq!(lookup(&c, &[10.0]).unwrap(), 100.0);
        assert_eq!(lookup(&c, &[5.0]).unwrap(), 50.0); // midway 0..10 of (0,100)
        assert_eq!(lookup(&c, &[15.0]).unwrap(), 75.0); // midway 10..20 of (100,50)
        assert_eq!(lookup(&c, &[-1.0]).unwrap(), 0.0); // clamp low
        assert_eq!(lookup(&c, &[99.0]).unwrap(), 50.0); // clamp high
    }

    #[test]
    fn three_dimensional_corners() {
        // 2x2x2 cube; body row-major over (x,y,z): index = ((ix*2)+iy)*2+iz.
        // Set body[i] = i so each corner is its own flat index.
        let c = CalTable {
            axes: vec![vec![0.0, 1.0], vec![0.0, 1.0], vec![0.0, 1.0]],
            body: (0..8).map(|i| i as f64).collect(),
        };
        assert_eq!(lookup(&c, &[0.0, 0.0, 0.0]).unwrap(), 0.0);
        assert_eq!(lookup(&c, &[0.0, 0.0, 1.0]).unwrap(), 1.0);
        assert_eq!(lookup(&c, &[0.0, 1.0, 0.0]).unwrap(), 2.0);
        assert_eq!(lookup(&c, &[1.0, 0.0, 0.0]).unwrap(), 4.0);
        assert_eq!(lookup(&c, &[1.0, 1.0, 1.0]).unwrap(), 7.0);
        // Centre of the cube: mean of 0..7 = 3.5.
        assert_eq!(lookup(&c, &[0.5, 0.5, 0.5]).unwrap(), 3.5);
    }

    #[test]
    fn single_breakpoint_axis() {
        // A degenerate axis with one breakpoint always returns that slice.
        let c = CalTable {
            axes: vec![vec![5.0], vec![0.0, 1.0]],
            body: vec![10.0, 20.0],
        };
        assert_eq!(lookup(&c, &[5.0, 0.0]).unwrap(), 10.0);
        assert_eq!(lookup(&c, &[999.0, 1.0]).unwrap(), 20.0);
        assert_eq!(lookup(&c, &[0.0, 0.5]).unwrap(), 15.0);
    }

    #[test]
    fn body_shape_mismatch_fails_loud() {
        let c = CalTable {
            axes: vec![vec![0.0, 1.0], vec![0.0, 1.0]],
            body: vec![1.0, 2.0, 3.0], // expected 4
        };
        assert!(matches!(
            lookup(&c, &[0.5, 0.5]),
            Err(EvalError::MissingCalibration { .. })
        ));
    }
}
