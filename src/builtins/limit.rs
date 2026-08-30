// SPDX-License-Identifier: GPL-3.0-or-later
//! The pure `Limit.*` clamping builtins (Tier-1).
//!
//! - `Limit.Max(x, hi)` — never above `hi`.
//! - `Limit.Min(x, lo)` — never below `lo`.
//! - `Limit.Range(x, lo, hi)` — clamp into `[lo, hi]`.
//!
//! Each returns `Integer|FloatingPoint`: the result is integral when every
//! operand is integral (signed/unsigned chosen by `numeric_join`) and binary32
//! when any operand is floating. Comparisons and results stay in the selected M1
//! family. Semantics are paraphrased from our understanding of the M1 library.

use crate::error::EvalError;
use crate::value::{M1Scalar, Value};
use m1_typecheck::types::{ValueType, numeric_join};

/// Evaluate one `Limit.<method>` call. Returns `Ok(None)` for any method not
/// implemented here so the dispatcher can fall through to its fail-loud default.
/// Arity is validated by the caller against the intrinsic library.
pub fn call(method: &str, args: &[Value]) -> Result<Option<Value>, EvalError> {
    let v = match method {
        // Limit.Max caps x at an upper bound: min(x, hi).
        "Max" => clamp_one(&args[0], &args[1], false)?,
        // Limit.Min floors x at a lower bound: max(x, lo).
        "Min" => clamp_one(&args[0], &args[1], true)?,
        "Range" => clamp_range(&args[0], &args[1], &args[2])?,
        _ => return Ok(None),
    };
    Ok(Some(v))
}

/// Clamp `x` against a single bound. `floor` true ⇒ raise `x` up to at least
/// `bound` (a lower bound); false ⇒ cap `x` down to at most `bound` (an upper
/// bound). The result is retyped under the join of `x` and `bound`.
fn clamp_one(x: &Value, bound: &Value, floor: bool) -> Result<Value, EvalError> {
    match numeric_join(value_type(x), value_type(bound)) {
        ValueType::Float => {
            let x = x.m1_scalar()?.as_f32();
            let bound = bound.m1_scalar()?.as_f32();
            Ok(Value::m1_float(if floor {
                x.max(bound)
            } else {
                x.min(bound)
            }))
        }
        ValueType::Unsigned => {
            let x = as_u32_bits(x)?;
            let bound = as_u32_bits(bound)?;
            Ok(Value::m1_unsigned(if floor {
                x.max(bound)
            } else {
                x.min(bound)
            }))
        }
        ValueType::Integer => {
            let x = as_i32(x)?;
            let bound = as_i32(bound)?;
            Ok(Value::m1_integer(if floor {
                x.max(bound)
            } else {
                x.min(bound)
            }))
        }
        _ => Err(non_numeric()),
    }
}

/// Clamp `x` into `[lo, hi]`. The result kind is the join of all three operands.
/// A reversed range (`lo > hi`) clamps to `hi` (the upper bound wins), which is
/// our documented choice for malformed ranges.
fn clamp_range(x: &Value, lo: &Value, hi: &Value) -> Result<Value, EvalError> {
    let target = numeric_join(numeric_join(value_type(x), value_type(lo)), value_type(hi));
    match target {
        ValueType::Float => Ok(Value::m1_float(
            x.m1_scalar()?
                .as_f32()
                .max(lo.m1_scalar()?.as_f32())
                .min(hi.m1_scalar()?.as_f32()),
        )),
        ValueType::Unsigned => Ok(Value::m1_unsigned(
            as_u32_bits(x)?.max(as_u32_bits(lo)?).min(as_u32_bits(hi)?),
        )),
        ValueType::Integer => Ok(Value::m1_integer(
            as_i32(x)?.max(as_i32(lo)?).min(as_i32(hi)?),
        )),
        _ => Err(non_numeric()),
    }
}

fn value_type(v: &Value) -> ValueType {
    match v {
        Value::Bool(_) => ValueType::Boolean,
        Value::M1(M1Scalar::Integer(_)) => ValueType::Integer,
        Value::M1(M1Scalar::UnsignedInteger(_)) => ValueType::Unsigned,
        Value::M1(M1Scalar::FloatingPoint(_) | M1Scalar::FixedPoint7dps(_)) => ValueType::Float,
        Value::Enum { id, .. } => ValueType::Enum(*id),
        Value::Str(_) => ValueType::String,
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

fn non_numeric() -> EvalError {
    EvalError::TypeError {
        detail: "Limit on non-numeric operands".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(method: &str, args: &[Value]) -> Value {
        call(method, args).unwrap().unwrap()
    }

    #[test]
    fn limit_max_caps_above() {
        assert_eq!(
            ok("Max", &[Value::m1_integer(8), Value::m1_integer(5)]),
            Value::m1_integer(5)
        );
        assert_eq!(
            ok("Max", &[Value::m1_integer(3), Value::m1_integer(5)]),
            Value::m1_integer(3)
        );
    }

    #[test]
    fn limit_min_floors_below() {
        assert_eq!(
            ok("Min", &[Value::m1_integer(2), Value::m1_integer(5)]),
            Value::m1_integer(5)
        );
        assert_eq!(
            ok("Min", &[Value::m1_integer(8), Value::m1_integer(5)]),
            Value::m1_integer(8)
        );
    }

    #[test]
    fn limit_range_clamps_both_ends() {
        assert_eq!(
            ok(
                "Range",
                &[
                    Value::m1_float(7.0),
                    Value::m1_float(0.0),
                    Value::m1_float(5.0)
                ]
            ),
            Value::m1_float(5.0)
        );
        assert_eq!(
            ok(
                "Range",
                &[
                    Value::m1_float(-1.0),
                    Value::m1_float(0.0),
                    Value::m1_float(5.0)
                ]
            ),
            Value::m1_float(0.0)
        );
        assert_eq!(
            ok(
                "Range",
                &[
                    Value::m1_float(3.0),
                    Value::m1_float(0.0),
                    Value::m1_float(5.0)
                ]
            ),
            Value::m1_float(3.0)
        );
        // All-integer range stays integral.
        assert_eq!(
            ok(
                "Range",
                &[
                    Value::m1_integer(9),
                    Value::m1_integer(0),
                    Value::m1_integer(5)
                ]
            ),
            Value::m1_integer(5)
        );
    }

    #[test]
    fn float_operand_promotes() {
        assert_eq!(
            ok("Max", &[Value::m1_integer(8), Value::m1_float(5.0)]),
            Value::m1_float(5.0)
        );
    }

    #[test]
    fn unimplemented_method_returns_none() {
        assert!(call("Nope", &[]).unwrap().is_none());
    }
}
