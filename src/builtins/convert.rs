// SPDX-License-Identifier: GPL-3.0-or-later
//! The pure `Convert.*` numeric-conversion builtins (Tier-1).
//!
//! - `Convert.ToInteger(x)` — truncate toward zero to a signed integer.
//! - `Convert.ToUnsignedInteger(x)` — truncate toward zero to an unsigned
//!   integer (a negative input truncates toward zero, then the magnitude is
//!   taken — documented assumption, since the intrinsic signature is unsigned).
//!
//! Truncation (toward zero), not rounding, is the documented choice for Phase 1;
//! MoTeC's exact rounding mode is to be confirmed against M1 Sim during fidelity
//! work. `Convert.ToFixed7DP` (a fixed-point type with no runtime [`Value`]
//! representation in Phase 1) is intentionally left unimplemented and falls
//! through to a fail-loud `UnsupportedBuiltin`.

use crate::error::EvalError;
use crate::value::Value;

/// Evaluate one `Convert.<method>` call. Returns `Ok(None)` for any method not
/// implemented here so the dispatcher can fall through to its fail-loud default
/// (e.g. `ToFixed7DP`). Arity is validated by the caller.
pub fn call(method: &str, args: &[Value]) -> Result<Option<Value>, EvalError> {
    let v = match method {
        "ToInteger" => Value::Int(args[0].as_f64()?.trunc() as i64),
        "ToUnsignedInteger" => {
            // Truncate toward zero; the intrinsic return type is unsigned, so a
            // negative value's magnitude is taken (documented assumption).
            let truncated = args[0].as_f64()?.trunc();
            Value::Uint(truncated.abs() as u64)
        }
        _ => return Ok(None),
    };
    Ok(Some(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(method: &str, args: &[Value]) -> Value {
        call(method, args).unwrap().unwrap()
    }

    #[test]
    fn to_integer_truncates_toward_zero() {
        assert_eq!(ok("ToInteger", &[Value::Float(2.9)]), Value::Int(2));
        assert_eq!(ok("ToInteger", &[Value::Float(-2.9)]), Value::Int(-2));
        // An already-integral value passes through.
        assert_eq!(ok("ToInteger", &[Value::Int(5)]), Value::Int(5));
    }

    #[test]
    fn to_unsigned_integer_truncates() {
        assert_eq!(ok("ToUnsignedInteger", &[Value::Float(3.7)]), Value::Uint(3));
        assert_eq!(ok("ToUnsignedInteger", &[Value::Int(9)]), Value::Uint(9));
    }

    #[test]
    fn non_numeric_input_fails_loud() {
        assert!(matches!(
            call("ToInteger", &[Value::Str("x".into())]),
            Err(EvalError::TypeError { .. })
        ));
    }

    #[test]
    fn unimplemented_method_returns_none() {
        // ToFixed7DP has no runtime Value representation; not implemented here.
        assert!(call("ToFixed7DP", &[Value::Int(1)]).unwrap().is_none());
    }
}
