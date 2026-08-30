// SPDX-License-Identifier: GPL-3.0-or-later
//! Core methods on resolved project value objects.
//!
//! The project file owns each value object's validation rule. Runtime dispatch
//! resolves the source spelling to a canonical project path, then uses this
//! index for `Validate`, `Constrain`, and `Set`. `GetUnscheduled` reads through
//! the normal value path; only dependency analysis treats that accessor
//! specially.

use crate::error::EvalError;
use crate::expr::EvalCtx;
use crate::ident::{Target, classify};
use crate::value::{FixedPoint7dps, M1Scalar, M1ScalarKind, Value};
use m1_typecheck::Project;
use m1_typecheck::symbols::SymbolKind;
use m1_typecheck::types::ValueType;
use std::collections::{HashMap, HashSet};

/// Validation rules parsed from `.m1prj` component properties.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ObjectRules {
    validation: HashMap<String, ValidationRule>,
}

#[derive(Debug, Clone, PartialEq)]
enum ValidationRule {
    MinMax {
        min: f64,
        max: f64,
    },
    /// M1 project exports use `Positive` for a lower bound of zero.
    NonNegative,
    /// Preserve unknown project data and fail only if execution reaches it.
    Unsupported(String),
}

impl ObjectRules {
    /// Parse validation metadata without retaining the project XML.
    pub(crate) fn from_project_xml(xml: &str) -> Result<Self, EvalError> {
        let doc =
            roxmltree::Document::parse(xml).map_err(|error| EvalError::UnsupportedConstruct {
                kind: format!("project XML re-parse for object rules failed: {error}"),
                at: 0,
            })?;
        let mut rules = Self::default();

        for component in doc
            .descendants()
            .filter(|node| node.has_tag_name("Component"))
        {
            let Some(path) = component.attribute("Name") else {
                continue;
            };
            let Some(props) = component.children().find(|node| node.has_tag_name("Props")) else {
                continue;
            };
            let Some(raw_kind) = props.attribute("Validation").map(str::trim) else {
                continue;
            };
            if raw_kind.is_empty() || raw_kind.eq_ignore_ascii_case("None") {
                continue;
            }

            let rule = match raw_kind {
                "MinMax" => {
                    let min = parse_bound(path, "ValMin", props.attribute("ValMin"))?;
                    let max = parse_bound(path, "ValMax", props.attribute("ValMax"))?;
                    if min > max {
                        return Err(invalid_rule(
                            path,
                            format!("ValMin {min} exceeds ValMax {max}"),
                        ));
                    }
                    ValidationRule::MinMax { min, max }
                }
                "Positive" => ValidationRule::NonNegative,
                other => ValidationRule::Unsupported(other.to_string()),
            };
            rules.validation.insert(path.to_string(), rule);
        }

        Ok(rules)
    }

    fn rule_for<'a>(&'a self, object: &str, value_path: &str) -> Option<&'a ValidationRule> {
        self.validation
            .get(object)
            .or_else(|| self.validation.get(value_path))
    }
}

fn parse_bound(path: &str, name: &str, raw: Option<&str>) -> Result<f64, EvalError> {
    let value = raw
        .ok_or_else(|| invalid_rule(path, format!("MinMax is missing {name}")))?
        .parse::<f64>()
        .map_err(|_| invalid_rule(path, format!("{name} is not a number")))?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(invalid_rule(path, format!("{name} must be finite")))
    }
}

fn invalid_rule(path: &str, detail: String) -> EvalError {
    EvalError::UnsupportedConstruct {
        kind: format!("invalid validation rule on {path:?}: {detail}"),
        at: 0,
    }
}

/// Whether a resolved project symbol provides a numeric scalar value.
pub(crate) fn is_numeric_source(canon: &str, project: &Project) -> bool {
    numeric_value_path(canon, project).is_some()
}

/// Resolve a value-bearing object to the concrete numeric symbol it reads.
/// Direct channels, parameters, and constants return themselves. A value
/// compound follows its declared default or generated `.Value` child.
pub(crate) fn numeric_value_path(canon: &str, project: &Project) -> Option<String> {
    numeric_value_path_inner(canon, project, &mut HashSet::new())
}

