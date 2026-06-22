// SPDX-License-Identifier: GPL-3.0-or-later
//! The expression evaluator: walks an `m1-core` CST expression [`Node`] and
//! produces a runtime [`Value`].
//!
//! Covered here (milestone M4):
//!
//! - literals (`Number`, `Boolean`/`True`/`False`, `String`),
//! - identifiers and dotted paths (channels/parameters/constants/locals),
//! - member expressions (`A.B`, `This.X`, `In.Param`),
//! - parentheses,
//! - unary (`- ! ~ not`) and binary (`+ - * / %`, comparisons, `eq`/`neq`,
//!   `and`/`or`, bitwise/shift) operators,
//! - the ternary `c ? a : b`,
//! - and the call-dispatch entry point for `Object.Method(args)` builtins.
//!
//! Value reads are **fail-loud**: an unset channel is a [`EvalError::MissingInput`],
//! a missing parameter/constant value a [`EvalError::MissingCalibration`], and an
//! unresolved name a [`EvalError::UnresolvedSymbol`] — never a guessed number.
//!
//! Identifier paths may contain spaces (`Cooling Fan`); we only ever split paths
//! on `.`, never on whitespace.

use crate::calib::Calibration;
use crate::env::{CallSite, Env, StateStore};
use crate::error::EvalError;
use crate::ident::{Target, classify};
use crate::value::Value;
use m1_core::{Field, Kind, Node};
use m1_typecheck::Project;
use m1_typecheck::symbols::SymbolKind;
use m1_typecheck::types::{ValueType, numeric_join, type_of_number_literal};

/// Everything an expression needs to evaluate against: the typed project model,
/// the calibration values, the mutable value/state stores, the lexical context
/// (enclosing group, backing function symbol, script name), and the tick `dt`.
///
/// The per-expression value sink (the `Trace`) and user-function call wiring are
/// later milestones; M4 carries only what literals/identifiers/operators/calls
/// need. The borrow of `project`/`calib` is shared; `env`/`state` are exclusive.
pub struct EvalCtx<'a> {
    /// The typed symbol model (for name resolution and symbol kinds).
    pub project: &'a Project,
    /// Calibration values (parameter scalars + table cells).
    pub calib: &'a Calibration,
    /// The runtime value store (channels/parameters/locals/statics).
    pub env: &'a mut Env,
    /// Per-call-site state for stateful builtins (M6).
    pub state: &'a mut StateStore,
    /// Canonical path of the enclosing group, for group-relative resolution.
    pub group: Option<&'a str>,
    /// Canonical path of the `Function`/`Method` symbol the script backs, for
    /// `In.<Param>` resolution.
    pub fn_symbol: Option<&'a str>,
    /// The current script's file name, for [`CallSite`] identity.
    pub script_name: &'a str,
    /// The tick step in seconds (stateful operators advance by this).
    pub dt: f64,
}

/// Evaluate an expression node to a [`Value`].
pub fn eval(node: &Node, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    match node.kind() {
        Kind::Number => eval_number(node),
        Kind::Boolean | Kind::True | Kind::False => eval_boolean(node),
        Kind::String => Ok(Value::Str(strip_quotes(node.text()).to_string())),
        Kind::Identifier => eval_path(node.text(), node, ctx),
        Kind::MemberExpression => eval_member(node, ctx),
        Kind::ParenthesizedExpression => eval_paren(node, ctx),
        Kind::UnaryExpression => eval_unary(node, ctx),
        Kind::BinaryExpression => eval_binary(node, ctx),
        Kind::TernaryExpression => eval_ternary(node, ctx),
        Kind::CallExpression => eval_call(node, ctx),
        other => Err(EvalError::UnsupportedConstruct {
            kind: format!("{other:?}"),
            at: node.byte_range().start,
        }),
    }
}

/// Parse a `Number` literal into the right numeric [`Value`] variant, using the
/// language's own literal-typing rule so `0xFF`/`7u` are unsigned, `2.5`/`1e3`
/// floats, and `7` an integer.
fn eval_number(node: &Node) -> Result<Value, EvalError> {
    let text = node.text().trim();
    match type_of_number_literal(text) {
        ValueType::Unsigned => parse_uint(text).map(Value::Uint),
        ValueType::Float => text
            .parse::<f64>()
            .map(Value::Float)
            .map_err(|_| bad_number(text)),
        // Integer (and any Unknown fallback the literal typer never returns here).
        _ => text
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| bad_number(text)),
    }
}

/// Parse an unsigned literal: hex (`0x…`, optional trailing `u`) or a decimal
/// with an optional trailing `u`.
fn parse_uint(text: &str) -> Result<u64, EvalError> {
    let lower = text.to_ascii_lowercase();
    let body = lower.strip_suffix('u').unwrap_or(&lower);
    let parsed = if let Some(hex) = body.strip_prefix("0x") {
        u64::from_str_radix(hex, 16)
    } else {
        body.parse::<u64>()
    };
    parsed.map_err(|_| bad_number(text))
}

