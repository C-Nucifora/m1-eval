// SPDX-License-Identifier: GPL-3.0-or-later
//! Counterfactual overrides: the [`Override`] model and its `CH=…` parser.
//!
//! A counterfactual run treats every logged channel as ground truth, then lets
//! the user replace one or more channels with an [`Override`] before recomputing
//! only the downstream cone. An [`Override`] is one such replacement, parsed from
//! a `CH=<rhs>` spec:
//!
//! - [`Override::Const`] — the right-hand side is a literal (`bool`, integer, or
//!   float). The channel is pinned to that constant [`Value`] every tick.
//! - [`Override::Expr`] — the right-hand side is anything else: a verbatim M1
//!   expression source, evaluated each tick in the channel's scope. The source is
//!   stored as-is here; this module performs **no evaluation** (that is a later
//!   counterfactual-runner task).
//!
//! ## Splitting `CH=rhs`
//!
//! M1 identifiers may contain spaces (`Engine Speed`, `Drive State`), and channel
//! *paths* are only ever split on `.`. A spec is therefore split on the **first**
//! `=` only: everything before it is the channel name (trimmed), everything after
//! is the right-hand side (trimmed). `Engine Speed=1000` keeps the space in the
//! channel; `Out=A = B` would take `Out` as the channel and `A = B` as the RHS.
//!
//! ## Const vs Expr classification
//!
//! The trimmed RHS is classified by the same scalar rules the scenario loader
//! uses ([`crate::scenario`]'s `RawValue`): it is a [`Override::Const`] when it
//! parses as a `bool` (`true`/`false`), an `i64`, or an `f64`; otherwise it is an
//! [`Override::Expr`] carrying the verbatim source. A bare numeric literal becomes
//! a [`Value::Int`] when it parses as an integer, else a [`Value::Float`] — the
//! same precedence the scenario `const` inputs follow.

use crate::error::EvalError;
use crate::value::Value;

/// A single counterfactual channel override, parsed from a `CH=<rhs>` spec.
///
/// Either a constant value pinned every tick ([`Override::Const`]) or a verbatim
/// expression source evaluated each tick in the channel's scope
/// ([`Override::Expr`]). This type is *model + parse only* — it never evaluates
/// the expression; the counterfactual runner does that.
#[derive(Debug, Clone, PartialEq)]
pub enum Override {
    /// Pin a channel to a literal value every tick.
    Const {
        /// The channel path being overridden (M1 path; may contain spaces).
        channel: String,
        /// The constant value to write each tick.
        value: Value,
    },
    /// Drive a channel from a verbatim M1 expression source, evaluated per tick.
    Expr {
        /// The channel path being overridden (M1 path; may contain spaces).
        channel: String,
        /// The verbatim right-hand-side source, stored as written (no evaluation).
        source: String,
    },
}

impl Override {
    /// Parse a `CH=<rhs>` override spec.
    ///
    /// Splits on the **first** `=` (channel paths may contain spaces, so the
    /// channel is everything before the first `=`, trimmed; the RHS is everything
    /// after, trimmed). A spec with no `=` fails loud with
    /// [`EvalError::UnsupportedConstruct`]; an empty channel name (e.g. `=5`) also
    /// fails loud. The trimmed RHS classifies as a [`Override::Const`] when it
    /// parses as `bool`/`i64`/`f64`, otherwise an [`Override::Expr`] holding the
    /// verbatim source.
    pub fn parse(spec: &str) -> Result<Override, EvalError> {
        let Some(eq) = spec.find('=') else {
            return Err(EvalError::UnsupportedConstruct {
                kind: format!(
                    "override spec {spec:?} has no `=` (expected `CHANNEL=value-or-expression`)"
                ),
                at: 0,
            });
        };
        let channel = spec[..eq].trim().to_string();
        let rhs = spec[eq + 1..].trim();
        if channel.is_empty() {
            return Err(EvalError::UnsupportedConstruct {
                kind: format!("override spec {spec:?} has an empty channel name before `=`"),
                at: 0,
            });
        }
        if rhs.is_empty() {
            return Err(EvalError::UnsupportedConstruct {
                kind: format!("override spec {spec:?} has an empty value/expression after `=`"),
                at: 0,
            });
        }

        // Classify the RHS as a literal (-> Const) or verbatim source (-> Expr),
        // following the scenario loader's scalar precedence: bool, then integer,
        // then float; anything else is an expression source held verbatim.
        if let Some(value) = parse_const_rhs(rhs) {
            Ok(Override::Const { channel, value })
        } else {
            Ok(Override::Expr {
                channel,
                source: rhs.to_string(),
            })
        }
    }

    /// The channel path this override targets (verbatim; may contain spaces).
    pub fn channel(&self) -> &str {
        match self {
            Override::Const { channel, .. } | Override::Expr { channel, .. } => channel,
        }
    }
}

