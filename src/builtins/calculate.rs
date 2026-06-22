// SPDX-License-Identifier: GPL-3.0-or-later
//! The pure `Calculate.*` math builtins (Tier-1).
//!
//! These are deterministic, side-effect-free, time-independent functions: max,
//! min, modulo, bias blend, the constant PI, floor/ceiling, power, square root,
//! NaN test, and the fast trig approximations. Their semantics are paraphrased
//! from our understanding of the M1 library (never copied from the proprietary
//! manuals).
//!
//! ## Numeric result typing
//!
//! Where the intrinsic signature returns `Integer|FloatingPoint`
//! (`Max`/`Min`/`Modulo`), the result kind follows the operands: integral when
//! every operand is integral (preserving signed/unsigned via `numeric_join`),
//! float when any operand is float. The float-only signatures
//! (`Floor`/`Ceiling`/`Power`/`FastSquareRoot`/trig/`Bias`) always return a
//! [`Value::Float`].
//!
//! Stateful `Calculate` methods (`Stable`, `Hysteresis`, `Between`, `Beyond`)
//! are flagged stateful in the intrinsic library and belong to the stateful
//! milestone (M6); they are deliberately *not* implemented here and therefore
//! fall through `dispatch` to a fail-loud `UnsupportedBuiltin`.

use crate::error::EvalError;
use crate::value::Value;
use m1_typecheck::types::{ValueType, numeric_join};

/// Evaluate one `Calculate.<method>` call. Returns `Ok(None)` when `method` is
/// not one of the pure functions implemented here, so the dispatcher can fall
/// through to its fail-loud default (the stateful `Calculate` methods land
/// there until M6). Arity is validated by the caller against the intrinsic
/// library before this runs.
pub fn call(method: &str, args: &[Value]) -> Result<Option<Value>, EvalError> {
    let v = match method {
        "Max" => binary_minmax(args, true)?,
        "Min" => binary_minmax(args, false)?,
        "Absolute" => abs(args)?,
        "Average" => average(args)?,
        "Modulo" => modulo(args)?,
        "Bias" => bias(args)?,
        "PI" => Value::Float(std::f64::consts::PI),
        "NAN" => Value::Float(f64::NAN),
        "Infinity" => Value::Float(f64::INFINITY),
        // The largest representable finite float (paraphrased: the maximum finite
        // floating-point magnitude the firmware can hold).
        "MaximumFloat" => Value::Float(f64::MAX),
        "Floor" => Value::Float(unary_f64(args)?.floor()),
        "Ceiling" => Value::Float(unary_f64(args)?.ceil()),
        "Power" => {
            let (base, exp) = two_f64(args)?;
            Value::Float(base.powf(exp))
        }
        "FastSquareRoot" => Value::Float(unary_f64(args)?.sqrt()),
        "IsNAN" => Value::Bool(unary_f64(args)?.is_nan()),
        "IsFinite" => Value::Bool(unary_f64(args)?.is_finite()),
        "FastSin" => Value::Float(unary_f64(args)?.sin()),
        "FastCos" => Value::Float(unary_f64(args)?.cos()),
        "FastTan" => Value::Float(unary_f64(args)?.tan()),
        // The inverse trig functions mirror the `FastSin` paraphrase note: the std
        // implementation, a documented assumption (radians, principal value).
        "InverseSin" => Value::Float(unary_f64(args)?.asin()),
        "InverseCos" => Value::Float(unary_f64(args)?.acos()),
        "InverseTan" => Value::Float(unary_f64(args)?.atan()),
        "InverseTan2" => {
            let (y, x) = two_f64(args)?;
            Value::Float(y.atan2(x))
        }
        // Not a pure Calculate function we implement: let the dispatcher decide
        // (stateful ones -> M6 -> UnsupportedBuiltin; unknown names likewise).
        _ => return Ok(None),
    };
    Ok(Some(v))
}