fn bad_number(text: &str) -> EvalError {
    EvalError::TypeError {
        detail: format!("invalid number literal {text:?}"),
    }
}

/// Evaluate a `Boolean`/`True`/`False` node.
fn eval_boolean(node: &Node) -> Result<Value, EvalError> {
    match node.text().trim() {
        "true" => Ok(Value::Bool(true)),
        "false" => Ok(Value::Bool(false)),
        other => Err(EvalError::TypeError {
            detail: format!("invalid boolean literal {other:?}"),
        }),
    }
}

/// Strip a single pair of surrounding double quotes from a string literal's text.
fn strip_quotes(text: &str) -> &str {
    text.strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(text)
}

/// Evaluate a parenthesized expression: just its single inner expression.
fn eval_paren(node: &Node, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    // The grammar gives the parenthesised expression no field; the single named
    // child is the wrapped expression.
    let inner = node.named_children().into_iter().next().ok_or_else(|| {
        EvalError::UnsupportedConstruct {
            kind: "empty parentheses".to_string(),
            at: node.byte_range().start,
        }
    })?;
    eval(&inner, ctx)
}

/// Evaluate a member expression (`A.B`, `This.X`, `In.Param`, `Parent.Y`) by
/// flattening it to a dotted path and reading that path's value. A member whose
/// head is a builtin library object (e.g. `Calculate.PI`) is not a value here —
/// the call path handles `Object.Method(...)`; a bare builtin member read is a
/// fail-loud unsupported construct.
fn eval_member(node: &Node, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    let path = flatten_member(node)?;
    eval_path(&path, node, ctx)
}

/// Flatten a `MemberExpression` to its dotted source path. The `object` may
/// itself be a member expression (`A.B.C`), so recurse; each segment is taken
/// verbatim (it may contain spaces). Only `.` joins segments — never whitespace.
fn flatten_member(node: &Node) -> Result<String, EvalError> {
    let object = node
        .child_by_field(Field::Object)
        .ok_or_else(|| member_shape_err(node))?;
    let property = node
        .child_by_field(Field::Property)
        .ok_or_else(|| member_shape_err(node))?;

    let head = match object.kind() {
        Kind::MemberExpression => flatten_member(&object)?,
        // Identifier (or any leaf) — its text is the segment verbatim.
        _ => object.text().to_string(),
    };
    Ok(format!("{head}.{}", property.text()))
}

fn member_shape_err(node: &Node) -> EvalError {
    EvalError::UnsupportedConstruct {
        kind: "malformed member expression".to_string(),
        at: node.byte_range().start,
    }
}

/// Rewrite a leading `This` anchor to the enclosing group's canonical path
/// (`This.Output` from group `Root.Demo` → `Root.Demo.Output`; bare `This` →
/// `Root.Demo`). `resolve` handles the `In`/`Out`/`Parent`/`Root` anchors itself
/// but not `This`, so we expand it here before classification. Only `.` splits
/// segments, never whitespace. Non-`This` paths are returned unchanged.
fn rewrite_this(path: &str, group: Option<&str>) -> Option<String> {
    let group = group?;
    if path == "This" {
        return Some(group.to_string());
    }
    path.strip_prefix("This.")
        .map(|rest| format!("{group}.{rest}"))
}

/// Read the value denoted by a (possibly dotted) `path`, written at `node` (used
/// only for byte-offset diagnostics). Classifies the path, then reads from the
/// appropriate store fail-loud.
fn eval_path(path: &str, node: &Node, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    // Expand a `This` anchor to the enclosing group before resolving.
    let rewritten = rewrite_this(path, ctx.group);
    let path = rewritten.as_deref().unwrap_or(path);

    let target = classify(path, ctx.group, ctx.fn_symbol, ctx.project, &ctx.env.locals);
    match target {
        Target::Local(name) => ctx
            .env
            .get_local(&name)
            .cloned()
            .ok_or(EvalError::MissingInput { channel: name }),
        Target::Symbol(canon) => read_symbol(&canon, ctx),
        // A bare builtin object read (`Calculate` on its own, or `Calculate.PI`
        // outside a call) is not a value the evaluator can produce — only
        // `Object.Method(...)` calls are. Fail loud.
        Target::Builtin { object } => Err(EvalError::UnsupportedConstruct {
            kind: format!("builtin object {object:?} used as a value"),
            at: node.byte_range().start,
        }),
        Target::Unresolved => Err(EvalError::UnresolvedSymbol {
            name: path.to_string(),
        }),
    }
}

