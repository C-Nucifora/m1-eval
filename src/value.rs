// SPDX-License-Identifier: GPL-3.0-or-later
//! Runtime value types and strict coercions.
//!
//! M1 is strongly typed: there is no implicit `int -> bool` or `bool -> int`
//! coercion. [`M1Scalar`] is the evaluator's only numeric runtime form. Its four
//! variants match the widths and signedness used by M1. Host-width numbers may
//! appear while parsing wire formats, measuring time, interpolating tables, or
//! rendering reports, but they never enter script execution as [`Value`]s.

use crate::error::EvalError;
use m1_typecheck::Project;

/// The signed, 32-bit storage used by M1's seven-decimal-place fixed-point
/// scalar. A raw value of `1` represents `0.0000001`.
///
/// This type only models storage and exact scale conversion. Language-level
/// rounding and range validation belong to the conversion builtin, not this
/// type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FixedPoint7dps(i32);

impl FixedPoint7dps {
    pub const SCALE: i64 = 10_000_000;
    pub const MIN: Self = Self(i32::MIN);
    pub const MAX: Self = Self(i32::MAX);
    pub const ZERO: Self = Self(0);

    /// Construct a value from its signed, scaled storage representation.
    pub const fn from_raw(raw: i32) -> Self {
        Self(raw)
    }

    /// Return the signed, scaled storage representation.
    pub const fn raw(self) -> i32 {
        self.0
    }

    /// Widen exactly for table interpolation, timing comparisons, or reporting.
    pub fn as_f64(self) -> f64 {
        f64::from(self.0) / Self::SCALE as f64
    }

    /// Parse an exact decimal or scientific-notation value into scaled storage.
    ///
    /// The parser uses integer decimal arithmetic. It rejects values with a
    /// non-zero digit beyond seven decimal places and values outside the signed
    /// 32-bit raw range, rather than rounding through a host float.
    pub fn parse_decimal(text: &str) -> Option<Self> {
        let text = text.trim();
        let (negative, unsigned) = match text.as_bytes().first() {
            Some(b'-') => (true, &text[1..]),
            Some(b'+') => (false, &text[1..]),
            _ => (false, text),
        };
        if unsigned.is_empty() {
            return None;
        }

        let mut exponent_split = unsigned.split(['e', 'E']);
        let mantissa = exponent_split.next()?;
        let exponent = match exponent_split.next() {
            Some(value) if !value.is_empty() => value.parse::<i32>().ok()?,
            Some(_) => return None,
            None => 0,
        };
        if exponent_split.next().is_some() {
            return None;
        }

        let mut decimal_split = mantissa.split('.');
        let whole = decimal_split.next()?;
        let fractional = decimal_split.next().unwrap_or("");
        if decimal_split.next().is_some()
            || (whole.is_empty() && fractional.is_empty())
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || !fractional.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }

        let mut digits = format!("{whole}{fractional}");
        let decimal_shift = exponent
            .checked_add(7)
            .and_then(|value| value.checked_sub(i32::try_from(fractional.len()).ok()?))?;

        if digits.bytes().all(|byte| byte == b'0') {
            return Some(Self::ZERO);
        }
        if decimal_shift < 0 {
            let discarded = usize::try_from(decimal_shift.unsigned_abs()).ok()?;
            let kept = digits.len().checked_sub(discarded)?;
            if !digits[kept..].bytes().all(|byte| byte == b'0') {
                return None;
            }
            digits.truncate(kept);
        }

        let significant = digits.trim_start_matches('0');
        let coefficient = significant.parse::<u128>().ok()?;
        let magnitude = if decimal_shift > 0 {
            coefficient.checked_mul(10_u128.checked_pow(decimal_shift as u32)?)?
        } else {
            coefficient
        };

        let signed = i128::try_from(magnitude).ok()?;
        let signed = if negative { -signed } else { signed };
        i32::try_from(signed).ok().map(Self::from_raw)
    }
}

impl std::fmt::Display for FixedPoint7dps {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let raw = i64::from(self.0);
        let negative = raw < 0;
        let magnitude = raw.abs();
        let integer = magnitude / Self::SCALE;
        let fractional = magnitude % Self::SCALE;

        if negative {
            formatter.write_str("-")?;
        }
        write!(formatter, "{integer}")?;
        if fractional != 0 {
            let digits = format!("{fractional:07}");
            write!(formatter, ".{}", digits.trim_end_matches('0'))?;
        }
        Ok(())
    }
}

/// Numeric values with the widths used by the M1 runtime.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum M1Scalar {
    /// IEEE-754 binary32.
    FloatingPoint(f32),
    /// Signed 32-bit integer.
    Integer(i32),
    /// Unsigned 32-bit integer.
    UnsignedInteger(u32),
    /// Signed 32-bit integer scaled by 10^-7.
    FixedPoint7dps(FixedPoint7dps),
}