/// `Max`/`Min` of two numbers. The result preserves integrality: if both
/// operands are integral the result is integral (signed/unsigned chosen by
/// `numeric_join`), otherwise it is a float. Comparison itself is done in `f64`
/// so mixed int/float operands compare correctly.
fn binary_minmax(args: &[Value], want_max: bool) -> Result<Value, EvalError> {
    let (a, b) = (&args[0], &args[1]);
    let af = a.as_f64()?;
    let bf = b.as_f64()?;
    let pick_a = if want_max { af >= bf } else { af <= bf };
    let chosen = if pick_a { a } else { b };
    // Re-type the chosen operand under the join so e.g. max(Int, Uint) is Uint.
    retype_numeric(chosen, numeric_join(value_type(a), value_type(b)))
}

/// `Modulo(a, b)` — the remainder of `a / b`. Integral when both operands are
/// integral (chosen by `numeric_join`), float otherwise. A zero divisor fails
/// loud rather than producing NaN.
fn modulo(args: &[Value]) -> Result<Value, EvalError> {
    let (a, b) = (&args[0], &args[1]);
    let joined = numeric_join(value_type(a), value_type(b));
    match joined {
        ValueType::Float => {
            let bf = b.as_f64()?;
            if bf == 0.0 {
                return Err(modulo_by_zero());
            }
            Ok(Value::Float(a.as_f64()? % bf))
        }
        ValueType::Unsigned => {
            let bu = as_u64(b)?;
            if bu == 0 {
                return Err(modulo_by_zero());
            }
            Ok(Value::Uint(as_u64(a)? % bu))
        }
        ValueType::Integer => {
            let bi = as_i64(b)?;
            if bi == 0 {
                return Err(modulo_by_zero());
            }
            Ok(Value::Int(as_i64(a)? % bi))
        }
        _ => Err(EvalError::TypeError {
            detail: format!("Calculate.Modulo on non-numeric operands {a:?}, {b:?}"),
        }),
    }
}

/// `Bias(a, b, t)` — a linear blend between `a` (at `t=0`) and `b` (at `t=1`):
/// `a + (b - a) * t`. The blended result is always a float (the blend factor is
/// fractional). This is our paraphrased reading of the bias/cross-fade helper.
fn bias(args: &[Value]) -> Result<Value, EvalError> {
    let a = args[0].as_f64()?;
    let b = args[1].as_f64()?;
    let t = args[2].as_f64()?;
    Ok(Value::Float(a + (b - a) * t))
}

/// `Absolute(x)` — the magnitude of `x`, preserving integrality the same way
/// `Max`/`Min` do (the intrinsic signature is `Integer|FloatingPoint`). An
/// unsigned operand is already non-negative, so it is returned unchanged; a
/// signed integer or float is negated when below zero. Re-typing under the
/// operand's own [`ValueType`] keeps `Int`→`Int`, `Uint`→`Uint`, `Float`→`Float`.
fn abs(args: &[Value]) -> Result<Value, EvalError> {
    match &args[0] {
        // An unsigned operand is already non-negative.
        Value::Uint(x) => Ok(Value::Uint(*x)),
        Value::Int(x) => Ok(Value::Int(x.abs())),
        Value::Float(x) => Ok(Value::Float(x.abs())),
        other => Err(EvalError::TypeError {
            detail: format!("Calculate.Absolute on non-numeric operand {other:?}"),
        }),
    }
}

/// `Average(a, b)` — the arithmetic mean `(a + b) / 2`. Always a [`Value::Float`]:
/// the mean of two integers is generally fractional, so we never round it back to
/// an integer (the intrinsic return tag is `Integer|FloatingPoint`, but a faithful
/// mean keeps the fractional part — a documented divergence from the raw tag).
fn average(args: &[Value]) -> Result<Value, EvalError> {
    let a = args[0].as_f64()?;
    let b = args[1].as_f64()?;
    Ok(Value::Float((a + b) / 2.0))
}

/// Coerce a single argument to `f64` for the float-only functions.
fn unary_f64(args: &[Value]) -> Result<f64, EvalError> {
    args[0].as_f64()
}

/// Coerce two arguments to `f64` for the two-arg float functions.
fn two_f64(args: &[Value]) -> Result<(f64, f64), EvalError> {
    Ok((args[0].as_f64()?, args[1].as_f64()?))
}

