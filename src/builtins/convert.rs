// SPDX-License-Identifier: GPL-3.0-or-later
//! The pure `Convert.*` numeric-conversion builtins (Tier-1).
//!
//! `ToInteger` and `ToUnsignedInteger` round to the nearest representable M1
//! integer, with halfway cases rounded away from zero. Values beyond the target
//! range select its nearest endpoint, so a negative unsigned conversion becomes
//! zero rather than a positive magnitude.
//!
//! The pinned catalogue declares `ToFixed7DP` for an integral argument. It is a
//! numeric conversion, not a raw-bit reinterpretation: `1` becomes `1.0000000`.
//! Because Fixed Point 7dps stores a signed 32-bit value scaled by `10^-7`, only
//! whole-number inputs from `-214` through `214` fit. Calls outside that range
//! fail loud instead of wrapping or silently clipping.

use crate::error::EvalError;
use crate::value::{FixedPoint7dps, M1Scalar, Value};

/// Evaluate one `Convert.<method>` call.
///
/// Returns `Ok(None)` when this module does not implement `method`. The caller
/// validates arity against the pinned intrinsic catalogue before dispatch.
pub fn call(method: &str, args: &[Value]) -> Result<Option<Value>, EvalError> {
    let value = match method {
        "ToInteger" => to_integer(&args[0])?,
        "ToUnsignedInteger" => to_unsigned_integer(&args[0])?,
        "ToFixed7DP" => to_fixed_7dps(&args[0])?,
        _ => return Ok(None),
    };
    Ok(Some(value))
}

fn to_integer(argument: &Value) -> Result<Value, EvalError> {
    let result = match argument.m1_scalar()? {
        M1Scalar::Integer(value) => value,
        M1Scalar::UnsignedInteger(value) => value.min(i32::MAX as u32) as i32,
        M1Scalar::FloatingPoint(value) => round_f32_to_i32(value)?,
        M1Scalar::FixedPoint7dps(value) => round_fixed_to_i32(value),
    };
    Ok(Value::m1_integer(result))
}

fn to_unsigned_integer(argument: &Value) -> Result<Value, EvalError> {
    let result = match argument.m1_scalar()? {
        M1Scalar::Integer(value) => value.max(0) as u32,
        M1Scalar::UnsignedInteger(value) => value,
        M1Scalar::FloatingPoint(value) => round_f32_to_u32(value)?,
        M1Scalar::FixedPoint7dps(value) => round_fixed_to_i32(value).max(0) as u32,
    };
    Ok(Value::m1_unsigned(result))
}

fn to_fixed_7dps(argument: &Value) -> Result<Value, EvalError> {
    let integer = match argument.m1_scalar()? {
        M1Scalar::Integer(value) => value,
        M1Scalar::UnsignedInteger(value) => {
            i32::try_from(value).map_err(|_| fixed_range_error(value))?
        }
        other => {
            return Err(EvalError::TypeError {
                detail: format!(
                    "Convert.ToFixed7DP expects an M1 Integer or UnsignedInteger, got {other:?}"
                ),
            });
        }
    };
    let raw = integer
        .checked_mul(10_000_000)
        .ok_or_else(|| fixed_range_error(integer))?;
    Ok(Value::M1(M1Scalar::FixedPoint7dps(
        FixedPoint7dps::from_raw(raw),
    )))
}

fn fixed_range_error(value: impl std::fmt::Display) -> EvalError {
    EvalError::TypeError {
        detail: format!(
            "Convert.ToFixed7DP input {value} is outside the Fixed Point 7dps range; integral inputs must be between -214 and 214"
        ),
    }
}

fn round_f32_to_i32(value: f32) -> Result<i32, EvalError> {
    if value.is_nan() {
        return Err(not_convertible("Integer", value));
    }
    Ok(value.round() as i32)
}

fn round_f32_to_u32(value: f32) -> Result<u32, EvalError> {
    if value.is_nan() {
        return Err(not_convertible("UnsignedInteger", value));
    }
    Ok(value.round() as u32)
}

/// Round an exact scaled fixed-point value to the nearest integer. Rust integer
/// division truncates toward zero, so adding one unit when the remainder is at
/// least half the scale implements halfway-away-from-zero without a float.
fn round_fixed_to_i32(value: FixedPoint7dps) -> i32 {
    let raw = value.raw();
    let scale = 10_000_000_i32;
    let whole = raw / scale;
    let remainder = raw % scale;
    let adjustment = if remainder.unsigned_abs() * 2 >= scale as u32 {
        remainder.signum()
    } else {
        0
    };
    whole + adjustment
}