/// Identifies one of the four numeric scalar types used by M1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum M1ScalarKind {
    FloatingPoint,
    Integer,
    UnsignedInteger,
    FixedPoint7dps,
}

impl M1Scalar {
    /// Return this scalar's M1 type.
    pub const fn kind(self) -> M1ScalarKind {
        match self {
            Self::FloatingPoint(_) => M1ScalarKind::FloatingPoint,
            Self::Integer(_) => M1ScalarKind::Integer,
            Self::UnsignedInteger(_) => M1ScalarKind::UnsignedInteger,
            Self::FixedPoint7dps(_) => M1ScalarKind::FixedPoint7dps,
        }
    }

    /// Widen for exact interpolation, timing comparisons, or reporting.
    pub fn as_f64(self) -> f64 {
        match self {
            Self::FloatingPoint(value) => f64::from(value),
            Self::Integer(value) => f64::from(value),
            Self::UnsignedInteger(value) => f64::from(value),
            Self::FixedPoint7dps(value) => value.as_f64(),
        }
    }

    /// Convert to the binary32 value used by M1 floating-point expressions.
    ///
    /// Integer and fixed-point inputs are rounded to binary32 here, before an
    /// operation is performed. This prevents an expression from accidentally
    /// gaining host `f64` precision between M1 operations.
    pub fn as_f32(self) -> f32 {
        match self {
            Self::FloatingPoint(value) => value,
            Self::Integer(value) => value as f32,
            Self::UnsignedInteger(value) => value as f32,
            Self::FixedPoint7dps(value) => value.as_f64() as f32,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    /// A numeric value represented with its M1 width and signedness.
    M1(M1Scalar),
    Enum {
        id: usize,
        member: String,
    },
    Str(String),
}

impl Value {
    /// Construct an M1 binary32 value.
    pub const fn m1_float(value: f32) -> Self {
        Self::M1(M1Scalar::FloatingPoint(value))
    }

    /// Construct an M1 signed 32-bit integer value.
    pub const fn m1_integer(value: i32) -> Self {
        Self::M1(M1Scalar::Integer(value))
    }

    /// Construct an M1 unsigned 32-bit integer value.
    pub const fn m1_unsigned(value: u32) -> Self {
        Self::M1(M1Scalar::UnsignedInteger(value))
    }

    /// Return the M1 scalar held by this value.
    pub fn m1_scalar(&self) -> Result<M1Scalar, EvalError> {
        match self {
            Self::M1(value) => Ok(*value),
            other => Err(EvalError::TypeError {
                detail: format!("{other:?} is not numeric"),
            }),
        }
    }

    /// Whether this value is numeric.
    pub const fn is_numeric(&self) -> bool {
        matches!(self, Value::M1(_))
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

    /// Convert an enum value to its declared integer (`.AsInteger`).
    ///
    /// For a [`Value::Enum`], look the held `member` up in the value's enum type
    /// (`project.symbols().enum_type(id).members`) and return its declared `i64`
    /// — the `ContainerOrder` for project-local enums, the documented
    /// `EnumMember.value` for builtin/firmware enums. A non-enum value, or an
    /// enum value whose `member` is not declared on its type, is a fail-loud
    /// [`EvalError::TypeError`] (the evaluator never guesses an integer).
    pub fn as_enum_int(&self, project: &Project) -> Result<i64, EvalError> {
        let Value::Enum { id, member } = self else {
            return Err(EvalError::TypeError {
                detail: format!("{self:?} is not an enum value (no .AsInteger)"),
            });
        };
        let enum_type = project.symbols().enum_type(*id);
        enum_type
            .members
            .iter()
            .find(|(name, _)| name == member)
            .map(|(_, value)| *value)
            .ok_or_else(|| EvalError::TypeError {
                detail: format!(
                    "enum member {member:?} is not a member of enum {:?}",
                    enum_type.name
                ),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Load the synthetic enums fixture project (project-local `Drive State`
    /// enum with members `Idle=0`, `Precharging=2`).
    fn enums_project() -> Project {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/enums");
        crate::loader::load(&dir.join("Project.m1prj"), None)
            .expect("enums fixture loads")
            .project
    }

    #[test]
    fn as_enum_int_returns_container_order_not_ordinal() {
        let project = enums_project();
        let id = project
            .symbols()
            .enum_by_name("Drive State")
            .expect("Drive State enum present");
        // Precharging has ContainerOrder=2 but is the *second* (ordinal index 1)
        // member — so a return of 2 proves the declared value, not the index.
        let v = Value::Enum {
            id,
            member: "Precharging".to_string(),
        };
        assert_eq!(v.as_enum_int(&project).unwrap(), 2);
        // And Idle is 0.
        let v = Value::Enum {
            id,
            member: "Idle".to_string(),
        };
        assert_eq!(v.as_enum_int(&project).unwrap(), 0);
    }

    #[test]
    fn as_enum_int_unknown_member_fails_loud() {
        let project = enums_project();
        let id = project.symbols().enum_by_name("Drive State").unwrap();
        let v = Value::Enum {
            id,
            member: "Nope".to_string(),
        };
        match v.as_enum_int(&project) {
            Err(EvalError::TypeError { .. }) => {}
            other => panic!("expected TypeError for unknown member, got {other:?}"),
        }
    }

    #[test]
    fn as_enum_int_on_non_enum_fails_loud() {
        let project = enums_project();
        match Value::m1_integer(3).as_enum_int(&project) {
            Err(EvalError::TypeError { .. }) => {}
            other => panic!("expected TypeError on non-enum value, got {other:?}"),
        }
    }

    #[test]
    fn every_m1_scalar_family_keeps_its_runtime_width() {
        let scalars = [
            M1Scalar::FloatingPoint(-0.0),
            M1Scalar::FloatingPoint(f32::from_bits(1)),
            M1Scalar::FloatingPoint(f32::MIN),
            M1Scalar::FloatingPoint(f32::MAX),
            M1Scalar::Integer(i32::MIN),
            M1Scalar::Integer(i32::MAX),
            M1Scalar::UnsignedInteger(0),
            M1Scalar::UnsignedInteger(u32::MAX),
            M1Scalar::FixedPoint7dps(FixedPoint7dps::MIN),
            M1Scalar::FixedPoint7dps(FixedPoint7dps::MAX),
        ];

        for scalar in scalars {
            let value = Value::M1(scalar);
            assert_eq!(value.m1_scalar().unwrap(), scalar);
            assert!(value.is_numeric());
        }
    }

    #[test]
    fn fixed_point_decimal_parser_is_exact_at_storage_boundaries() {
        for (text, raw) in [
            ("-214.7483648", i32::MIN),
            ("-1.2345678", -12_345_678),
            ("-0.0000001", -1),
            ("0", 0),
            (".0000001", 1),
            ("1.234567800", 12_345_678),
            ("1.0000000000000000000000000000000000000000", 10_000_000),
            ("12345678e-7", 12_345_678),
            ("214.7483647", i32::MAX),
        ] {
            assert_eq!(FixedPoint7dps::parse_decimal(text).unwrap().raw(), raw);
        }
        for text in [
            "214.7483648",
            "-214.7483649",
            "1.23456781",
            "1e1000",
            "NaN",
            "Infinity",
            "",
        ] {
            assert!(
                FixedPoint7dps::parse_decimal(text).is_none(),
                "accepted {text:?}"
            );
        }
    }

    #[test]
    fn fixed_point_storage_formats_and_widens_exactly() {
        let positive = FixedPoint7dps::from_raw(12_345_678);
        let negative = FixedPoint7dps::from_raw(-12_345_678);
        assert_eq!(positive.raw(), 12_345_678);
        assert_eq!(positive.as_f64(), 1.2345678);
        assert_eq!(negative.as_f64(), -1.2345678);
        assert_eq!(FixedPoint7dps::ZERO.as_f64(), 0.0);
        assert_eq!(FixedPoint7dps::MIN.as_f64(), -214.7483648);
        assert_eq!(FixedPoint7dps::MAX.as_f64(), 214.7483647);
        assert_eq!(positive.to_string(), "1.2345678");
        assert_eq!(FixedPoint7dps::MIN.to_string(), "-214.7483648");
        assert_eq!(FixedPoint7dps::MAX.to_string(), "214.7483647");
    }

    #[test]
    fn non_numeric_values_fail_numeric_access() {
        let v = Value::Enum {
            id: 1,
            member: "On".into(),
        };
        assert!(v.m1_scalar().is_err());
        assert!(!v.is_numeric());
        assert!(Value::Str("x".into()).m1_scalar().is_err());
    }

    #[test]
    fn bool_coercion() {
        assert!(Value::Bool(true).as_bool().unwrap());
        assert!(!Value::Bool(false).as_bool().unwrap());
        // M1 is strongly typed: no int->bool.
        assert!(Value::m1_integer(1).as_bool().is_err());
        assert!(Value::m1_float(0.0).as_bool().is_err());
    }

    #[test]
    fn truthy_forwards_to_as_bool() {
        assert!(Value::Bool(true).truthy().unwrap());
        assert!(!Value::Bool(false).truthy().unwrap());
        assert!(Value::m1_unsigned(0).truthy().is_err());
    }
}