/// Resolve a writable value object to the channel or parameter that stores it.
/// A direct channel/parameter stores itself. A value compound follows its
/// declared default, then its generated `.Value` child. Tables and package IO
/// objects retain their dedicated dispatch rather than becoming arbitrary
/// writable channels through this helper.
pub(crate) fn writable_value_path(canon: &str, project: &Project) -> Option<String> {
    writable_value_path_inner(canon, project, &mut HashSet::new())
}

fn writable_value_path_inner(
    canon: &str,
    project: &Project,
    seen: &mut HashSet<String>,
) -> Option<String> {
    if !seen.insert(canon.to_string()) {
        return None;
    }
    let symbol = project.symbols().get(canon)?;
    if matches!(symbol.kind, SymbolKind::Channel | SymbolKind::Parameter) {
        return Some(canon.to_string());
    }
    if symbol.kind != SymbolKind::Group {
        return None;
    }

    if let Some(default) = symbol.default_value.as_deref() {
        let locals = HashMap::new();
        if let Target::Symbol(path) = classify(default, Some(canon), None, project, &locals)
            && let Some(value_path) = writable_value_path_inner(&path, project, seen)
        {
            return Some(value_path);
        }
    }

    let value_path = format!("{canon}.Value");
    project
        .symbols()
        .get(&value_path)
        .and_then(|_| writable_value_path_inner(&value_path, project, seen))
}

fn numeric_value_path_inner(
    canon: &str,
    project: &Project,
    seen: &mut HashSet<String>,
) -> Option<String> {
    if !seen.insert(canon.to_string()) {
        return None;
    }
    let symbol = project.symbols().get(canon)?;
    let numeric = symbol_is_numeric(symbol.value_type, symbol.declared_type.as_deref());
    match symbol.kind {
        SymbolKind::Channel | SymbolKind::Parameter | SymbolKind::Constant if numeric => {
            return Some(canon.to_string());
        }
        SymbolKind::Object | SymbolKind::Reference | SymbolKind::Other if numeric => {
            // These are the typed external-value objects that `read_symbol`
            // reads directly. In particular, do not apply this shortcut to a
            // Table: its type describes the generated `.Value` output, not a
            // scalar stored on the table symbol itself.
            return Some(canon.to_string());
        }
        _ => {}
    }

    if symbol.kind == SymbolKind::Group
        && let Some(default) = symbol.default_value.as_deref()
    {
        let locals = HashMap::new();
        if let Target::Symbol(path) = classify(default, Some(canon), None, project, &locals)
            && let Some(value_path) = numeric_value_path_inner(&path, project, seen)
        {
            return Some(value_path);
        }
    }

    if matches!(
        symbol.kind,
        SymbolKind::Group | SymbolKind::Table | SymbolKind::Object
    ) {
        let value_path = format!("{canon}.Value");
        return project
            .symbols()
            .get(&value_path)
            .and_then(|_| numeric_value_path_inner(&value_path, project, seen));
    }
    None
}

fn symbol_is_numeric(value_type: ValueType, declared_type: Option<&str>) -> bool {
    matches!(
        value_type,
        ValueType::Integer | ValueType::Unsigned | ValueType::Float
    ) || declared_type.is_some_and(|raw| {
        raw.eq_ignore_ascii_case("FixedPoint7dps") || raw.eq_ignore_ascii_case("fixed7dps")
    })
}

/// Return whether `value` satisfies the receiver's validation range.
pub(crate) fn validate(
    object: &str,
    args: &[Value],
    ctx: &mut EvalCtx,
) -> Result<Value, EvalError> {
    let (canon, value_path) = resolve_numeric_source(object, "Validate", ctx)?;
    let value = args
        .first()
        .ok_or_else(|| bad_arity(object, "Validate", args.len(), 1))?;
    let scalar = value.m1_scalar()?;
    let Some(rule) = ctx
        .object_rules
        .and_then(|rules| rules.rule_for(&canon, &value_path))
    else {
        return Ok(Value::Bool(true));
    };
    Ok(Value::Bool(rule_accepts(rule, scalar, &canon)?))
}