/// Read a resolved project symbol's current value. The store depends on the
/// symbol kind: channels come from the value store (fail loud if unset), while
/// parameters/constants come from calibration (with an `Env` override taking
/// precedence). A table or group has no scalar value.
fn read_symbol(canon: &str, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    // An explicit `Env` override (a pinned channel, a previously written value)
    // always wins — that is how computed channels read back what an earlier
    // statement wrote, and how scenario inputs are seeded.
    if let Some(v) = ctx.env.get(canon) {
        return Ok(v.clone());
    }

    let kind = ctx.project.symbols().get(canon).map(|s| s.kind);
    match kind {
        Some(SymbolKind::Parameter | SymbolKind::Constant) => {
            calib_param(canon, ctx.calib).ok_or_else(|| EvalError::MissingCalibration {
                path: canon.to_string(),
            })
        }
        Some(SymbolKind::Channel) => Err(EvalError::MissingInput {
            channel: canon.to_string(),
        }),
        // Tables/groups/functions/objects are not scalar values.
        Some(_) => Err(EvalError::TypeError {
            detail: format!("symbol {canon:?} has no scalar value"),
        }),
        // Resolved to a canonical path the symbol table does not actually carry:
        // treat as unresolved rather than guess.
        None => Err(EvalError::UnresolvedSymbol {
            name: canon.to_string(),
        }),
    }
}

/// Look up a parameter/constant calibration value by its canonical symbol path.
/// Real `.m1cfg` exports omit the implicit leading `Root.` group prefix that the
/// symbol table uses, so try the canonical path first, then the `Root.`-stripped
/// form. Returns the value as a [`Value::Float`] (calibration cells are numeric).
fn calib_param(canon: &str, calib: &Calibration) -> Option<Value> {
    calib
        .param(canon)
        .or_else(|| canon.strip_prefix("Root.").and_then(|p| calib.param(p)))
        .map(Value::Float)
}

/// Evaluate a unary expression (`- ! ~ not`).
fn eval_unary(node: &Node, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    let op = node
        .child_by_field(Field::Operator)
        .ok_or_else(|| op_shape_err(node, "unary"))?;
    // The operand is the single named child that is not the operator token.
    let operand = node
        .named_children()
        .into_iter()
        .find(|c| c.byte_range() != op.byte_range())
        .ok_or_else(|| op_shape_err(node, "unary operand"))?;
    let v = eval(&operand, ctx)?;
    match op.kind() {
        Kind::Minus => match v {
            Value::Int(x) => Ok(Value::Int(-x)),
            Value::Float(x) => Ok(Value::Float(-x)),
            // Negating an unsigned yields a signed result.
            Value::Uint(x) => Ok(Value::Int(-(x as i64))),
            other => Err(EvalError::TypeError {
                detail: format!("cannot negate {other:?}"),
            }),
        },
        // `not` and `!` are logical negation: boolean only (M1 is strongly typed).
        Kind::Not | Kind::Bang => Ok(Value::Bool(!v.as_bool()?)),
        // `~` is bitwise complement: integral only.
        Kind::Tilde => match v {
            Value::Int(x) => Ok(Value::Int(!x)),
            Value::Uint(x) => Ok(Value::Uint(!x)),
            other => Err(EvalError::TypeError {
                detail: format!("cannot bitwise-complement {other:?}"),
            }),
        },
        other => Err(EvalError::UnsupportedConstruct {
            kind: format!("unary operator {other:?}"),
            at: op.byte_range().start,
        }),
    }
}

/// Evaluate a binary expression. Short-circuits `and`/`or`; otherwise evaluates
/// both operands then applies the operator.
fn eval_binary(node: &Node, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    let op = node
        .child_by_field(Field::Operator)
        .ok_or_else(|| op_shape_err(node, "binary"))?;
    let left = node
        .child_by_field(Field::Left)
        .ok_or_else(|| op_shape_err(node, "binary left"))?;
    let right = node
        .child_by_field(Field::Right)
        .ok_or_else(|| op_shape_err(node, "binary right"))?;

    let kind = op.kind();

    // Short-circuit logical operators: evaluate the right operand only when the
    // left does not already decide the result. Operands must be boolean.
    match kind {
        Kind::And | Kind::AmpAmp => {
            let l = eval(&left, ctx)?.as_bool()?;
            if !l {
                return Ok(Value::Bool(false));
            }
            return Ok(Value::Bool(eval(&right, ctx)?.as_bool()?));
        }
        Kind::Or | Kind::PipePipe => {
            let l = eval(&left, ctx)?.as_bool()?;
            if l {
                return Ok(Value::Bool(true));
            }
            return Ok(Value::Bool(eval(&right, ctx)?.as_bool()?));
        }
        _ => {}
    }

    let l = eval(&left, ctx)?;
    let r = eval(&right, ctx)?;

    match kind {
        // Arithmetic.
        Kind::Plus | Kind::Minus | Kind::Star | Kind::Slash | Kind::Percent => {
            arithmetic(kind, &l, &r)
        }
        // Ordered comparisons.
        Kind::Lt | Kind::Gt | Kind::LtEq | Kind::GtEq => compare(kind, &l, &r),
        // Equality (`eq`/`==` and `neq`/`!=`), including enum equality by member.
        Kind::Eq | Kind::EqEq => Ok(Value::Bool(values_equal(&l, &r)?)),
        Kind::Neq | Kind::BangEq => Ok(Value::Bool(!values_equal(&l, &r)?)),
        // Bitwise / shift (integral only).
        Kind::Amp | Kind::Pipe | Kind::Caret | Kind::LtLt | Kind::GtGt => bitwise(kind, &l, &r),
        other => Err(EvalError::UnsupportedConstruct {
            kind: format!("binary operator {other:?}"),
            at: op.byte_range().start,
        }),
    }
}