/// Classify a trimmed override RHS as a constant literal, returning its [`Value`].
///
/// Returns `Some` for `true`/`false` ([`Value::Bool`]), an integer literal
/// ([`Value::Int`]), or a float literal ([`Value::Float`]); `None` when the RHS is
/// not a bare literal (so the caller treats it as an expression source). Integer
/// is tried before float so `5` is an `Int` and `5.0` is a `Float`, matching the
/// scenario `const` rules.
fn parse_const_rhs(rhs: &str) -> Option<Value> {
    if rhs == "true" {
        return Some(Value::Bool(true));
    }
    if rhs == "false" {
        return Some(Value::Bool(false));
    }
    if let Ok(i) = rhs.parse::<i64>() {
        return Some(Value::Int(i));
    }
    if let Ok(f) = rhs.parse::<f64>() {
        // Reject non-finite "numbers" (`inf`, `nan`) as constants: a logged
        // channel literal is never infinite/NaN, and treating those words as
        // numeric would silently swallow what was almost certainly a typo'd
        // expression. They fall through to the Expr arm, which fails loud later.
        if f.is_finite() {
            return Some(Value::Float(f));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn const_integer_rhs_is_int_const() {
        // A bare integer literal -> Const(Value::Int), channel verbatim.
        let ov = Override::parse("Root.CF.Sensor=5").expect("parses");
        assert_eq!(
            ov,
            Override::Const {
                channel: "Root.CF.Sensor".to_string(),
                value: Value::Int(5),
            }
        );
        assert_eq!(ov.channel(), "Root.CF.Sensor");
    }

    #[test]
    fn const_float_rhs_is_float_const() {
        // A literal with a decimal point -> Const(Value::Float).
        let ov = Override::parse("Root.CF.Sensor=5.0").expect("parses");
        assert_eq!(
            ov,
            Override::Const {
                channel: "Root.CF.Sensor".to_string(),
                value: Value::Float(5.0),
            }
        );
    }

    #[test]
    fn negative_and_scientific_numbers_are_const() {
        assert_eq!(
            Override::parse("X=-3").unwrap(),
            Override::Const {
                channel: "X".to_string(),
                value: Value::Int(-3),
            }
        );
        assert_eq!(
            Override::parse("X=1.5e3").unwrap(),
            Override::Const {
                channel: "X".to_string(),
                value: Value::Float(1500.0),
            }
        );
    }

    #[test]
    fn bool_rhs_is_bool_const() {
        assert_eq!(
            Override::parse("Enabled=true").unwrap(),
            Override::Const {
                channel: "Enabled".to_string(),
                value: Value::Bool(true),
            }
        );
        assert_eq!(
            Override::parse("Enabled=false").unwrap(),
            Override::Const {
                channel: "Enabled".to_string(),
                value: Value::Bool(false),
            }
        );
    }

    #[test]
    fn channel_name_with_space_split_on_first_eq() {
        // The channel keeps its space; the split is on the first `=` only.
        let ov = Override::parse("Engine Speed=1000").expect("parses");
        assert_eq!(ov.channel(), "Engine Speed");
        assert_eq!(
            ov,
            Override::Const {
                channel: "Engine Speed".to_string(),
                value: Value::Int(1000),
            }
        );
    }

    #[test]
    fn expr_rhs_is_verbatim_source() {
        // A non-literal RHS -> Expr, source stored verbatim (trimmed).
        let ov = Override::parse("Root.CF.Sensor=Logged * 1.05").expect("parses");
        assert_eq!(
            ov,
            Override::Expr {
                channel: "Root.CF.Sensor".to_string(),
                source: "Logged * 1.05".to_string(),
            }
        );
        assert_eq!(ov.channel(), "Root.CF.Sensor");
    }

    #[test]
    fn expr_split_on_first_eq_only() {
        // Only the FIRST `=` splits: `Out` is the channel, `A = B` the source.
        let ov = Override::parse("Out=A = B").expect("parses");
        assert_eq!(
            ov,
            Override::Expr {
                channel: "Out".to_string(),
                source: "A = B".to_string(),
            }
        );
    }

    #[test]
    fn whitespace_around_channel_and_rhs_is_trimmed() {
        let ov = Override::parse("  Engine Speed  =  Logged + 10  ").expect("parses");
        assert_eq!(
            ov,
            Override::Expr {
                channel: "Engine Speed".to_string(),
                source: "Logged + 10".to_string(),
            }
        );
    }

    #[test]
    fn no_equals_fails_loud() {
        match Override::parse("nope") {
            Err(EvalError::UnsupportedConstruct { .. }) => {}
            other => panic!("expected UnsupportedConstruct for missing `=`, got {other:?}"),
        }
    }

    #[test]
    fn empty_channel_fails_loud() {
        match Override::parse("=5") {
            Err(EvalError::UnsupportedConstruct { .. }) => {}
            other => panic!("expected UnsupportedConstruct for empty channel, got {other:?}"),
        }
    }

    #[test]
    fn empty_rhs_fails_loud() {
        match Override::parse("Channel=") {
            Err(EvalError::UnsupportedConstruct { .. }) => {}
            other => panic!("expected UnsupportedConstruct for empty RHS, got {other:?}"),
        }
    }

    #[test]
    fn non_finite_literal_words_fall_through_to_expr() {
        // `inf`/`nan` are not valid constant channel values; they are held as
        // expression source (and would fail loud at eval time), never a Const.
        let ov = Override::parse("X=inf").expect("parses");
        assert!(matches!(ov, Override::Expr { .. }));
        let ov = Override::parse("X=NaN").expect("parses");
        assert!(matches!(ov, Override::Expr { .. }));
    }
}