/// Clamp a numeric argument to the receiver's validation range while retaining
/// the argument's M1 scalar family.
pub(crate) fn constrain(
    object: &str,
    args: &[Value],
    ctx: &mut EvalCtx,
) -> Result<Value, EvalError> {
    let (canon, value_path) = resolve_numeric_source(object, "Constrain", ctx)?;
    let value = args
        .first()
        .cloned()
        .ok_or_else(|| bad_arity(object, "Constrain", args.len(), 1))?;
    constrain_value(&canon, &value_path, value, ctx.object_rules)
}

/// Read a numeric value without adding a scheduler dependency. The scheduler
/// distinction lives in `summary`; runtime uses the same read semantics as the
/// source object itself.
pub(crate) fn get_unscheduled(object: &str, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    let (_, value_path) = resolve_numeric_source(object, "GetUnscheduled", ctx)?;
    crate::expr::read_symbol(&value_path, ctx)
}

/// Set a writable channel, parameter, or value compound after target-type
/// conversion and range clamping.
pub(crate) fn set(object: &str, args: &[Value], ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    let Target::Symbol(canon) = classify(
        object,
        ctx.group,
        ctx.fn_symbol,
        ctx.project,
        &ctx.env.locals,
    ) else {
        return Err(unsupported(object, "Set"));
    };
    let value_path =
        writable_value_path(&canon, ctx.project).ok_or_else(|| unsupported(object, "Set"))?;
    let value = args
        .first()
        .cloned()
        .ok_or_else(|| bad_arity(object, "Set", args.len(), 1))?;
    let value = crate::expr::coerce_for_channel(&value_path, value, ctx.project)?;
    let value = if value.is_numeric() {
        constrain_value(&canon, &value_path, value, ctx.object_rules)?
    } else {
        value
    };

    ctx.env.set(value_path.clone(), value.clone());
    if let Some(trace) = ctx.trace.as_deref_mut() {
        trace.record_channel(value_path, value);
    }
    Ok(Value::Bool(true))
}

fn resolve_numeric_source(
    object: &str,
    method: &str,
    ctx: &EvalCtx,
) -> Result<(String, String), EvalError> {
    let Target::Symbol(canon) = classify(
        object,
        ctx.group,
        ctx.fn_symbol,
        ctx.project,
        &ctx.env.locals,
    ) else {
        return Err(unsupported(object, method));
    };
    let value_path =
        numeric_value_path(&canon, ctx.project).ok_or_else(|| unsupported(object, method))?;
    Ok((canon, value_path))
}

fn constrain_value(
    canon: &str,
    value_path: &str,
    value: Value,
    rules: Option<&ObjectRules>,
) -> Result<Value, EvalError> {
    let Some(rule) = rules.and_then(|rules| rules.rule_for(canon, value_path)) else {
        return Ok(value);
    };
    let scalar = value.m1_scalar()?;
    let x = scalar.as_f64();
    let (min, max) = rule_bounds(rule, canon)?;

    if x.is_nan() {
        return Ok(value);
    }
    if min.is_some_and(|bound| x < bound) {
        let constrained = bound_value(
            min.expect("checked above"),
            scalar.kind(),
            BoundSide::Lower,
            canon,
        )?;
        return ensure_in_range(constrained, rule, canon, min, max);
    }
    if max.is_some_and(|bound| x > bound) {
        let constrained = bound_value(
            max.expect("checked above"),
            scalar.kind(),
            BoundSide::Upper,
            canon,
        )?;
        return ensure_in_range(constrained, rule, canon, min, max);
    }
    Ok(value)
}

