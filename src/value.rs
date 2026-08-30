// SPDX-License-Identifier: GPL-3.0-or-later
//! Runtime value types and strict coercions.
//!
//! M1 is strongly typed: there is no implicit `int -> bool` or `bool -> int`
//! coercion. [`M1Scalar`] holds the actual M1-width numeric forms. The legacy
//! `i64`/`u64`/`f64` variants on [`Value`] remain temporarily so the evaluator
//! can migrate one boundary at a time without changing existing callers.
//!
//! The numeric coercions here exist only to drive code that has not migrated
//! yet. Anything non-numeric (`Bool`, `Enum`, `Str`) is an explicit
//! `EvalError::TypeError` rather than a silent fallback. The evaluator never
//! substitutes a guessed numeric value.

use crate::error::EvalError;
use m1_typecheck::Project;

/// The signed, 32-bit storage used by M1's seven-decimal-place fixed-point
/// scalar. A raw value of `1` represents `0.0000001`.
///
/// This type only models storage and exact scale conversion. Language-level
/// rounding and saturation belong to the conversion builtin, not this type.
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

    /// Convert to a host float for temporary compatibility boundaries.
    pub fn as_f64(self) -> f64 {
        f64::from(self.0) / Self::SCALE as f64
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

    /// Widen to the host numeric format used by the temporary evaluator path.
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

    /// Convert to one of the legacy numeric [`Value`] variants.
    ///
    /// This is an explicit compatibility boundary scheduled for removal with
    /// the legacy execution path. Widening the three binary forms is lossless;
    /// fixed point is represented by its scaled decimal value as an `f64`.
    /// [`Value::try_as_m1_scalar_for`] restores the original scalar when the
    /// caller supplies this value's [`M1ScalarKind`].
    pub fn into_legacy_value(self) -> Value {
        match self {
            Self::FloatingPoint(value) => Value::Float(f64::from(value)),
            Self::Integer(value) => Value::Int(i64::from(value)),
            Self::UnsignedInteger(value) => Value::Uint(u64::from(value)),
            Self::FixedPoint7dps(value) => Value::Float(value.as_f64()),
        }
    }
}

