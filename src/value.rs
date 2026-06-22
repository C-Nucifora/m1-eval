// SPDX-License-Identifier: GPL-3.0-or-later
//! Runtime value type and strict coercions.
//!
//! M1 is strongly typed: there is no implicit `int -> bool` or `bool -> int`
//! coercion. The numeric coercions here (`Int`/`Uint`/`Float -> f64`) exist
//! only to drive arithmetic and table interpolation, which operate on `f64`
//! internally. Anything non-numeric (`Bool`, `Enum`, `Str`) is an explicit
//! `EvalError::TypeError` rather than a silent fallback — the evaluator never
//! substitutes a guessed numeric value.

use crate::error::EvalError;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Enum { id: usize, member: String },
    Str(String),
}

impl Value {
    /// Coerce a numeric value to `f64`. Non-numeric values are a `TypeError`;
    /// we never invent a default number.
    pub fn as_f64(&self) -> Result<f64, EvalError> {
        match self {
            Value::Float(x) => Ok(*x),
            Value::Int(x) => Ok(*x as f64),
            Value::Uint(x) => Ok(*x as f64),
            other => Err(EvalError::TypeError {
                detail: format!("{other:?} is not numeric"),
            }),
        }
    }

    /// Extract a boolean. M1 has no truthiness on numbers, so only `Bool`
    /// succeeds; everything else is a `TypeError`.
    pub fn as_bool(&self) -> Result<bool, EvalError> {
        match self {
            Value::Bool(b) => Ok(*b),
            other => Err(EvalError::TypeError {
                detail: format!("{other:?} is not boolean"),
            }),
        }
    }

    /// Truthiness for conditions/logical operators. In M1 this is strictly a
    /// boolean test (no implicit numeric-to-bool), so it forwards to `as_bool`.
    pub fn truthy(&self) -> Result<bool, EvalError> {
        self.as_bool()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_and_int_coerce_to_f64() {
        assert_eq!(Value::Float(2.5).as_f64().unwrap(), 2.5);
        assert_eq!(Value::Int(-3).as_f64().unwrap(), -3.0);
        assert_eq!(Value::Uint(7).as_f64().unwrap(), 7.0);
        assert!(Value::Str("x".into()).as_f64().is_err());
    }

    #[test]
    fn enum_is_not_numeric() {
        let v = Value::Enum {
            id: 1,
            member: "On".into(),
        };
        assert!(v.as_f64().is_err());
    }

    #[test]
    fn bool_coercion() {
        assert!(Value::Bool(true).as_bool().unwrap());
        assert!(!Value::Bool(false).as_bool().unwrap());
        // M1 is strongly typed: no int->bool.
        assert!(Value::Int(1).as_bool().is_err());
        assert!(Value::Float(0.0).as_bool().is_err());
    }

    #[test]
    fn truthy_forwards_to_as_bool() {
        assert!(Value::Bool(true).truthy().unwrap());
        assert!(!Value::Bool(false).truthy().unwrap());
        assert!(Value::Uint(0).truthy().is_err());
    }

    #[test]
    fn as_f64_error_is_type_error() {
        match Value::Str("nope".into()).as_f64() {
            Err(EvalError::TypeError { .. }) => {}
            other => panic!("expected TypeError, got {other:?}"),
        }
    }
}