/// Re-express a value under a target numeric [`ValueType`]. Used so `Max`/`Min`
/// hand back the joined kind (e.g. an `Int` operand chosen in a join that landed
/// on `Float` is returned as a `Float`).
fn retype_numeric(v: &Value, target: ValueType) -> Result<Value, EvalError> {
    match target {
        ValueType::Float => Ok(Value::Float(v.as_f64()?)),
        ValueType::Unsigned => Ok(Value::Uint(as_u64(v)?)),
        ValueType::Integer => Ok(Value::Int(as_i64(v)?)),
        _ => Err(EvalError::TypeError {
            detail: format!("non-numeric operand {v:?} in Calculate min/max"),
        }),
    }
}

/// The [`ValueType`] of a runtime value, for `numeric_join`-driven result typing.
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

fn as_i64(v: &Value) -> Result<i64, EvalError> {
    match v {
        Value::Int(x) => Ok(*x),
        Value::Uint(x) => Ok(*x as i64),
        other => Err(EvalError::TypeError {
            detail: format!("{other:?} is not an integer"),
        }),
    }
}

fn as_u64(v: &Value) -> Result<u64, EvalError> {
    match v {
        Value::Uint(x) => Ok(*x),
        Value::Int(x) => Ok(*x as u64),
        other => Err(EvalError::TypeError {
            detail: format!("{other:?} is not an unsigned integer"),
        }),
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

    fn ok(method: &str, args: &[Value]) -> Value {
        call(method, args).unwrap().unwrap()
    }

    #[test]
    fn max_and_min_preserve_integrality() {
        assert_eq!(ok("Max", &[Value::Int(2), Value::Int(3)]), Value::Int(3));
        assert_eq!(ok("Min", &[Value::Int(2), Value::Int(3)]), Value::Int(2));
        // A float operand promotes the result.
        assert_eq!(
            ok("Max", &[Value::Int(2), Value::Float(3.5)]),
            Value::Float(3.5)
        );
        // The smaller wins for Min, retyped to the join (float here).
        assert_eq!(
            ok("Min", &[Value::Int(2), Value::Float(3.5)]),
            Value::Float(2.0)
        );
        // Unsigned join stays unsigned.
        assert_eq!(ok("Max", &[Value::Uint(2), Value::Uint(9)]), Value::Uint(9));
    }

    #[test]
    fn modulo_int_float_and_zero() {
        assert_eq!(ok("Modulo", &[Value::Int(7), Value::Int(3)]), Value::Int(1));
        assert_eq!(
            ok("Modulo", &[Value::Float(7.5), Value::Float(2.0)]),
            Value::Float(1.5)
        );
        assert!(matches!(
            call("Modulo", &[Value::Int(1), Value::Int(0)]),
            Err(EvalError::TypeError { .. })
        ));
    }

    #[test]
    fn bias_blends_linearly() {
        // t=0 -> a, t=1 -> b, t=0.5 -> midpoint.
        assert_eq!(
            ok(
                "Bias",
                &[Value::Float(10.0), Value::Float(20.0), Value::Float(0.0)]
            ),
            Value::Float(10.0)
        );
        assert_eq!(
            ok(
                "Bias",
                &[Value::Float(10.0), Value::Float(20.0), Value::Float(1.0)]
            ),
            Value::Float(20.0)
        );
        assert_eq!(
            ok(
                "Bias",
                &[Value::Float(10.0), Value::Float(20.0), Value::Float(0.25)]
            ),
            Value::Float(12.5)
        );
    }

    #[test]
    fn pi_is_the_constant() {
        assert_eq!(ok("PI", &[]), Value::Float(std::f64::consts::PI));
    }

    #[test]
    fn floor_ceiling_power_sqrt() {
        assert_eq!(ok("Floor", &[Value::Float(2.7)]), Value::Float(2.0));
        assert_eq!(ok("Ceiling", &[Value::Float(2.1)]), Value::Float(3.0));
        assert_eq!(
            ok("Power", &[Value::Float(2.0), Value::Float(10.0)]),
            Value::Float(1024.0)
        );
        assert_eq!(
            ok("FastSquareRoot", &[Value::Float(16.0)]),
            Value::Float(4.0)
        );
        // Integral input coerces to f64 for the float-only functions.
        assert_eq!(ok("Floor", &[Value::Int(5)]), Value::Float(5.0));
    }

    #[test]
    fn is_nan_detects_nan() {
        assert_eq!(ok("IsNAN", &[Value::Float(f64::NAN)]), Value::Bool(true));
        assert_eq!(ok("IsNAN", &[Value::Float(1.0)]), Value::Bool(false));
    }

    #[test]
    fn fast_trig_matches_std() {
        // Our fast trig is the std implementation in Phase 1 (documented
        // assumption); exact-match the std result.
        assert_eq!(
            ok("FastSin", &[Value::Float(0.0)]),
            Value::Float(0.0_f64.sin())
        );
        assert_eq!(
            ok("FastCos", &[Value::Float(0.0)]),
            Value::Float(0.0_f64.cos())
        );
        assert_eq!(
            ok("FastTan", &[Value::Float(0.0)]),
            Value::Float(0.0_f64.tan())
        );
        // atan2(1, 1) = pi/4.
        assert_eq!(
            ok("InverseTan2", &[Value::Float(1.0), Value::Float(1.0)]),
            Value::Float(std::f64::consts::FRAC_PI_4)
        );
    }

    #[test]
    fn absolute_preserves_integrality() {
        // Signed integer magnitude stays an Int.
        assert_eq!(ok("Absolute", &[Value::Int(-3)]), Value::Int(3));
        assert_eq!(ok("Absolute", &[Value::Int(3)]), Value::Int(3));
        // Float magnitude stays a Float.
        assert_eq!(ok("Absolute", &[Value::Float(-2.5)]), Value::Float(2.5));
        // Unsigned is already non-negative and stays a Uint.
        assert_eq!(ok("Absolute", &[Value::Uint(7)]), Value::Uint(7));
    }

    #[test]
    fn average_is_the_mean_and_always_float() {
        // (a + b) / 2; fractional even for two integers.
        assert_eq!(
            ok("Average", &[Value::Int(2), Value::Int(4)]),
            Value::Float(3.0)
        );
        assert_eq!(
            ok("Average", &[Value::Int(1), Value::Int(2)]),
            Value::Float(1.5)
        );
        assert_eq!(
            ok("Average", &[Value::Float(10.0), Value::Float(20.0)]),
            Value::Float(15.0)
        );
    }

    #[test]
    fn nan_and_infinity_constants() {
        // NAN() -> a Float that reports as NaN.
        match ok("NAN", &[]) {
            Value::Float(x) => assert!(x.is_nan()),
            other => panic!("expected NaN Float, got {other:?}"),
        }
        assert_eq!(ok("Infinity", &[]), Value::Float(f64::INFINITY));
    }

    #[test]
    fn is_finite_classifies() {
        assert_eq!(ok("IsFinite", &[Value::Float(1.0)]), Value::Bool(true));
        assert_eq!(
            ok("IsFinite", &[Value::Float(f64::INFINITY)]),
            Value::Bool(false)
        );
        assert_eq!(
            ok("IsFinite", &[Value::Float(f64::NAN)]),
            Value::Bool(false)
        );
    }

    #[test]
    fn maximum_float_is_the_largest_finite() {
        assert_eq!(ok("MaximumFloat", &[]), Value::Float(f64::MAX));
    }

    #[test]
    fn inverse_trig_matches_std() {
        // Principal values in radians (std implementation, documented assumption).
        assert_eq!(
            ok("InverseSin", &[Value::Float(0.0)]),
            Value::Float(0.0_f64.asin())
        );
        assert_eq!(
            ok("InverseCos", &[Value::Float(1.0)]),
            Value::Float(1.0_f64.acos())
        );
        assert_eq!(
            ok("InverseTan", &[Value::Float(0.0)]),
            Value::Float(0.0_f64.atan())
        );
        // asin(1) = pi/2.
        assert_eq!(
            ok("InverseSin", &[Value::Float(1.0)]),
            Value::Float(std::f64::consts::FRAC_PI_2)
        );
    }

    #[test]
    fn unimplemented_method_returns_none() {
        // Stateful Calculate methods are not implemented here -> None, so the
        // dispatcher fails loud (UnsupportedBuiltin) until M6.
        assert!(
            call("Stable", &[Value::Float(1.0), Value::Float(0.1)])
                .unwrap()
                .is_none()
        );
        assert!(call("Hysteresis", &[]).unwrap().is_none());
        assert!(call("NotAMethod", &[]).unwrap().is_none());
    }
}
