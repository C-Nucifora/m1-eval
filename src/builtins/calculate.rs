// SPDX-License-Identifier: GPL-3.0-or-later
//! The pure `Calculate.*` math builtins (Tier-1).
//!
//! These functions operate directly on the runtime's M1-width scalar values.
//! Integer overloads return an M1 integer family, while floating overloads
//! calculate and return IEEE-754 binary32 values. The behavior is paraphrased
//! from the local M1 development manual and pinned intrinsic catalogue; no
//! proprietary source text or data is included here.
//!
//! Stateful `Calculate` methods (`Stable`, `Hysteresis`, `Between`, `Beyond`)
//! are handled by the stateful builtin engine and are not implemented here.

use crate::error::EvalError;
use crate::value::{M1Scalar, Value};
use m1_typecheck::types::{ValueType, numeric_join};

/// Evaluate one `Calculate.<method>` call.
///
/// Returns `Ok(None)` when this pure engine does not implement `method`, so the
/// dispatcher can try the appropriate route or fail loud. The dispatcher
/// validates arity against the pinned intrinsic catalogue before calling here.
pub fn call(method: &str, args: &[Value]) -> Result<Option<Value>, EvalError> {
    let value = match method {
        "Max" => binary_minmax(args, true)?,
        "Min" => binary_minmax(args, false)?,
        "Absolute" => absolute(args)?,
        "Average" => average(args)?,
        "Modulo" => modulo(args)?,
        "Bias" => bias(args)?,
        "PI" => Value::m1_float(std::f32::consts::PI),
        "NAN" => Value::m1_float(f32::NAN),
        "Infinity" => Value::m1_float(f32::INFINITY),
        "MaximumFloat" => Value::m1_float(f32::MAX),
        "Floor" => Value::m1_float(unary_f32(args)?.floor()),
        "Ceiling" => Value::m1_float(unary_f32(args)?.ceil()),
        "Power" => {
            let (base, exponent) = two_f32(args)?;
            Value::m1_float(base.powf(exponent))
        }
        "FastSquareRoot" => Value::m1_float(unary_f32(args)?.sqrt()),
        "IsNAN" => Value::Bool(unary_f32(args)?.is_nan()),
        "IsFinite" => Value::Bool(unary_f32(args)?.is_finite()),
        "FastSin" => Value::m1_float(unary_f32(args)?.sin()),
        "FastCos" => Value::m1_float(unary_f32(args)?.cos()),
        "FastTan" => Value::m1_float(unary_f32(args)?.tan()),
        // The standard-library implementations remain explicit evaluator
        // assumptions for the firmware's approximation details.
        "InverseSin" => Value::m1_float(unary_f32(args)?.asin()),
        "InverseCos" => Value::m1_float(unary_f32(args)?.acos()),
        "InverseTan" => Value::m1_float(unary_f32(args)?.atan()),
        "InverseTan2" => {
            let (y, x) = two_f32(args)?;
            Value::m1_float(y.atan2(x))
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}

/// Return the greater or lesser argument under M1 numeric promotion.
fn binary_minmax(args: &[Value], want_max: bool) -> Result<Value, EvalError> {
    let (left, right) = (&args[0], &args[1]);
    let joined = numeric_join(value_type(left), value_type(right));
    match joined {
        ValueType::Float => {
            let left = left.m1_scalar()?.as_f32();
            let right = right.m1_scalar()?.as_f32();
            Ok(Value::m1_float(if want_max {
                left.max(right)
            } else {
                left.min(right)
            }))
        }
        ValueType::Unsigned => {
            let left = as_u32_bits(left)?;
            let right = as_u32_bits(right)?;
            Ok(Value::m1_unsigned(if want_max {
                left.max(right)
            } else {
                left.min(right)
            }))
        }
        ValueType::Integer => {
            let left = as_i32(left)?;
            let right = as_i32(right)?;
            Ok(Value::m1_integer(if want_max {
                left.max(right)
            } else {
                left.min(right)
            }))
        }
        _ => Err(non_numeric("min/max", left, right)),
    }
}

/// Return the remainder of the first argument divided by the second.
fn modulo(args: &[Value]) -> Result<Value, EvalError> {
    let (left, right) = (&args[0], &args[1]);
    match numeric_join(value_type(left), value_type(right)) {
        ValueType::Float => {
            let divisor = right.m1_scalar()?.as_f32();
            if divisor == 0.0 {
                return Err(modulo_by_zero());
            }
            Ok(Value::m1_float(left.m1_scalar()?.as_f32() % divisor))
        }
        ValueType::Unsigned => {
            let divisor = as_u32_bits(right)?;
            if divisor == 0 {
                return Err(modulo_by_zero());
            }
            Ok(Value::m1_unsigned(as_u32_bits(left)? % divisor))
        }
        ValueType::Integer => {
            let divisor = as_i32(right)?;
            if divisor == 0 {
                return Err(modulo_by_zero());
            }
            Ok(Value::m1_integer(as_i32(left)?.wrapping_rem(divisor)))
        }
        _ => Err(non_numeric("Modulo", left, right)),
    }
}

/// Return the manual's biased average of two values.
///
/// A bias of `-1` selects the lower argument, `0` selects their average, and
/// `1` selects the higher argument. Intermediate values interpolate from the
/// midpoint by half the absolute separation. Evaluation is binary32 throughout.
fn bias(args: &[Value]) -> Result<Value, EvalError> {
    let left = args[0].m1_scalar()?.as_f32();
    let right = args[1].m1_scalar()?.as_f32();
    let bias = args[2].m1_scalar()?.as_f32();
    let average = left.midpoint(right);
    let half_separation = left.midpoint(-right).abs();
    let result = if bias == -1.0 {
        left.min(right)
    } else if bias == 0.0 {
        average
    } else if bias == 1.0 {
        left.max(right)
    } else {
        average + bias * half_separation
    };
    Ok(Value::m1_float(result))
}

/// Return the magnitude of one numeric value, preserving its documented
/// integer or floating overload family.
fn absolute(args: &[Value]) -> Result<Value, EvalError> {
    match args[0].m1_scalar()? {
        M1Scalar::Integer(value) => {
            value
                .checked_abs()
                .map(Value::m1_integer)
                .ok_or_else(|| EvalError::TypeError {
                    detail: format!("Calculate.Absolute({value}) is outside the M1 Integer range"),
                })
        }
        M1Scalar::UnsignedInteger(value) => Ok(Value::m1_unsigned(value)),
        M1Scalar::FloatingPoint(value) => Ok(Value::m1_float(value.abs())),
        M1Scalar::FixedPoint7dps(value) => Ok(Value::m1_float(value.as_f64().abs() as f32)),
    }
}

/// Return the arithmetic mean using the overload selected by the arguments.
///
/// The integral overload returns the joined integral family and discards a half
/// unit toward zero. The floating overload calculates and returns binary32.
/// Wider intermediates keep equal extrema from overflowing before the division.
fn average(args: &[Value]) -> Result<Value, EvalError> {
    let (left, right) = (&args[0], &args[1]);
    match numeric_join(value_type(left), value_type(right)) {
        ValueType::Float => {
            let left = left.m1_scalar()?.as_f32();
            let right = right.m1_scalar()?.as_f32();
            Ok(Value::m1_float(left.midpoint(right)))
        }
        ValueType::Unsigned => {
            let sum = u64::from(as_u32_bits(left)?) + u64::from(as_u32_bits(right)?);
            Ok(Value::m1_unsigned((sum / 2) as u32))
        }
        ValueType::Integer => {
            let sum = i64::from(as_i32(left)?) + i64::from(as_i32(right)?);
            Ok(Value::m1_integer((sum / 2) as i32))
        }
        _ => Err(non_numeric("Average", left, right)),
    }
}

fn unary_f32(args: &[Value]) -> Result<f32, EvalError> {
    Ok(args[0].m1_scalar()?.as_f32())
}

fn two_f32(args: &[Value]) -> Result<(f32, f32), EvalError> {
    Ok((args[0].m1_scalar()?.as_f32(), args[1].m1_scalar()?.as_f32()))
}

fn value_type(value: &Value) -> ValueType {
    match value {
        Value::M1(M1Scalar::Integer(_)) => ValueType::Integer,
        Value::M1(M1Scalar::UnsignedInteger(_)) => ValueType::Unsigned,
        Value::M1(M1Scalar::FloatingPoint(_) | M1Scalar::FixedPoint7dps(_)) => ValueType::Float,
        Value::Bool(_) => ValueType::Boolean,
        Value::Enum { id, .. } => ValueType::Enum(*id),
        Value::Str(_) => ValueType::String,
        Value::Int(_) | Value::Uint(_) | Value::Float(_) => ValueType::Unknown,
    }
}

fn as_i32(value: &Value) -> Result<i32, EvalError> {
    match value.m1_scalar()? {
        M1Scalar::Integer(value) => Ok(value),
        other => Err(EvalError::TypeError {
            detail: format!("{other:?} is not an M1 Integer"),
        }),
    }
}

fn as_u32_bits(value: &Value) -> Result<u32, EvalError> {
    match value.m1_scalar()? {
        M1Scalar::Integer(value) => Ok(value as u32),
        M1Scalar::UnsignedInteger(value) => Ok(value),
        other => Err(EvalError::TypeError {
            detail: format!("{other:?} is not an integral M1 value"),
        }),
    }
}

fn non_numeric(operation: &str, left: &Value, right: &Value) -> EvalError {
    EvalError::TypeError {
        detail: format!("Calculate.{operation} requires numeric operands, got {left:?}, {right:?}"),
    }
}

fn modulo_by_zero() -> EvalError {
    EvalError::TypeError {
        detail: "Calculate.Modulo by zero".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::FixedPoint7dps;

    fn ok(method: &str, args: &[Value]) -> Value {
        call(method, args).unwrap().unwrap()
    }

    #[test]
    fn max_and_min_preserve_m1_numeric_families() {
        assert_eq!(
            ok("Max", &[Value::m1_integer(2), Value::m1_integer(3)]),
            Value::m1_integer(3)
        );
        assert_eq!(
            ok("Min", &[Value::m1_integer(2), Value::m1_integer(3)]),
            Value::m1_integer(2)
        );
        assert_eq!(
            ok("Max", &[Value::m1_integer(2), Value::m1_float(3.5)]),
            Value::m1_float(3.5)
        );
        assert_eq!(
            ok("Min", &[Value::m1_integer(2), Value::m1_float(3.5)]),
            Value::m1_float(2.0)
        );
        assert_eq!(
            ok("Max", &[Value::m1_unsigned(2), Value::m1_unsigned(9)]),
            Value::m1_unsigned(9)
        );
    }

    #[test]
    fn modulo_uses_m1_width_and_fails_on_zero() {
        assert_eq!(
            ok("Modulo", &[Value::m1_integer(7), Value::m1_integer(3)]),
            Value::m1_integer(1)
        );
        assert_eq!(
            ok("Modulo", &[Value::m1_float(7.5), Value::m1_float(2.0)]),
            Value::m1_float(1.5)
        );
        assert!(matches!(
            call("Modulo", &[Value::m1_integer(1), Value::m1_integer(0)]),
            Err(EvalError::TypeError { .. })
        ));
    }

    #[test]
    fn bias_matches_minimum_average_and_maximum_across_signs() {
        for (left, right) in [
            (10.0_f32, 20.0_f32),
            (-20.0_f32, -10.0_f32),
            (-10.0_f32, 20.0_f32),
        ] {
            let low = left.min(right);
            let high = left.max(right);
            assert_eq!(
                ok(
                    "Bias",
                    &[
                        Value::m1_float(left),
                        Value::m1_float(right),
                        Value::m1_float(-1.0),
                    ],
                ),
                Value::m1_float(low)
            );
            assert_eq!(
                ok(
                    "Bias",
                    &[
                        Value::m1_float(left),
                        Value::m1_float(right),
                        Value::m1_float(0.0),
                    ],
                ),
                Value::m1_float(left * 0.5 + right * 0.5)
            );
            assert_eq!(
                ok(
                    "Bias",
                    &[
                        Value::m1_float(left),
                        Value::m1_float(right),
                        Value::m1_float(1.0),
                    ],
                ),
                Value::m1_float(high)
            );
        }

        assert_eq!(
            ok(
                "Bias",
                &[
                    Value::m1_integer(0),
                    Value::m1_integer(0),
                    Value::m1_float(0.75),
                ],
            ),
            Value::m1_float(0.0)
        );
        // Reversing the arguments must not reverse the meaning of the bias.
        assert_eq!(
            ok(
                "Bias",
                &[
                    Value::m1_float(20.0),
                    Value::m1_float(10.0),
                    Value::m1_float(-0.5),
                ],
            ),
            Value::m1_float(12.5)
        );

        for (left, right) in [(f32::MAX, -f32::MAX), (-f32::MAX, f32::MAX)] {
            for (bias, expected) in [(-1.0, -f32::MAX), (0.0, 0.0), (1.0, f32::MAX)] {
                assert_eq!(
                    ok(
                        "Bias",
                        &[
                            Value::m1_float(left),
                            Value::m1_float(right),
                            Value::m1_float(bias),
                        ],
                    ),
                    Value::m1_float(expected),
                    "failed finite Bias anchor {bias} for {left}, {right}",
                );
            }
        }
    }

    #[test]
    fn average_returns_the_selected_overload_type() {
        assert_eq!(
            ok("Average", &[Value::m1_integer(2), Value::m1_integer(4)]),
            Value::m1_integer(3)
        );
        assert_eq!(
            ok("Average", &[Value::m1_integer(1), Value::m1_integer(2)]),
            Value::m1_integer(1)
        );
        assert_eq!(
            ok("Average", &[Value::m1_integer(-1), Value::m1_integer(-2)]),
            Value::m1_integer(-1)
        );
        assert_eq!(
            ok("Average", &[Value::m1_unsigned(1), Value::m1_unsigned(2)]),
            Value::m1_unsigned(1)
        );
        assert_eq!(
            ok("Average", &[Value::m1_float(10.0), Value::m1_float(20.0)]),
            Value::m1_float(15.0)
        );
        assert_eq!(
            ok("Average", &[Value::m1_float(1.0), Value::m1_float(2.0)]),
            Value::m1_float(1.5)
        );
        assert_eq!(
            ok(
                "Average",
                &[Value::m1_integer(i32::MAX), Value::m1_integer(i32::MAX)]
            ),
            Value::m1_integer(i32::MAX)
        );
        assert_eq!(
            ok(
                "Average",
                &[Value::m1_unsigned(u32::MAX), Value::m1_unsigned(u32::MAX),]
            ),
            Value::m1_unsigned(u32::MAX)
        );

        for value in [f32::from_bits(1), -f32::from_bits(1)] {
            assert_eq!(
                ok("Average", &[Value::m1_float(value), Value::m1_float(value)]),
                Value::m1_float(value),
                "the average of two identical subnormals must preserve the input",
            );
            assert_eq!(
                ok(
                    "Bias",
                    &[
                        Value::m1_float(value),
                        Value::m1_float(value),
                        Value::m1_float(0.0),
                    ],
                ),
                Value::m1_float(value),
                "zero Bias of identical subnormals must preserve their average",
            );
        }
    }

    #[test]
    fn constants_use_binary32() {
        assert_eq!(ok("PI", &[]), Value::m1_float(std::f32::consts::PI));
        assert_eq!(ok("Infinity", &[]), Value::m1_float(f32::INFINITY));
        assert_eq!(ok("MaximumFloat", &[]), Value::m1_float(f32::MAX));
        assert!(matches!(
            ok("NAN", &[]),
            Value::M1(M1Scalar::FloatingPoint(value)) if value.is_nan()
        ));
    }

    #[test]
    fn unary_and_power_functions_return_binary32() {
        assert_eq!(ok("Floor", &[Value::m1_float(2.7)]), Value::m1_float(2.0));
        assert_eq!(ok("Ceiling", &[Value::m1_float(2.1)]), Value::m1_float(3.0));
        assert_eq!(
            ok("Power", &[Value::m1_float(2.0), Value::m1_float(10.0)]),
            Value::m1_float(1024.0)
        );
        assert_eq!(
            ok("FastSquareRoot", &[Value::m1_float(16.0)]),
            Value::m1_float(4.0)
        );
        assert_eq!(ok("Floor", &[Value::m1_integer(5)]), Value::m1_float(5.0));
    }

    #[test]
    fn predicates_classify_binary32_values() {
        assert_eq!(ok("IsNAN", &[Value::m1_float(f32::NAN)]), Value::Bool(true));
        assert_eq!(ok("IsFinite", &[Value::m1_float(1.0)]), Value::Bool(true));
        assert_eq!(
            ok("IsFinite", &[Value::m1_float(f32::INFINITY)]),
            Value::Bool(false)
        );
    }

    #[test]
    fn trig_functions_use_binary32_results() {
        assert_eq!(
            ok("FastSin", &[Value::m1_float(0.0)]),
            Value::m1_float(0.0_f32.sin())
        );
        assert_eq!(
            ok("FastCos", &[Value::m1_float(0.0)]),
            Value::m1_float(0.0_f32.cos())
        );
        assert_eq!(
            ok("InverseTan2", &[Value::m1_float(1.0), Value::m1_float(1.0)]),
            Value::m1_float(std::f32::consts::FRAC_PI_4)
        );
        assert_eq!(
            ok("InverseSin", &[Value::m1_float(1.0)]),
            Value::m1_float(std::f32::consts::FRAC_PI_2)
        );
    }

    #[test]
    fn absolute_preserves_integral_or_float_overload() {
        assert_eq!(
            ok("Absolute", &[Value::m1_integer(-3)]),
            Value::m1_integer(3)
        );
        assert_eq!(
            ok("Absolute", &[Value::m1_unsigned(7)]),
            Value::m1_unsigned(7)
        );
        assert_eq!(
            ok("Absolute", &[Value::m1_float(-2.5)]),
            Value::m1_float(2.5)
        );
        assert_eq!(
            ok(
                "Absolute",
                &[Value::M1(M1Scalar::FixedPoint7dps(
                    FixedPoint7dps::from_raw(-12_500_000),
                ))],
            ),
            Value::m1_float(1.25)
        );
        match call("Absolute", &[Value::m1_integer(i32::MIN)]) {
            Err(EvalError::TypeError { detail }) => {
                assert!(detail.contains("outside the M1 Integer range"), "{detail}");
            }
            other => panic!("minimum M1 Integer magnitude must fail loud, got {other:?}"),
        }
    }

    #[test]
    fn rejects_legacy_numeric_arguments() {
        assert!(matches!(
            call("Average", &[Value::Int(1), Value::Int(2)]),
            Err(EvalError::TypeError { .. })
        ));
    }

    #[test]
    fn unimplemented_method_returns_none() {
        assert!(
            call("Stable", &[Value::m1_float(1.0), Value::m1_float(0.1)])
                .unwrap()
                .is_none()
        );
        assert!(call("NotAMethod", &[]).unwrap().is_none());
    }
}
