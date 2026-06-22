// SPDX-License-Identifier: GPL-3.0-or-later
//! The pure `Limit.*` clamping builtins (Tier-1).
//!
//! - `Limit.Max(x, hi)` — never above `hi`.
//! - `Limit.Min(x, lo)` — never below `lo`.
//! - `Limit.Range(x, lo, hi)` — clamp into `[lo, hi]`.
//!
//! Each returns `Integer|FloatingPoint`: the result is integral when every
//! operand is integral (signed/unsigned chosen by `numeric_join`) and float
//! when any operand is float. Comparison is done in `f64`. Semantics are
//! paraphrased from our understanding of the M1 library, not copied.

use crate::error::EvalError;
use crate::value::Value;
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
    let xf = x.as_f64()?;
    let bf = bound.as_f64()?;
    let target = numeric_join(value_type(x), value_type(bound));
    let out = if floor { xf.max(bf) } else { xf.min(bf) };
    retype_numeric(out, target)
}

/// Clamp `x` into `[lo, hi]`. The result kind is the join of all three operands.
/// A reversed range (`lo > hi`) clamps to `hi` (the upper bound wins), which is
/// our documented choice for malformed ranges.
fn clamp_range(x: &Value, lo: &Value, hi: &Value) -> Result<Value, EvalError> {
    let xf = x.as_f64()?;
    let lof = lo.as_f64()?;
    let hif = hi.as_f64()?;
    let clamped = xf.max(lof).min(hif);
    let target = numeric_join(numeric_join(value_type(x), value_type(lo)), value_type(hi));
    retype_numeric(clamped, target)
}

/// Re-express a clamped `f64` under the target numeric kind so an all-integer
/// clamp stays integral.
fn retype_numeric(out: f64, target: ValueType) -> Result<Value, EvalError> {
    match target {
        ValueType::Float => Ok(Value::Float(out)),
        ValueType::Unsigned => Ok(Value::Uint(out as u64)),
        ValueType::Integer => Ok(Value::Int(out as i64)),
        _ => Err(EvalError::TypeError {
            detail: "Limit on non-numeric operands".to_string(),
        }),
    }
}

fn value_type(v: &Value) -> ValueType {
    match v {
        Value::Bool(_) => ValueType::Boolean,
        Value::Int(_) => ValueType::Integer,
        Value::Uint(_) => ValueType::Unsigned,
        Value::Float(_) => ValueType::Float,
        Value::Enum { id, .. } => ValueType::Enum(*id),
        Value::Str(_) => ValueType::String,
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
        assert_eq!(ok("Max", &[Value::Int(8), Value::Int(5)]), Value::Int(5));
        assert_eq!(ok("Max", &[Value::Int(3), Value::Int(5)]), Value::Int(3));
    }

    #[test]
    fn limit_min_floors_below() {
        assert_eq!(ok("Min", &[Value::Int(2), Value::Int(5)]), Value::Int(5));
        assert_eq!(ok("Min", &[Value::Int(8), Value::Int(5)]), Value::Int(8));
    }

    #[test]
    fn limit_range_clamps_both_ends() {
        assert_eq!(
            ok(
                "Range",
                &[Value::Float(7.0), Value::Float(0.0), Value::Float(5.0)]
            ),
            Value::Float(5.0)
        );
        assert_eq!(
            ok(
                "Range",
                &[Value::Float(-1.0), Value::Float(0.0), Value::Float(5.0)]
            ),
            Value::Float(0.0)
        );
        assert_eq!(
            ok(
                "Range",
                &[Value::Float(3.0), Value::Float(0.0), Value::Float(5.0)]
            ),
            Value::Float(3.0)
        );
        // All-integer range stays integral.
        assert_eq!(
            ok("Range", &[Value::Int(9), Value::Int(0), Value::Int(5)]),
            Value::Int(5)
        );
    }

    #[test]
    fn float_operand_promotes() {
        assert_eq!(
            ok("Max", &[Value::Int(8), Value::Float(5.0)]),
            Value::Float(5.0)
        );
    }

    #[test]
    fn unimplemented_method_returns_none() {
        assert!(call("Nope", &[]).unwrap().is_none());
    }
}