/// Apply an arithmetic operator. Integer/unsigned operands stay integral (with
/// the result kind chosen by `numeric_join`); any float operand promotes to
/// float. Division/modulo by zero fail loud rather than producing NaN/inf.
fn arithmetic(op: Kind, l: &Value, r: &Value) -> Result<Value, EvalError> {
    let lt = value_type(l);
    let rt = value_type(r);
    let joined = numeric_join(lt, rt);

    match joined {
        ValueType::Float => {
            let a = l.as_f64()?;
            let b = r.as_f64()?;
            let out = match op {
                Kind::Plus => a + b,
                Kind::Minus => a - b,
                Kind::Star => a * b,
                Kind::Slash => a / b,
                Kind::Percent => a % b,
                _ => unreachable!("arithmetic called with non-arith op"),
            };
            Ok(Value::Float(out))
        }
        ValueType::Unsigned => {
            let a = as_u64(l)?;
            let b = as_u64(r)?;
            int_op_u64(op, a, b)
        }
        ValueType::Integer => {
            let a = as_i64(l)?;
            let b = as_i64(r)?;
            int_op_i64(op, a, b)
        }
        // One operand is non-numeric (Bool/Enum/String) or Unknown.
        _ => Err(EvalError::TypeError {
            detail: format!("arithmetic on non-numeric operands {l:?} and {r:?}"),
        }),
    }
}

fn int_op_i64(op: Kind, a: i64, b: i64) -> Result<Value, EvalError> {
    let out = match op {
        Kind::Plus => a.wrapping_add(b),
        Kind::Minus => a.wrapping_sub(b),
        Kind::Star => a.wrapping_mul(b),
        Kind::Slash => {
            if b == 0 {
                return Err(div_by_zero());
            }
            a.wrapping_div(b)
        }
        Kind::Percent => {
            if b == 0 {
                return Err(div_by_zero());
            }
            a.wrapping_rem(b)
        }
        _ => unreachable!(),
    };
    Ok(Value::Int(out))
}

fn int_op_u64(op: Kind, a: u64, b: u64) -> Result<Value, EvalError> {
    let out = match op {
        Kind::Plus => a.wrapping_add(b),
        Kind::Minus => a.wrapping_sub(b),
        Kind::Star => a.wrapping_mul(b),
        Kind::Slash => {
            if b == 0 {
                return Err(div_by_zero());
            }
            a.wrapping_div(b)
        }
        Kind::Percent => {
            if b == 0 {
                return Err(div_by_zero());
            }
            a.wrapping_rem(b)
        }
        _ => unreachable!(),
    };
    Ok(Value::Uint(out))
}

fn div_by_zero() -> EvalError {
    EvalError::TypeError {
        detail: "division or modulo by zero".to_string(),
    }
}

/// Apply an ordered comparison (`< > <= >=`). Numeric operands compare as `f64`;
/// non-numeric operands are a type error.
fn compare(op: Kind, l: &Value, r: &Value) -> Result<Value, EvalError> {
    let a = l.as_f64()?;
    let b = r.as_f64()?;
    let out = match op {
        Kind::Lt => a < b,
        Kind::Gt => a > b,
        Kind::LtEq => a <= b,
        Kind::GtEq => a >= b,
        _ => unreachable!("compare called with non-comparison op"),
    };
    Ok(Value::Bool(out))
}

/// Structural equality for the `eq`/`==` (and negated `neq`/`!=`) operators.
///
/// Numbers compare by value across int/uint/float; enums compare by `(id,
/// member)`; booleans and strings compare directly. Comparing fundamentally
/// different kinds (e.g. a number with a string, or an enum with a number) is a
/// type error rather than silently `false`.
fn values_equal(l: &Value, r: &Value) -> Result<bool, EvalError> {
    use Value::*;
    match (l, r) {
        (Bool(a), Bool(b)) => Ok(a == b),
        (Str(a), Str(b)) => Ok(a == b),
        (Enum { id: i1, member: m1 }, Enum { id: i2, member: m2 }) => Ok(i1 == i2 && m1 == m2),
        // Any numeric pairing compares by f64 value.
        (Int(_) | Uint(_) | Float(_), Int(_) | Uint(_) | Float(_)) => Ok(l.as_f64()? == r.as_f64()?),
        _ => Err(EvalError::TypeError {
            detail: format!("cannot compare {l:?} with {r:?} for equality"),
        }),
    }
}