fn ensure_in_range(
    value: Value,
    rule: &ValidationRule,
    canon: &str,
    min: Option<f64>,
    max: Option<f64>,
) -> Result<Value, EvalError> {
    let scalar = value.m1_scalar()?;
    if rule_accepts(rule, scalar, canon)? {
        return Ok(value);
    }
    let range = match (min, max) {
        (Some(min), Some(max)) => format!("[{min}, {max}]"),
        (Some(min), None) => format!("[{min}, +infinity)"),
        (None, Some(max)) => format!("(-infinity, {max}]"),
        (None, None) => "unbounded".to_string(),
    };
    Err(EvalError::TypeError {
        detail: format!(
            "validation range {range} on {canon:?} has no value in M1 {:?}",
            scalar.kind()
        ),
    })
}

fn rule_accepts(rule: &ValidationRule, scalar: M1Scalar, canon: &str) -> Result<bool, EvalError> {
    let x = scalar.as_f64();
    if x.is_nan() {
        return Ok(false);
    }
    let (min, max) = rule_bounds(rule, canon)?;
    Ok(min.is_none_or(|bound| x >= bound) && max.is_none_or(|bound| x <= bound))
}

fn rule_bounds(
    rule: &ValidationRule,
    canon: &str,
) -> Result<(Option<f64>, Option<f64>), EvalError> {
    match rule {
        ValidationRule::MinMax { min, max } => Ok((Some(*min), Some(*max))),
        ValidationRule::NonNegative => Ok((Some(0.0), None)),
        ValidationRule::Unsupported(kind) => Err(EvalError::TypeError {
            detail: format!("object {canon:?} uses unsupported validation rule {kind:?}"),
        }),
    }
}

#[derive(Clone, Copy)]
enum BoundSide {
    Lower,
    Upper,
}

fn bound_value(
    bound: f64,
    kind: M1ScalarKind,
    side: BoundSide,
    canon: &str,
) -> Result<Value, EvalError> {
    let invalid = || EvalError::TypeError {
        detail: format!("validation bound {bound} on {canon:?} has no value in M1 {kind:?}"),
    };
    let scalar = match kind {
        M1ScalarKind::FloatingPoint => {
            let mut value = bound as f32;
            if !value.is_finite() {
                return Err(invalid());
            }
            match side {
                BoundSide::Lower if f64::from(value) < bound => value = value.next_up(),
                BoundSide::Upper if f64::from(value) > bound => value = value.next_down(),
                _ => {}
            }
            M1Scalar::FloatingPoint(value)
        }
        M1ScalarKind::Integer => {
            let rounded = match side {
                BoundSide::Lower => bound.ceil(),
                BoundSide::Upper => bound.floor(),
            };
            if rounded < f64::from(i32::MIN) || rounded > f64::from(i32::MAX) {
                return Err(invalid());
            }
            M1Scalar::Integer(rounded as i32)
        }
        M1ScalarKind::UnsignedInteger => {
            let rounded = match side {
                BoundSide::Lower => bound.ceil(),
                BoundSide::Upper => bound.floor(),
            };
            if rounded < 0.0 || rounded > f64::from(u32::MAX) {
                return Err(invalid());
            }
            M1Scalar::UnsignedInteger(rounded as u32)
        }
        M1ScalarKind::FixedPoint7dps => {
            let scaled = bound * FixedPoint7dps::SCALE as f64;
            if !scaled.is_finite() {
                return Err(invalid());
            }
            let min_raw = i64::from(i32::MIN);
            let max_raw = i64::from(i32::MAX);
            let mut raw = scaled.round().clamp(min_raw as f64, max_raw as f64) as i64;
            let fixed_at = |raw: i64| {
                FixedPoint7dps::from_raw(i32::try_from(raw).expect("raw is range-checked"))
            };

            match side {
                BoundSide::Lower => {
                    while raw <= max_raw && fixed_at(raw).as_f64() < bound {
                        raw += 1;
                    }
                    while raw > min_raw && fixed_at(raw - 1).as_f64() >= bound {
                        raw -= 1;
                    }
                }
                BoundSide::Upper => {
                    while raw >= min_raw && fixed_at(raw).as_f64() > bound {
                        raw -= 1;
                    }
                    while raw < max_raw && fixed_at(raw + 1).as_f64() <= bound {
                        raw += 1;
                    }
                }
            }
            let raw = i32::try_from(raw).map_err(|_| invalid())?;
            M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(raw))
        }
    };
    Ok(Value::M1(scalar))
}