fn not_convertible(target: &str, value: f32) -> EvalError {
    EvalError::TypeError {
        detail: format!("{value:?} has no nearest M1 {target} value"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(raw: i32) -> Value {
        Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(raw)))
    }

    fn ok(method: &str, args: &[Value]) -> Value {
        call(method, args).unwrap().unwrap()
    }

    #[test]
    fn to_integer_rounds_to_nearest_with_ties_away_from_zero() {
        for (source, expected) in [
            (2.4, 2),
            (2.6, 3),
            (-2.4, -2),
            (-2.6, -3),
            (2.5, 3),
            (-2.5, -3),
            (0.0, 0),
        ] {
            assert_eq!(
                ok("ToInteger", &[Value::m1_float(source)]),
                Value::m1_integer(expected),
                "failed to round {source}"
            );
        }
    }

    #[test]
    fn to_integer_clamps_to_the_signed_range() {
        assert_eq!(
            ok("ToInteger", &[Value::m1_unsigned(u32::MAX)]),
            Value::m1_integer(i32::MAX)
        );
        assert_eq!(
            ok("ToInteger", &[Value::m1_float(f32::MAX)]),
            Value::m1_integer(i32::MAX)
        );
        assert_eq!(
            ok("ToInteger", &[Value::m1_float(-f32::MAX)]),
            Value::m1_integer(i32::MIN)
        );
        assert_eq!(
            ok("ToInteger", &[Value::m1_integer(i32::MIN)]),
            Value::m1_integer(i32::MIN)
        );
    }

    #[test]
    fn to_unsigned_rounds_then_clamps_negative_values_to_zero() {
        assert_eq!(
            ok("ToUnsignedInteger", &[Value::m1_float(3.6)]),
            Value::m1_unsigned(4)
        );
        assert_eq!(
            ok("ToUnsignedInteger", &[Value::m1_float(3.5)]),
            Value::m1_unsigned(4)
        );
        for source in [-0.4, -0.5, -2.4, -2.5, -2.6] {
            assert_eq!(
                ok("ToUnsignedInteger", &[Value::m1_float(source)]),
                Value::m1_unsigned(0),
                "negative source {source} must not become its magnitude"
            );
        }
        assert_eq!(
            ok("ToUnsignedInteger", &[Value::m1_integer(i32::MIN)]),
            Value::m1_unsigned(0)
        );
        assert_eq!(
            ok("ToUnsignedInteger", &[Value::m1_float(f32::MAX)]),
            Value::m1_unsigned(u32::MAX)
        );
    }

    #[test]
    fn fixed_point_arguments_round_exactly_without_binary_float() {
        for (raw, expected) in [
            (14_999_999, 1),
            (15_000_000, 2),
            (-14_999_999, -1),
            (-15_000_000, -2),
        ] {
            assert_eq!(ok("ToInteger", &[fixed(raw)]), Value::m1_integer(expected));
        }
        assert_eq!(
            ok("ToUnsignedInteger", &[fixed(-15_000_000)]),
            Value::m1_unsigned(0)
        );
    }

    #[test]
    fn to_fixed_7dps_is_numeric_and_covers_its_integral_domain() {
        for (source, raw) in [
            (-214, -2_140_000_000),
            (-1, -10_000_000),
            (0, 0),
            (1, 10_000_000),
            (214, 2_140_000_000),
        ] {
            assert_eq!(ok("ToFixed7DP", &[Value::m1_integer(source)]), fixed(raw));
        }
        assert_eq!(
            ok("ToFixed7DP", &[Value::m1_unsigned(214)]),
            fixed(2_140_000_000)
        );
    }

    #[test]
    fn to_fixed_7dps_rejects_one_beyond_each_integral_boundary() {
        for argument in [Value::m1_integer(-215), Value::m1_integer(215)] {
            match call("ToFixed7DP", &[argument]) {
                Err(EvalError::TypeError { detail }) => {
                    assert!(detail.contains("between -214 and 214"), "{detail}");
                }
                other => panic!("expected fixed-point range error, got {other:?}"),
            }
        }
        assert!(matches!(
            call("ToFixed7DP", &[Value::m1_unsigned(215)]),
            Err(EvalError::TypeError { .. })
        ));
    }

    #[test]
    fn invalid_conversion_inputs_fail_loud() {
        assert!(matches!(
            call("ToInteger", &[Value::m1_float(f32::NAN)]),
            Err(EvalError::TypeError { .. })
        ));
        assert!(matches!(
            call("ToFixed7DP", &[Value::m1_float(1.0)]),
            Err(EvalError::TypeError { .. })
        ));
        assert!(matches!(
            call("ToInteger", &[Value::Str("x".into())]),
            Err(EvalError::TypeError { .. })
        ));
    }

    #[test]
    fn unimplemented_method_returns_none() {
        assert!(call("NotAMethod", &[]).unwrap().is_none());
    }
}