/// Apply a bitwise/shift operator. Both operands must be the same integral
/// family (both signed or both unsigned); mixing or non-integral operands is a
/// type error.
fn bitwise(op: Kind, l: &Value, r: &Value) -> Result<Value, EvalError> {
    match (l, r) {
        (Value::Uint(a), Value::Uint(b)) => Ok(Value::Uint(bit_u64(op, *a, *b))),
        (Value::Int(a), Value::Int(b)) => Ok(Value::Int(bit_i64(op, *a, *b))),
        _ => Err(EvalError::TypeError {
            detail: format!("bitwise operator requires matching integral operands, got {l:?} and {r:?}"),
        }),
    }
}

fn bit_u64(op: Kind, a: u64, b: u64) -> u64 {
    match op {
        Kind::Amp => a & b,
        Kind::Pipe => a | b,
        Kind::Caret => a ^ b,
        Kind::LtLt => a.wrapping_shl(b as u32),
        Kind::GtGt => a.wrapping_shr(b as u32),
        _ => unreachable!("bit_u64 called with non-bitwise op"),
    }
}

fn bit_i64(op: Kind, a: i64, b: i64) -> i64 {
    match op {
        Kind::Amp => a & b,
        Kind::Pipe => a | b,
        Kind::Caret => a ^ b,
        Kind::LtLt => a.wrapping_shl(b as u32),
        Kind::GtGt => a.wrapping_shr(b as u32),
        _ => unreachable!("bit_i64 called with non-bitwise op"),
    }
}

/// The [`ValueType`] of a runtime value, for `numeric_join`-driven arithmetic
/// result typing. Non-numeric values map to their lattice type.
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

/// Coerce to `i64` for integer arithmetic; an unsigned value fits via `as`.
fn as_i64(v: &Value) -> Result<i64, EvalError> {
    match v {
        Value::Int(x) => Ok(*x),
        Value::Uint(x) => Ok(*x as i64),
        other => Err(EvalError::TypeError {
            detail: format!("{other:?} is not an integer"),
        }),
    }
}

/// Coerce to `u64` for unsigned arithmetic.
fn as_u64(v: &Value) -> Result<u64, EvalError> {
    match v {
        Value::Uint(x) => Ok(*x),
        Value::Int(x) => Ok(*x as u64),
        other => Err(EvalError::TypeError {
            detail: format!("{other:?} is not an unsigned integer"),
        }),
    }
}

/// Evaluate a ternary `condition ? consequence : alternative`. The condition
/// must be boolean (no truthiness on numbers); the chosen branch is evaluated,
/// the other is not.
fn eval_ternary(node: &Node, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    let cond = node
        .child_by_field(Field::Condition)
        .ok_or_else(|| op_shape_err(node, "ternary condition"))?;
    let conseq = node
        .child_by_field(Field::Consequence)
        .ok_or_else(|| op_shape_err(node, "ternary consequence"))?;
    let alt = node
        .child_by_field(Field::Alternative)
        .ok_or_else(|| op_shape_err(node, "ternary alternative"))?;

    if eval(&cond, ctx)?.as_bool()? {
        eval(&conseq, ctx)
    } else {
        eval(&alt, ctx)
    }
}

/// Evaluate a call expression `Object.Method(args)`. The callee must be a member
/// expression naming a builtin object; its arguments are evaluated left to right
/// and dispatched through [`crate::builtins::dispatch`] with the call's stable
/// [`CallSite`]. A call to a user function/method is out of the Phase-1 cone
/// scope and fails loud as an unsupported construct.
fn eval_call(node: &Node, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    let callee = node
        .child_by_field(Field::Function)
        .ok_or_else(|| op_shape_err(node, "call function"))?;

    // The call site keys stateful operator state across ticks (M6): the script
    // name plus the byte offset of the whole call node.
    let site = CallSite::of(ctx.script_name, node);

    // Evaluate the arguments left to right.
    let mut args = Vec::new();
    if let Some(arglist) = node.child_by_field(Field::Arguments) {
        for arg in arglist.named_children() {
            args.push(eval(&arg, ctx)?);
        }
    }

    match callee.kind() {
        Kind::MemberExpression => {
            let object_node = callee
                .child_by_field(Field::Object)
                .ok_or_else(|| op_shape_err(&callee, "call object"))?;
            let method_node = callee
                .child_by_field(Field::Property)
                .ok_or_else(|| op_shape_err(&callee, "call method"))?;
            let method = method_node.text();

            // A call whose object is a single builtin-library identifier
            // (`Calculate`, `Limit`, …) dispatches as a builtin. Anything else —
            // a method on a project symbol (table `.Lookup`, object accessors)
            // or a deeper member object — is handled by later milestones (table
            // lookup in M5, stateful operators in M6); for M4 they go through
            // dispatch too and fail loud as unsupported.
            let object = match object_node.kind() {
                Kind::MemberExpression => flatten_member(&object_node)?,
                _ => object_node.text().to_string(),
            };
            crate::builtins::dispatch(&object, method, &args, site, ctx)
        }
        // A bare-identifier callee is a user-function call — out of the M4 cone.
        _ => Err(EvalError::UnsupportedConstruct {
            kind: "user-function call (out of Phase-1 cone scope)".to_string(),
            at: node.byte_range().start,
        }),
    }
}