fn bad_arity(object: &str, method: &str, got: usize, expected: usize) -> EvalError {
    EvalError::BadCall {
        detail: format!("{object}.{method} expects {expected} argument(s), got {got}"),
    }
}

fn unsupported(object: &str, method: &str) -> EvalError {
    EvalError::UnsupportedBuiltin {
        object: object.to_string(),
        method: method.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_min_max_positive_and_unknown_rules() {
        let rules = ObjectRules::from_project_xml(
            r#"<Project><Component Name="Root.Min"><Props Validation="MinMax" ValMin="-2" ValMax="3"/></Component><Component Name="Root.Positive"><Props Validation="Positive"/></Component><Component Name="Root.Future"><Props Validation="FutureRule"/></Component></Project>"#,
        )
        .unwrap();
        assert_eq!(
            rules.validation.get("Root.Min"),
            Some(&ValidationRule::MinMax {
                min: -2.0,
                max: 3.0
            })
        );
        assert_eq!(
            rules.validation.get("Root.Positive"),
            Some(&ValidationRule::NonNegative)
        );
        assert_eq!(
            rules.validation.get("Root.Future"),
            Some(&ValidationRule::Unsupported("FutureRule".to_string()))
        );
    }

    #[test]
    fn malformed_min_max_rule_fails_with_the_object_path() {
        let error = ObjectRules::from_project_xml(
            r#"<Project><Component Name="Root.Bad"><Props Validation="MinMax" ValMin="4" ValMax="3"/></Component></Project>"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("Root.Bad"), "{error}");
        assert!(error.to_string().contains("exceeds"), "{error}");
    }

    #[test]
    fn constrain_rejects_ranges_with_no_value_in_the_argument_family() {
        let cases = [
            (
                "signed",
                0.2,
                0.8,
                Value::m1_integer(-1),
                Value::m1_integer(2),
            ),
            (
                "unsigned",
                0.2,
                0.8,
                Value::m1_unsigned(0),
                Value::m1_unsigned(1),
            ),
            (
                "float",
                1.000_000_01,
                1.000_000_02,
                Value::m1_float(1.0),
                Value::m1_float(f32::from_bits(1.0f32.to_bits() + 1)),
            ),
            (
                "fixed",
                0.000_000_01,
                0.000_000_02,
                Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::ZERO)),
                Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(1))),
            ),
        ];

        for (name, min, max, below, above) in cases {
            let rules = ObjectRules {
                validation: HashMap::from([(
                    name.to_string(),
                    ValidationRule::MinMax { min, max },
                )]),
            };
            for input in [below, above] {
                let error = constrain_value(name, name, input, Some(&rules)).unwrap_err();
                assert!(
                    error.to_string().contains("has no value in M1"),
                    "{name}: {error}"
                );
            }
        }
    }

    #[test]
    fn fixed_bounds_preserve_exact_seven_decimal_places() {
        let lower_rules = ObjectRules {
            validation: HashMap::from([(
                "lower".to_string(),
                ValidationRule::MinMax {
                    min: 79.020_497_0,
                    max: 100.0,
                },
            )]),
        };
        assert_eq!(
            constrain_value(
                "lower",
                "lower",
                Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::ZERO)),
                Some(&lower_rules),
            )
            .unwrap(),
            Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(
                790_204_970,
            )))
        );

        let upper_rules = ObjectRules {
            validation: HashMap::from([(
                "upper".to_string(),
                ValidationRule::MinMax {
                    min: 0.0,
                    max: 88.717_452_5,
                },
            )]),
        };
        assert_eq!(
            constrain_value(
                "upper",
                "upper",
                Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(
                    1_000_000_000,
                ))),
                Some(&upper_rules),
            )
            .unwrap(),
            Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(
                887_174_525,
            )))
        );
    }
}