/// Identifies a temporary host-width numeric variant so migration code can
/// count the legacy values still crossing a boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyNumericKind {
    Int64,
    Uint64,
    Float64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    /// Temporary signed host-width representation. Use [`Value::M1`] for new
    /// M1-width values.
    Int(i64),
    /// Temporary unsigned host-width representation. Use [`Value::M1`] for new
    /// M1-width values.
    Uint(u64),
    /// Temporary host-width floating representation. Use [`Value::M1`] for new
    /// M1-width values.
    Float(f64),
    /// A numeric value represented with its actual M1 width and signedness.
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
    ///
    /// Unlike [`Value::try_as_m1_scalar`], this rejects every legacy numeric
    /// variant. Core evaluator paths use this accessor so a host-width value
    /// cannot silently re-enter script execution outside a named boundary.
    pub fn m1_scalar(&self) -> Result<M1Scalar, EvalError> {
        match self {
            Self::M1(value) => Ok(*value),
            Self::Int(_) | Self::Uint(_) | Self::Float(_) => Err(EvalError::TypeError {
                detail: format!("legacy numeric value {self:?} reached an M1-only evaluator path"),
            }),
            other => Err(EvalError::TypeError {
                detail: format!("{other:?} is not numeric"),
            }),
        }
    }

    /// Narrow a legacy builtin result to its corresponding M1 scalar family.
    ///
    /// This is the named output boundary for builtin implementations that are
    /// migrated by issue #38. Signed and unsigned results are range checked;
    /// legacy floats round to binary32 and reject finite overflow. Non-numeric
    /// results already use their final representation and pass through.
    pub(crate) fn restore_legacy_builtin_result(self) -> Result<Self, EvalError> {
        match self {
            Self::Int(value) => i32::try_from(value)
                .map(Self::m1_integer)
                .map_err(|_| numeric_width_error(&Self::Int(value), "Integer (i32)")),
            Self::Uint(value) => u32::try_from(value)
                .map(Self::m1_unsigned)
                .map_err(|_| numeric_width_error(&Self::Uint(value), "UnsignedInteger (u32)")),
            Self::Float(value) => Self::Float(value)
                .try_as_m1_scalar_for(M1ScalarKind::FloatingPoint)
                .map(Self::M1),
            value => Ok(value),
        }
    }

    /// Widen an M1 scalar for a legacy builtin implementation.
    ///
    /// This is the named input boundary paired with
    /// [`Value::restore_legacy_builtin_result`]. It is deliberately unavailable as
    /// a general evaluator coercion.
    pub(crate) fn into_legacy_builtin_argument(self) -> Self {
        match self {
            Self::M1(value) => value.into_legacy_value(),
            value => value,
        }
    }

    /// Coerce a numeric value to `f64`. Non-numeric values are a `TypeError`;
    /// we never invent a default number.
    pub fn as_f64(&self) -> Result<f64, EvalError> {
        match self {
            Value::Float(x) => Ok(*x),
            Value::Int(x) => Ok(*x as f64),
            Value::Uint(x) => Ok(*x as f64),
            Value::M1(value) => Ok(value.as_f64()),
            other => Err(EvalError::TypeError {
                detail: format!("{other:?} is not numeric"),
            }),
        }
    }

    /// Convert a numeric value to its unambiguous M1-width representation.
    ///
    /// Existing M1 values pass through unchanged. Legacy `Int` and `Uint` values
    /// map to their matching M1 integer types. A legacy [`Value::Float`] is
    /// ambiguous because it may represent either M1 `FloatingPoint` or
    /// `FixedPoint7dps`, so this convenience method rejects it. Use
    /// [`Value::try_as_m1_scalar_for`] at typed migration boundaries.
    pub fn try_as_m1_scalar(&self) -> Result<M1Scalar, EvalError> {
        match self {
            Value::M1(value) => Ok(*value),
            Value::Int(_) => self.try_as_m1_scalar_for(M1ScalarKind::Integer),
            Value::Uint(_) => self.try_as_m1_scalar_for(M1ScalarKind::UnsignedInteger),
            Value::Float(_) => Err(EvalError::TypeError {
                detail: format!(
                    "{self:?} is ambiguous between M1 FloatingPoint and FixedPoint7dps; use Value::try_as_m1_scalar_for"
                ),
            }),
            other => Err(EvalError::TypeError {
                detail: format!("{other:?} is not numeric"),
            }),
        }
    }

    /// Restore a value at a typed legacy-to-M1 compatibility boundary.
    ///
    /// This conversion restores storage identity; it is not a language cast.
    /// Legacy `Int`, `Uint`, and `Float` values must target their corresponding
    /// storage family. `Float` may target either `FloatingPoint` or
    /// `FixedPoint7dps`, which resolves the ambiguity in the legacy model.
    /// Fixed Point restoration accepts only values exactly produced by the 7dps
    /// scale, so it recovers the original raw `i32` without applying the rounding
    /// or saturation rules owned by `Convert.ToFixed7DP`.
    pub fn try_as_m1_scalar_for(&self, target: M1ScalarKind) -> Result<M1Scalar, EvalError> {
        match (self, target) {
            (Value::M1(value), target) if value.kind() == target => Ok(*value),
            (Value::M1(value), target) => Err(incompatible_scalar_kind(*value, target)),
            (Value::Int(value), M1ScalarKind::Integer) => i32::try_from(*value)
                .map(M1Scalar::Integer)
                .map_err(|_| numeric_width_error(self, "Integer (i32)")),
            (Value::Uint(value), M1ScalarKind::UnsignedInteger) => u32::try_from(*value)
                .map(M1Scalar::UnsignedInteger)
                .map_err(|_| numeric_width_error(self, "UnsignedInteger (u32)")),
            (Value::Float(value), M1ScalarKind::FloatingPoint) => {
                let narrowed = *value as f32;
                if value.is_finite() && narrowed.is_infinite() {
                    Err(numeric_width_error(self, "FloatingPoint (binary32)"))
                } else {
                    Ok(M1Scalar::FloatingPoint(narrowed))
                }
            }
            (Value::Float(value), M1ScalarKind::FixedPoint7dps) => {
                restore_fixed_point_7dps(self, *value).map(M1Scalar::FixedPoint7dps)
            }
            (Value::Int(_) | Value::Uint(_) | Value::Float(_), target) => {
                Err(incompatible_legacy_kind(self, target))
            }
            (other, _) => Err(EvalError::TypeError {
                detail: format!("{other:?} is not numeric"),
            }),
        }
    }

    /// Identify the temporary host-width numeric form, if this value uses one.
    /// This makes remaining legacy traffic countable during the migration.
    pub const fn legacy_numeric_kind(&self) -> Option<LegacyNumericKind> {
        match self {
            Value::Int(_) => Some(LegacyNumericKind::Int64),
            Value::Uint(_) => Some(LegacyNumericKind::Uint64),
            Value::Float(_) => Some(LegacyNumericKind::Float64),
            _ => None,
        }
    }

    /// Whether this is either an M1-width or temporary legacy numeric value.
    pub const fn is_numeric(&self) -> bool {
        matches!(
            self,
            Value::Int(_) | Value::Uint(_) | Value::Float(_) | Value::M1(_)
        )
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

fn numeric_width_error(value: &Value, target: &str) -> EvalError {
    EvalError::TypeError {
        detail: format!("{value:?} is outside the range of M1 {target}"),
    }
}

fn incompatible_scalar_kind(value: M1Scalar, target: M1ScalarKind) -> EvalError {
    EvalError::TypeError {
        detail: format!(
            "M1 {:?} cannot be restored as M1 {target:?}; language casts are not compatibility conversions",
            value.kind()
        ),
    }
}

fn incompatible_legacy_kind(value: &Value, target: M1ScalarKind) -> EvalError {
    EvalError::TypeError {
        detail: format!(
            "{value:?} cannot restore M1 {target:?}; language casts are not compatibility conversions"
        ),
    }
}

fn restore_fixed_point_7dps(source: &Value, value: f64) -> Result<FixedPoint7dps, EvalError> {
    if !value.is_finite() {
        return Err(numeric_width_error(source, "FixedPoint7dps"));
    }

    let scaled = value * FixedPoint7dps::SCALE as f64;
    if scaled < f64::from(i32::MIN) || scaled > f64::from(i32::MAX) {
        return Err(numeric_width_error(source, "FixedPoint7dps"));
    }

    let restored = FixedPoint7dps::from_raw(scaled.round() as i32);
    if restored.as_f64().to_bits() != value.to_bits() {
        return Err(EvalError::TypeError {
            detail: format!("{source:?} is not an exact M1 FixedPoint7dps compatibility value"),
        });
    }
    Ok(restored)
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

    // ---- as_enum_int (.AsInteger over EnumType.members) ----

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
        match Value::Int(3).as_enum_int(&project) {
            Err(EvalError::TypeError { .. }) => {}
            other => panic!("expected TypeError on non-enum value, got {other:?}"),
        }
    }

    #[test]
    fn float_and_int_coerce_to_f64() {
        assert_eq!(Value::Float(2.5).as_f64().unwrap(), 2.5);
        assert_eq!(Value::Int(-3).as_f64().unwrap(), -3.0);
        assert_eq!(Value::Uint(7).as_f64().unwrap(), 7.0);
        assert_eq!(
            Value::M1(M1Scalar::FloatingPoint(2.5)).as_f64().unwrap(),
            2.5
        );
        assert_eq!(
            Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(
                12_345_678
            )))
            .as_f64()
            .unwrap(),
            1.2345678
        );
        assert!(Value::Str("x".into()).as_f64().is_err());
    }

    #[test]
    fn m1_scalar_boundaries_are_explicit() {
        assert_eq!(
            Value::Int(i64::from(i32::MIN)).try_as_m1_scalar().unwrap(),
            M1Scalar::Integer(i32::MIN)
        );
        assert_eq!(
            Value::Int(i64::from(i32::MAX)).try_as_m1_scalar().unwrap(),
            M1Scalar::Integer(i32::MAX)
        );
        assert!(
            Value::Int(i64::from(i32::MIN) - 1)
                .try_as_m1_scalar()
                .is_err()
        );
        assert!(
            Value::Int(i64::from(i32::MAX) + 1)
                .try_as_m1_scalar()
                .is_err()
        );

        assert_eq!(
            Value::Uint(0).try_as_m1_scalar().unwrap(),
            M1Scalar::UnsignedInteger(0)
        );
        assert_eq!(
            Value::Uint(u64::from(u32::MAX)).try_as_m1_scalar().unwrap(),
            M1Scalar::UnsignedInteger(u32::MAX)
        );
        assert!(
            Value::Uint(u64::from(u32::MAX) + 1)
                .try_as_m1_scalar()
                .is_err()
        );
    }

    #[test]
    fn binary32_conversion_exposes_precision_and_range() {
        let narrowed = Value::Float(16_777_217.0)
            .try_as_m1_scalar_for(M1ScalarKind::FloatingPoint)
            .unwrap();
        assert_eq!(narrowed, M1Scalar::FloatingPoint(16_777_216.0));
        assert_eq!(narrowed.into_legacy_value(), Value::Float(16_777_216.0));

        assert_eq!(
            Value::Float(f64::from(f32::MIN))
                .try_as_m1_scalar_for(M1ScalarKind::FloatingPoint)
                .unwrap(),
            M1Scalar::FloatingPoint(f32::MIN)
        );
        assert_eq!(
            Value::Float(f64::from(f32::MAX))
                .try_as_m1_scalar_for(M1ScalarKind::FloatingPoint)
                .unwrap(),
            M1Scalar::FloatingPoint(f32::MAX)
        );
        let smallest_subnormal = f32::from_bits(1);
        assert_eq!(
            Value::Float(f64::from(smallest_subnormal))
                .try_as_m1_scalar_for(M1ScalarKind::FloatingPoint)
                .unwrap(),
            M1Scalar::FloatingPoint(smallest_subnormal)
        );
        let negative_zero = Value::Float(-0.0)
            .try_as_m1_scalar_for(M1ScalarKind::FloatingPoint)
            .unwrap();
        assert!(matches!(
            negative_zero,
            M1Scalar::FloatingPoint(value) if value.to_bits() == (-0.0_f32).to_bits()
        ));
        assert!(
            Value::Float(f64::MAX)
                .try_as_m1_scalar_for(M1ScalarKind::FloatingPoint)
                .is_err()
        );
        assert!(matches!(
            Value::Float(f64::NAN).try_as_m1_scalar_for(M1ScalarKind::FloatingPoint),
            Ok(M1Scalar::FloatingPoint(value)) if value.is_nan()
        ));
        assert_eq!(
            Value::Float(f64::NEG_INFINITY)
                .try_as_m1_scalar_for(M1ScalarKind::FloatingPoint)
                .unwrap(),
            M1Scalar::FloatingPoint(f32::NEG_INFINITY)
        );
    }

    #[test]
    fn inferred_conversion_rejects_ambiguous_legacy_float_storage() {
        assert!(Value::Float(1.0).try_as_m1_scalar().is_err());
        assert_eq!(
            Value::Int(-1).try_as_m1_scalar().unwrap(),
            M1Scalar::Integer(-1)
        );
        assert_eq!(
            Value::Uint(1).try_as_m1_scalar().unwrap(),
            M1Scalar::UnsignedInteger(1)
        );

        let fixed = M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(1));
        assert_eq!(Value::M1(fixed).try_as_m1_scalar().unwrap(), fixed);
    }

    #[test]
    fn every_m1_scalar_has_an_explicit_legacy_compatibility_conversion() {
        assert_eq!(
            M1Scalar::Integer(i32::MIN).into_legacy_value(),
            Value::Int(i64::from(i32::MIN))
        );
        assert_eq!(
            M1Scalar::UnsignedInteger(u32::MAX).into_legacy_value(),
            Value::Uint(u64::from(u32::MAX))
        );
        assert_eq!(
            M1Scalar::FloatingPoint(f32::MIN_POSITIVE).into_legacy_value(),
            Value::Float(f64::from(f32::MIN_POSITIVE))
        );
        assert_eq!(
            M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(-1)).into_legacy_value(),
            Value::Float(-0.0000001)
        );
    }

    #[test]
    fn every_m1_scalar_kind_round_trips_through_legacy_storage() {
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
            let legacy = scalar.into_legacy_value();
            let restored = legacy.try_as_m1_scalar_for(scalar.kind()).unwrap();
            assert_eq!(restored, scalar, "failed to restore {scalar:?}");
        }
    }

    #[test]
    fn fixed_point_legacy_round_trip_restores_exact_raw_storage() {
        for raw in [i32::MIN, -12_345_678, -1, 0, 1, 12_345_678, i32::MAX] {
            let scalar = M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(raw));
            let legacy = scalar.into_legacy_value();
            assert_eq!(
                legacy
                    .try_as_m1_scalar_for(M1ScalarKind::FixedPoint7dps)
                    .unwrap(),
                scalar,
                "failed to restore fixed-point raw value {raw}"
            );
        }
    }

    #[test]
    fn fixed_point_compatibility_restore_rejects_lossy_values_and_language_casts() {
        for value in [1.23456789, -0.0, 214.7483648, f64::NAN, f64::INFINITY] {
            assert!(
                Value::Float(value)
                    .try_as_m1_scalar_for(M1ScalarKind::FixedPoint7dps)
                    .is_err(),
                "unexpectedly restored {value:?} as exact fixed point"
            );
        }

        assert!(
            Value::Int(1)
                .try_as_m1_scalar_for(M1ScalarKind::FloatingPoint)
                .is_err()
        );
        assert!(
            Value::M1(M1Scalar::Integer(1))
                .try_as_m1_scalar_for(M1ScalarKind::UnsignedInteger)
                .is_err()
        );
    }

    #[test]
    fn fixed_point_7dps_uses_signed_scaled_i32_storage() {
        let positive = FixedPoint7dps::from_raw(12_345_678);
        let negative = FixedPoint7dps::from_raw(-12_345_678);
        assert_eq!(positive.raw(), 12_345_678);
        assert_eq!(positive.as_f64(), 1.2345678);
        assert_eq!(negative.as_f64(), -1.2345678);
        assert_eq!(FixedPoint7dps::ZERO.as_f64(), 0.0);
        assert_eq!(FixedPoint7dps::MIN.as_f64(), -214.7483648);
        assert_eq!(FixedPoint7dps::MAX.as_f64(), 214.7483647);

        assert_eq!(
            M1Scalar::FixedPoint7dps(positive).into_legacy_value(),
            Value::Float(1.2345678)
        );
    }

    #[test]
    fn legacy_numeric_forms_are_measurable() {
        assert_eq!(
            Value::Int(0).legacy_numeric_kind(),
            Some(LegacyNumericKind::Int64)
        );
        assert_eq!(
            Value::Uint(0).legacy_numeric_kind(),
            Some(LegacyNumericKind::Uint64)
        );
        assert_eq!(
            Value::Float(0.0).legacy_numeric_kind(),
            Some(LegacyNumericKind::Float64)
        );
        assert_eq!(Value::M1(M1Scalar::Integer(0)).legacy_numeric_kind(), None);
        assert_eq!(Value::Bool(false).legacy_numeric_kind(), None);
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