fn op_shape_err(node: &Node, what: &str) -> EvalError {
    EvalError::UnsupportedConstruct {
        kind: format!("malformed {what}"),
        at: node.byte_range().start,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use m1_core::parse;
    use std::path::Path;

    /// Load the synthetic mini fixture project for resolution-backed tests.
    fn mini_project() -> Project {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini");
        crate::loader::load(&dir.join("Project.m1prj"), None)
            .expect("mini fixture loads")
            .project
    }

    /// Build a throwaway `EvalCtx` over fresh stores. `group`/`fn_symbol` default
    /// to the demo function so group-relative names resolve.
    struct Harness {
        project: Project,
        calib: Calibration,
        env: Env,
        state: StateStore,
    }

    impl Harness {
        fn new() -> Harness {
            Harness {
                project: mini_project(),
                calib: Calibration::default(),
                env: Env::new(),
                state: StateStore::new(),
            }
        }

        fn ctx(&mut self) -> EvalCtx<'_> {
            EvalCtx {
                project: &self.project,
                calib: &self.calib,
                env: &mut self.env,
                state: &mut self.state,
                group: Some("Root.Demo"),
                fn_symbol: Some("Root.Demo.Update"),
                script_name: "Demo.Update.m1scr",
                dt: 0.01,
            }
        }
    }

    /// Parse `x = <expr>;` and return the value-expression node's owning Cst plus
    /// a way to locate it. Returns the parsed Cst; the caller pulls the rhs.
    fn rhs_value(src_expr: &str, h: &mut Harness) -> Result<Value, EvalError> {
        let src = format!("x = {src_expr};\n");
        let cst = parse(&src);
        let assign = cst.root().children().into_iter().next().unwrap();
        // The value-side expression is the second named child (after the target).
        let rhs = assign.named_children().into_iter().nth(1).unwrap();
        let mut ctx = h.ctx();
        eval(&rhs, &mut ctx)
    }

    // ---- Task 8: literals, identifiers, parentheses ----

    #[test]
    fn number_literals_pick_the_right_variant() {
        let mut h = Harness::new();
        assert_eq!(rhs_value("2.5", &mut h).unwrap(), Value::Float(2.5));
        assert_eq!(rhs_value("7", &mut h).unwrap(), Value::Int(7));
        assert_eq!(rhs_value("0xFF", &mut h).unwrap(), Value::Uint(255));
        assert_eq!(rhs_value("10u", &mut h).unwrap(), Value::Uint(10));
        assert_eq!(rhs_value("1e3", &mut h).unwrap(), Value::Float(1000.0));
    }

    #[test]
    fn boolean_and_string_literals() {
        let mut h = Harness::new();
        assert_eq!(rhs_value("true", &mut h).unwrap(), Value::Bool(true));
        assert_eq!(rhs_value("false", &mut h).unwrap(), Value::Bool(false));
        assert_eq!(
            rhs_value("\"hello\"", &mut h).unwrap(),
            Value::Str("hello".to_string())
        );
    }

    #[test]
    fn parentheses_pass_through() {
        let mut h = Harness::new();
        assert_eq!(rhs_value("(2.5)", &mut h).unwrap(), Value::Float(2.5));
    }

    #[test]
    fn channel_identifier_reads_env_or_fails_loud() {
        let mut h = Harness::new();
        // Unset channel: fail loud with MissingInput.
        match rhs_value("Speed", &mut h) {
            Err(EvalError::MissingInput { channel }) => assert_eq!(channel, "Root.Demo.Speed"),
            other => panic!("expected MissingInput, got {other:?}"),
        }
        // Seed the channel; now it reads back.
        h.env.set("Root.Demo.Speed", Value::Float(42.0));
        assert_eq!(rhs_value("Speed", &mut h).unwrap(), Value::Float(42.0));
    }

    #[test]
    fn parameter_identifier_reads_calibration() {
        let mut h = Harness::new();
        // No calibration value -> fail loud.
        match rhs_value("Gain", &mut h) {
            Err(EvalError::MissingCalibration { path }) => assert_eq!(path, "Root.Demo.Gain"),
            other => panic!("expected MissingCalibration, got {other:?}"),
        }
        // Provide it under the Root-stripped name real exports use.
        h.calib.params.insert("Demo.Gain".to_string(), 2.5);
        assert_eq!(rhs_value("Gain", &mut h).unwrap(), Value::Float(2.5));
    }

    #[test]
    fn unresolved_identifier_fails_loud() {
        let mut h = Harness::new();
        match rhs_value("NoSuchThing", &mut h) {
            Err(EvalError::UnresolvedSymbol { name }) => assert_eq!(name, "NoSuchThing"),
            other => panic!("expected UnresolvedSymbol, got {other:?}"),
        }
    }

    #[test]
    fn local_identifier_reads_local_store() {
        let mut h = Harness::new();
        h.env.set_local("scaled", Value::Int(9));
        assert_eq!(rhs_value("scaled", &mut h).unwrap(), Value::Int(9));
    }

    // ---- Task 9: member expressions ----

    #[test]
    fn this_member_rewrites_to_group() {
        let mut h = Harness::new();
        // `This.Output` from group Root.Demo resolves to Root.Demo.Output.
        h.env.set("Root.Demo.Output", Value::Float(3.0));
        assert_eq!(rhs_value("This.Output", &mut h).unwrap(), Value::Float(3.0));
    }

    #[test]
    fn absolute_member_path_reads() {
        let mut h = Harness::new();
        h.env.set("Root.Sibling", Value::Float(11.0));
        assert_eq!(
            rhs_value("Root.Sibling", &mut h).unwrap(),
            Value::Float(11.0)
        );
    }

    #[test]
    fn parent_member_walks_up() {
        let mut h = Harness::new();
        h.env.set("Root.Sibling", Value::Float(5.0));
        // From Root.Demo, Parent.Sibling is Root.Sibling.
        assert_eq!(
            rhs_value("Parent.Sibling", &mut h).unwrap(),
            Value::Float(5.0)
        );
    }

    #[test]
    fn builtin_member_as_value_fails_loud() {
        let mut h = Harness::new();
        // `Calculate.PI` read as a value (not called) is unsupported in M4.
        match rhs_value("Calculate.PI", &mut h) {
            Err(EvalError::UnsupportedConstruct { .. }) => {}
            other => panic!("expected UnsupportedConstruct, got {other:?}"),
        }
    }

    // ---- Task 10: unary & binary operators ----

    #[test]
    fn arithmetic_int_and_float() {
        let mut h = Harness::new();
        assert_eq!(rhs_value("2 + 3", &mut h).unwrap(), Value::Int(5));
        assert_eq!(rhs_value("2 * 3", &mut h).unwrap(), Value::Int(6));
        assert_eq!(rhs_value("7 % 3", &mut h).unwrap(), Value::Int(1));
        // A float operand promotes the result to float (numeric_join).
        assert_eq!(rhs_value("2 + 1.5", &mut h).unwrap(), Value::Float(3.5));
        assert_eq!(rhs_value("3.0 / 2.0", &mut h).unwrap(), Value::Float(1.5));
    }

    #[test]
    fn unsigned_arithmetic_stays_unsigned() {
        let mut h = Harness::new();
        assert_eq!(rhs_value("10u + 5u", &mut h).unwrap(), Value::Uint(15));
    }

    #[test]
    fn division_by_zero_fails_loud() {
        let mut h = Harness::new();
        assert!(matches!(
            rhs_value("1 / 0", &mut h),
            Err(EvalError::TypeError { .. })
        ));
    }

    #[test]
    fn comparisons() {
        let mut h = Harness::new();
        assert_eq!(rhs_value("3 > 2", &mut h).unwrap(), Value::Bool(true));
        assert_eq!(rhs_value("2 >= 2", &mut h).unwrap(), Value::Bool(true));
        assert_eq!(rhs_value("1 < 0", &mut h).unwrap(), Value::Bool(false));
        assert_eq!(rhs_value("2.0 <= 1.5", &mut h).unwrap(), Value::Bool(false));
    }

    #[test]
    fn equality_keyword_and_symbolic() {
        let mut h = Harness::new();
        assert_eq!(rhs_value("2 eq 2", &mut h).unwrap(), Value::Bool(true));
        assert_eq!(rhs_value("2 == 3", &mut h).unwrap(), Value::Bool(false));
        assert_eq!(rhs_value("2 neq 3", &mut h).unwrap(), Value::Bool(true));
        assert_eq!(rhs_value("2 != 2", &mut h).unwrap(), Value::Bool(false));
    }

    #[test]
    fn enum_equality_by_member() {
        // Enum equality is direct on the runtime value (no project enum needed).
        let a = Value::Enum {
            id: 3,
            member: "On".to_string(),
        };
        let b = Value::Enum {
            id: 3,
            member: "On".to_string(),
        };
        let c = Value::Enum {
            id: 3,
            member: "Off".to_string(),
        };
        assert!(values_equal(&a, &b).unwrap());
        assert!(!values_equal(&a, &c).unwrap());
    }

    #[test]
    fn logical_operators_short_circuit() {
        let mut h = Harness::new();
        assert_eq!(
            rhs_value("true and false", &mut h).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            rhs_value("false or true", &mut h).unwrap(),
            Value::Bool(true)
        );
        // Short-circuit: the right operand of `false and X` is never evaluated,
        // so an undefined channel there does not error.
        assert_eq!(
            rhs_value("false and Speed", &mut h).unwrap(),
            Value::Bool(false)
        );
        // Likewise `true or X`.
        assert_eq!(
            rhs_value("true or Speed", &mut h).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn logical_on_non_bool_fails_loud() {
        let mut h = Harness::new();
        assert!(matches!(
            rhs_value("1 and 2", &mut h),
            Err(EvalError::TypeError { .. })
        ));
    }

    #[test]
    fn unary_operators() {
        let mut h = Harness::new();
        assert_eq!(rhs_value("-5", &mut h).unwrap(), Value::Int(-5));
        assert_eq!(rhs_value("-2.5", &mut h).unwrap(), Value::Float(-2.5));
        assert_eq!(rhs_value("not true", &mut h).unwrap(), Value::Bool(false));
        assert_eq!(rhs_value("!false", &mut h).unwrap(), Value::Bool(true));
    }

    #[test]
    fn bitwise_and_shift() {
        let mut h = Harness::new();
        assert_eq!(rhs_value("12u & 10u", &mut h).unwrap(), Value::Uint(8));
        assert_eq!(rhs_value("12u | 1u", &mut h).unwrap(), Value::Uint(13));
        assert_eq!(rhs_value("6u ^ 3u", &mut h).unwrap(), Value::Uint(5));
        assert_eq!(rhs_value("1u << 4u", &mut h).unwrap(), Value::Uint(16));
        assert_eq!(rhs_value("16u >> 2u", &mut h).unwrap(), Value::Uint(4));
        assert_eq!(rhs_value("~0u", &mut h).unwrap(), Value::Uint(u64::MAX));
    }

    #[test]
    fn bitwise_on_float_fails_loud() {
        let mut h = Harness::new();
        assert!(matches!(
            rhs_value("1.0 & 2u", &mut h),
            Err(EvalError::TypeError { .. })
        ));
    }

    #[test]
    fn operator_precedence_via_grammar() {
        let mut h = Harness::new();
        // 2 + 3 * 4 = 14 (the grammar nests the multiply tighter).
        assert_eq!(rhs_value("2 + 3 * 4", &mut h).unwrap(), Value::Int(14));
        // Parentheses override: (2 + 3) * 4 = 20.
        assert_eq!(rhs_value("(2 + 3) * 4", &mut h).unwrap(), Value::Int(20));
    }

    // ---- Task 11: ternary + call dispatch ----

    #[test]
    fn ternary_picks_branch() {
        let mut h = Harness::new();
        assert_eq!(rhs_value("true ? 1 : 2", &mut h).unwrap(), Value::Int(1));
        assert_eq!(rhs_value("false ? 1 : 2", &mut h).unwrap(), Value::Int(2));
        // The non-taken branch is not evaluated: an undefined channel there is fine.
        assert_eq!(
            rhs_value("true ? 7 : Speed", &mut h).unwrap(),
            Value::Int(7)
        );
    }

    #[test]
    fn ternary_non_bool_condition_fails_loud() {
        let mut h = Harness::new();
        assert!(matches!(
            rhs_value("1 ? 2 : 3", &mut h),
            Err(EvalError::TypeError { .. })
        ));
    }

    #[test]
    fn pure_builtin_call_dispatches_and_evaluates() {
        let mut h = Harness::new();
        // M5 wires the pure builtins: Calculate.Max(2, 3) dispatches through
        // builtins::dispatch and computes a real value (3).
        assert_eq!(
            rhs_value("Calculate.Max(2, 3)", &mut h).unwrap(),
            Value::Int(3)
        );
    }

    #[test]
    fn unimplemented_builtin_call_still_fails_loud() {
        let mut h = Harness::new();
        // A stateful builtin (Filter.FirstOrder, M6) is not implemented yet, so a
        // call to it must fail loud rather than no-op.
        match rhs_value("Filter.FirstOrder(1.0, 0.1)", &mut h) {
            Err(EvalError::UnsupportedBuiltin { object, method }) => {
                assert_eq!(object, "Filter");
                assert_eq!(method, "FirstOrder");
            }
            other => panic!("expected UnsupportedBuiltin, got {other:?}"),
        }
    }

    #[test]
    fn builtin_call_evaluates_args_before_dispatch() {
        let mut h = Harness::new();
        // An argument that itself fails to evaluate surfaces before dispatch:
        // here a bad arithmetic (1/0) errors during argument evaluation.
        match rhs_value("Calculate.Max(1 / 0, 3)", &mut h) {
            Err(EvalError::TypeError { .. }) => {}
            other => panic!("expected argument-eval TypeError, got {other:?}"),
        }
    }

    #[test]
    fn user_function_call_is_out_of_scope() {
        let mut h = Harness::new();
        // A bare-identifier callee is a user function — out of the Phase-1 cone.
        match rhs_value("SomeUserFunc(1)", &mut h) {
            Err(EvalError::UnsupportedConstruct { .. }) => {}
            other => panic!("expected UnsupportedConstruct, got {other:?}"),
        }
    }
}
