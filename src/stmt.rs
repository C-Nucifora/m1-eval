// SPDX-License-Identifier: GPL-3.0-or-later
//! The statement executor: walks an `m1-core` CST statement [`Node`] and applies
//! its effect to the evaluation context.
//!
//! Covered here (milestone M7):
//!
//! - [`AssignmentStatement`](m1_core::Kind::AssignmentStatement), including the
//!   compound read-then-write forms (`+= -= *= /= %= &= |= ^= <<= >>=`),
//! - [`ExpressionStatement`](m1_core::Kind::ExpressionStatement) — evaluated for
//!   its side effects (e.g. a Tier-3 IO call),
//! - [`IfStatement`](m1_core::Kind::IfStatement) with `else`/`else if` chains,
//! - [`WhenStatement`](m1_core::Kind::WhenStatement) over
//!   [`IsClause`](m1_core::Kind::IsClause)s matched against the subject's value,
//! - [`ExpandStatement`](m1_core::Kind::ExpandStatement) — the compile-time
//!   `Start..End` loop binding `$(VAR)` interpolations,
//! - [`LocalDeclaration`](m1_core::Kind::LocalDeclaration) — `local` and
//!   `static local` variables,
//! - [`Block`](m1_core::Kind::Block) — a sequence of statements, and
//! - [`exec_script`], which walks a `SourceFile`'s top-level statements.
//!
//! Execution is **fail-loud**: an unsupported statement kind, an unresolved write
//! target, or any failing sub-expression surfaces as an [`EvalError`] — a
//! statement never silently no-ops or guesses.
//!
//! ## Where a write lands
//!
//! An assignment target is either a function-local variable (a bare name that is
//! a known local), a `static local`, or a project channel/parameter addressed by
//! its canonical path. We classify the target path exactly as the expression
//! evaluator classifies a read, so `This.Output`, `Output`, and
//! `Root.Demo.Output` all address the same store slot. Identifier paths may
//! contain spaces; we only ever split on `.`.

use crate::error::EvalError;
use crate::expr::{self, EvalCtx, eval};
use crate::ident::{Target, classify};
use crate::value::Value;
use m1_core::{Field, Kind, Node};
use m1_typecheck::types::ValueType;
use m1_typecheck::types::declared_local_type;

/// Execute one statement node, applying its effect to `ctx`.
pub fn exec(node: &Node, ctx: &mut EvalCtx) -> Result<(), EvalError> {
    match node.kind() {
        Kind::AssignmentStatement => exec_assignment(node, ctx),
        Kind::ExpressionStatement => exec_expression_statement(node, ctx),
        Kind::LocalDeclaration => exec_local_declaration(node, ctx),
        Kind::IfStatement => exec_if(node, ctx),
        Kind::WhenStatement => exec_when(node, ctx),
        Kind::ExpandStatement => exec_expand(node, ctx),
        Kind::Block => exec_block(node, ctx),
        // A bare `;` is a no-op.
        Kind::EmptyStatement => Ok(()),
        // Comments and the like are not statements; ignore non-statement trivia
        // when a caller hands them to us, but fail loud on a genuine unhandled
        // statement kind so nothing is silently skipped.
        other => Err(EvalError::UnsupportedConstruct {
            kind: format!("statement {other:?}"),
            at: node.byte_range().start,
        }),
    }
}

/// Walk a `SourceFile` root, executing each top-level statement in source order.
/// Non-statement trivia (comments) between statements is skipped; every real
/// statement is executed. A failure in any statement aborts the script fail-loud.
pub fn exec_script(root: &Node, ctx: &mut EvalCtx) -> Result<(), EvalError> {
    for child in root.children() {
        if is_statement(child.kind()) {
            exec(&child, ctx)?;
        }
    }
    Ok(())
}

/// Whether a node kind is one of the executable statement forms (so the script
/// and block walkers can skip braces, comments, and other non-statement trivia).
fn is_statement(kind: Kind) -> bool {
    matches!(
        kind,
        Kind::AssignmentStatement
            | Kind::ExpressionStatement
            | Kind::LocalDeclaration
            | Kind::IfStatement
            | Kind::WhenStatement
            | Kind::ExpandStatement
            | Kind::Block
            | Kind::EmptyStatement
    )
}

// ---- Task 20: assignment (incl. compound) + expression statement ----

/// Execute an assignment. The `target` field is the write destination (a local,
/// a `static local`, or a project channel/parameter path); the `value` field is
/// the right-hand-side expression. A compound operator (`+=`, …) reads the
/// current value, applies the operator, and writes the result back.
fn exec_assignment(node: &Node, ctx: &mut EvalCtx) -> Result<(), EvalError> {
    let target = node
        .child_by_field(Field::Target)
        .ok_or_else(|| shape_err(node, "assignment target"))?;
    let value_node = node
        .child_by_field(Field::Value)
        .ok_or_else(|| shape_err(node, "assignment value"))?;
    let op = node
        .child_by_field(Field::Operator)
        .ok_or_else(|| shape_err(node, "assignment operator"))?;

    // Resolve where this assignment writes (and how to read it back for compound
    // forms): a local slot, a static-local slot, or a canonical channel path.
    let dest = resolve_target(&target, ctx)?;

    let rhs = eval(&value_node, ctx)?;

    // Plain `=` writes the rhs directly. A compound `op=` reads the current value
    // first, applies the corresponding binary operator, then writes the result.
    let final_value = if m1_core::is_compound_assign(op.kind()) {
        let current = read_dest(&dest, ctx)?;
        apply_compound(op.kind(), &current, &rhs)?
    } else {
        rhs
    };

    write_dest(&dest, final_value, ctx);
    Ok(())
}

/// Execute an expression statement: evaluate its inner expression purely for its
/// side effects (e.g. `Output.SetState(...)`, a Tier-3 IO call). The produced
/// value is discarded; any evaluation error surfaces fail-loud.
fn exec_expression_statement(node: &Node, ctx: &mut EvalCtx) -> Result<(), EvalError> {
    // The single named child is the expression.
    let inner = node
        .named_children()
        .into_iter()
        .next()
        .ok_or_else(|| shape_err(node, "expression statement"))?;
    eval(&inner, ctx)?;
    Ok(())
}

/// A resolved assignment destination: where the value lives so we can read it
/// back (compound assignment) and write it.
enum Dest {
    /// A function-local variable, by name.
    Local(String),
    /// A `static local`, by its owning function symbol + variable name.
    Static { fn_symbol: String, var: String },
    /// A project channel/parameter, by canonical path.
    Channel(String),
    /// The current function frame's `Out` return slot (`Out = <expr>;`). The
    /// `Out` keyword is function-local and never a project symbol — `resolve`
    /// treats it as opaque — so it is special-cased here to land in the env
    /// out-slot, which `userfn::call` reads as the function's return value.
    Out,
}

/// Resolve an assignment target node into a [`Dest`]. The target is a path
/// expression (identifier or member); we flatten it, rewrite a leading `This`,
/// then classify it. A local name writes a local; a known `static local` name
/// writes the static slot; a project symbol writes its canonical channel path.
/// An unresolved target fails loud rather than inventing a new channel.
fn resolve_target(target: &Node, ctx: &mut EvalCtx) -> Result<Dest, EvalError> {
    let path = target_path(target)?;
    let rewritten = expr::rewrite_this(&path, ctx.group);
    let path = rewritten.as_deref().unwrap_or(&path);

    // The `Out` return-value object: a user function's `Out = <expr>;` assignment
    // writes the env out-slot, the value `userfn::call` reads back as the return.
    // `Out` is function-local (never a project symbol), so it is recognised here
    // before classification rather than failing loud as an unresolved target.
    if path == "Out" {
        return Ok(Dest::Out);
    }

    // A bare name that already names a `static local` of the current function is a
    // static write — this is how a `static local x` accumulator is updated.
    if let Some(fn_symbol) = ctx.fn_symbol
        && !path.contains('.')
        && ctx.env.get_static(fn_symbol, path).is_some()
    {
        return Ok(Dest::Static {
            fn_symbol: fn_symbol.to_string(),
            var: path.to_string(),
        });
    }

    match classify(path, ctx.group, ctx.fn_symbol, ctx.project, &ctx.env.locals) {
        Target::Local(name) => Ok(Dest::Local(name)),
        Target::Symbol(canon) => Ok(Dest::Channel(canon)),
        // Writing through a builtin object or an unresolved name is a fail-loud
        // error: the evaluator never invents a destination.
        Target::Builtin { object } => Err(EvalError::UnsupportedConstruct {
            kind: format!("assignment to builtin object {object:?}"),
            at: target.byte_range().start,
        }),
        Target::Unresolved => Err(EvalError::UnresolvedSymbol {
            name: path.to_string(),
        }),
    }
}

/// The dotted source path of an assignment target node: an identifier verbatim,
/// or a member expression flattened on `.`.
fn target_path(target: &Node) -> Result<String, EvalError> {
    match target.kind() {
        Kind::MemberExpression => expr::flatten_member(target),
        Kind::Identifier => Ok(target.text().to_string()),
        other => Err(EvalError::UnsupportedConstruct {
            kind: format!("assignment target {other:?}"),
            at: target.byte_range().start,
        }),
    }
}

/// Read the current value at a destination (for compound assignment). A local /
/// static / `Out` slot that has no current value is a fail-loud error — a compound
/// assignment that reads an unset slot cannot proceed. A channel reads through
/// [`expr::read_symbol`] so its read-back honours the same semantics as any other
/// channel read: a written/seeded value, else (in whole-project mode) the
/// channel's externally-driven startup default, else fail-loud `MissingInput`.
fn read_dest(dest: &Dest, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    match dest {
        Dest::Local(name) => {
            ctx.env
                .get_local(name)
                .cloned()
                .ok_or_else(|| EvalError::MissingInput {
                    channel: name.clone(),
                })
        }
        Dest::Static { fn_symbol, var } => {
            ctx.env
                .get_static(fn_symbol, var)
                .cloned()
                .ok_or_else(|| EvalError::MissingInput {
                    channel: var.clone(),
                })
        }
        Dest::Channel(canon) => expr::read_symbol(canon, ctx),
        Dest::Out => ctx
            .env
            .get_out()
            .cloned()
            .ok_or_else(|| EvalError::MissingInput {
                channel: "Out".to_string(),
            }),
    }
}

/// Write a value to a destination, recording channel writes into the trace.
fn write_dest(dest: &Dest, value: Value, ctx: &mut EvalCtx) {
    match dest {
        Dest::Local(name) => ctx.env.set_local(name.clone(), value),
        Dest::Static { fn_symbol, var } => ctx.env.set_static(fn_symbol, var, value),
        Dest::Channel(canon) => {
            // Coerce a numeric write to an enum-typed channel to its enum member,
            // so an enum channel assigned an integer holds a typed enum value (the
            // same implicit int→enum conversion M1 applies on assignment).
            let value = expr::coerce_for_channel(canon, value, ctx.project);
            ctx.env.set(canon.clone(), value.clone());
            if let Some(trace) = ctx.trace.as_deref_mut() {
                trace.record_channel(canon.clone(), value);
            }
        }
        // The return slot is function-local: write it, do not record a channel.
        Dest::Out => ctx.env.set_out(value),
    }
}

/// Apply a compound-assignment operator to the current value and the rhs. The
/// arithmetic/bitwise semantics match the expression evaluator's binary
/// operators (delegated through a fresh binary-op evaluation).
fn apply_compound(op: Kind, current: &Value, rhs: &Value) -> Result<Value, EvalError> {
    // Map each compound operator to its underlying binary operator, then reuse the
    // expression evaluator's binary semantics so the result typing/coercions stay
    // consistent (`numeric_join`, integral-only bitwise, div-by-zero handling).
    let binop = match op {
        Kind::PlusEq => Kind::Plus,
        Kind::MinusEq => Kind::Minus,
        Kind::StarEq => Kind::Star,
        Kind::SlashEq => Kind::Slash,
        Kind::PercentEq => Kind::Percent,
        Kind::AmpEq => Kind::Amp,
        Kind::PipeEq => Kind::Pipe,
        Kind::CaretEq => Kind::Caret,
        Kind::LtLtEq => Kind::LtLt,
        Kind::GtGtEq => Kind::GtGt,
        other => {
            return Err(EvalError::UnsupportedConstruct {
                kind: format!("compound assignment operator {other:?}"),
                at: 0,
            });
        }
    };
    expr::apply_binary_values(binop, current, rhs)
}

// ---- Task 21: if/else, when/is, expand/to, local/static local, block ----

/// Execute a block: each child statement in source order. Braces and trivia are
/// skipped via [`is_statement`].
fn exec_block(node: &Node, ctx: &mut EvalCtx) -> Result<(), EvalError> {
    for child in node.children() {
        if is_statement(child.kind()) {
            exec(&child, ctx)?;
        }
    }
    Ok(())
}

/// Execute an `if`/`else if`/`else` chain. The `condition` must be boolean; when
/// true the `consequence` block runs, otherwise the `ElseClause` (a child node)
/// runs — its content is either a `Block` (plain `else`) or a nested
/// `IfStatement` (`else if`).
fn exec_if(node: &Node, ctx: &mut EvalCtx) -> Result<(), EvalError> {
    let cond = node
        .child_by_field(Field::Condition)
        .ok_or_else(|| shape_err(node, "if condition"))?;
    let consequence = node
        .child_by_field(Field::Consequence)
        .ok_or_else(|| shape_err(node, "if consequence"))?;

    if eval(&cond, ctx)?.as_bool()? {
        return exec(&consequence, ctx);
    }

    // No `else if` matched the boolean: run the else clause if present.
    if let Some(else_clause) = node
        .children()
        .into_iter()
        .find(|c| c.kind() == Kind::ElseClause)
    {
        // The else clause wraps either a Block (`else { … }`) or a nested
        // IfStatement (`else if … { … }`). Execute whichever it carries.
        for child in else_clause.children() {
            match child.kind() {
                Kind::Block => return exec_block(&child, ctx),
                Kind::IfStatement => return exec_if(&child, ctx),
                _ => {}
            }
        }
    }
    Ok(())
}

/// Execute a `when (Subject) { is (Pattern) { … } … }`. The subject is evaluated
/// once; each `IsClause` is tried in order and the first whose pattern matches
/// the subject's value runs its body. A non-matching `when` runs no clause (no
/// fall-through). Patterns are enum members (`State.On`, `On`) compared against
/// the subject enum value, or a pattern list (`is (A or B)`).
fn exec_when(node: &Node, ctx: &mut EvalCtx) -> Result<(), EvalError> {
    let subject_node = node
        .child_by_field(Field::Subject)
        .ok_or_else(|| shape_err(node, "when subject"))?;
    let subject = eval(&subject_node, ctx)?;

    for clause in node.children() {
        if clause.kind() != Kind::IsClause {
            continue;
        }
        let state = clause
            .child_by_field(Field::State)
            .ok_or_else(|| shape_err(&clause, "is-clause state"))?;
        if pattern_matches(&state, &subject, ctx)? {
            if let Some(body) = clause.child_by_field(Field::Body) {
                return exec(&body, ctx);
            }
            return Ok(());
        }
    }
    Ok(())
}

/// Whether an `is`-clause pattern matches the subject value. A single pattern is
/// an enum member (its last `.`-segment is the member name); a pattern list
/// (`is (A or B)`) matches if any of its member patterns match.
fn pattern_matches(pattern: &Node, subject: &Value, ctx: &mut EvalCtx) -> Result<bool, EvalError> {
    if pattern.kind() == Kind::IsPatternList {
        for p in pattern.named_children() {
            if pattern_matches(&p, subject, ctx)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    single_pattern_matches(pattern, subject, ctx)
}

/// Match one (non-list) pattern against the subject. Enum subjects compare by
/// member name (the pattern's trailing `.`-segment, e.g. `State.On` → `On`).
/// Boolean subjects accept the `True`/`False` patterns. Other subject kinds fall
/// back to evaluating the pattern as an expression and comparing equality.
fn single_pattern_matches(
    pattern: &Node,
    subject: &Value,
    ctx: &mut EvalCtx,
) -> Result<bool, EvalError> {
    // The member-name spelling of the pattern: the last `.`-segment of its text.
    let pattern_text = pattern.text();
    let member = pattern_text
        .rsplit('.')
        .next()
        .unwrap_or(pattern_text)
        .trim();

    match subject {
        Value::Enum {
            member: subj_member,
            ..
        } => Ok(subj_member == member),
        Value::Bool(b) => match member {
            "True" | "true" => Ok(*b),
            "False" | "false" => Ok(!*b),
            _ => Ok(false),
        },
        // A numeric/string subject: compare against the pattern evaluated as an
        // expression. This keeps `when` usable for non-enum subjects without
        // guessing.
        _ => {
            let pat_value = eval(pattern, ctx)?;
            values_equal_loose(subject, &pat_value)
        }
    }
}

/// Loose equality used by non-enum `when` patterns: numeric-by-value, otherwise
/// direct. Mismatched kinds are simply not equal (a `when` clause that cannot
/// match is skipped, not an error).
fn values_equal_loose(a: &Value, b: &Value) -> Result<bool, EvalError> {
    match (a, b) {
        (Value::Bool(x), Value::Bool(y)) => Ok(x == y),
        (Value::Str(x), Value::Str(y)) => Ok(x == y),
        (Value::Enum { id: i1, member: m1 }, Value::Enum { id: i2, member: m2 }) => {
            Ok(i1 == i2 && m1 == m2)
        }
        (
            Value::Int(_) | Value::Uint(_) | Value::Float(_),
            Value::Int(_) | Value::Uint(_) | Value::Float(_),
        ) => Ok(a.as_f64()? == b.as_f64()?),
        _ => Ok(false),
    }
}

/// Execute an `expand (VAR = Start to End) { body }` compile-time loop. The loop
/// variable takes each integer value from `Start` to `End` inclusive; for each
/// iteration the body is re-evaluated with `$(VAR)` interpolations substituted by
/// the current value. We substitute textually then re-parse the body so the
/// expanded identifiers resolve like ordinary source.
fn exec_expand(node: &Node, ctx: &mut EvalCtx) -> Result<(), EvalError> {
    let var = node
        .child_by_field(Field::Variable)
        .ok_or_else(|| shape_err(node, "expand variable"))?;
    let start_node = node
        .child_by_field(Field::Start)
        .ok_or_else(|| shape_err(node, "expand start"))?;
    let end_node = node
        .child_by_field(Field::End)
        .ok_or_else(|| shape_err(node, "expand end"))?;
    let body = node
        .children()
        .into_iter()
        .find(|c| c.kind() == Kind::Block)
        .ok_or_else(|| shape_err(node, "expand body"))?;

    let var_name = var.text().trim().to_string();
    let start = eval(&start_node, ctx)?.as_f64()? as i64;
    let end = eval(&end_node, ctx)?.as_f64()? as i64;

    // The body source, with `$(VAR)` placeholders, taken verbatim from the CST.
    let body_src = body.text().to_string();

    // The expand range is inclusive of both ends (the manual's `Start to End`).
    // Walk it in whichever direction the bounds imply so descending loops work.
    let range: Vec<i64> = if start <= end {
        (start..=end).collect()
    } else {
        (end..=start).rev().collect()
    };

    // Preserve any existing local that shadows the loop-variable name so we can
    // restore it after the loop (the loop variable is scoped to the expand body).
    let saved = ctx.env.get_local(&var_name).cloned();

    for i in range {
        // The loop variable is an integer local for the body: a bare `i` reads it,
        // while `$(i)` splices its value textually into identifiers/paths
        // (`Out$(i)` → `Out1`). Bind the local, then substitute the placeholders.
        ctx.env.set_local(var_name.clone(), Value::Int(i));
        let substituted = substitute_interpolation(&body_src, &var_name, i);
        // Re-parse the substituted block and execute its statements. The block is
        // a `{ … }`; parsing it as a standalone source yields a `Block` (or a
        // SourceFile wrapping one), whose statements we run.
        let cst = m1_core::parse(&substituted);
        let root = cst.root();
        exec_expanded_root(&root, ctx)?;
    }

    // Restore the prior local binding (or clear the loop var if there was none).
    match saved {
        Some(v) => ctx.env.set_local(var_name, v),
        None => {
            ctx.env.locals.remove(&var_name);
        }
    }
    Ok(())
}

/// Execute the statements of a re-parsed expand-body. The re-parsed source is a
/// brace-delimited block; its root is a `SourceFile` containing a `Block` (or the
/// statements directly). Run every statement found at the top level or inside a
/// single wrapping block.
fn exec_expanded_root(root: &Node, ctx: &mut EvalCtx) -> Result<(), EvalError> {
    for child in root.children() {
        match child.kind() {
            Kind::Block => exec_block(&child, ctx)?,
            k if is_statement(k) => exec(&child, ctx)?,
            _ => {}
        }
    }
    Ok(())
}

/// Substitute every `$(VAR)` occurrence in `src` with the current integer value.
/// M1's expand interpolation splices the loop value into identifiers/paths
/// (`Output$(i)` → `Output1`), so we replace the exact `$(VAR)` token text. Only
/// the named loop variable is substituted; other `$(...)` are left untouched.
fn substitute_interpolation(src: &str, var: &str, value: i64) -> String {
    let needle = format!("$({var})");
    src.replace(&needle, &value.to_string())
}

/// Execute a `local` / `static local` declaration. The `name` field is the
/// variable; the optional `value` field its initialiser. A `static` keyword (a
/// `Static` child) makes it persist across function entry/exit, keyed by the
/// owning function symbol — and it is only initialised once (its persisted value
/// survives re-execution). A plain `local` is (re)initialised each time.
fn exec_local_declaration(node: &Node, ctx: &mut EvalCtx) -> Result<(), EvalError> {
    let name_node = node
        .child_by_field(Field::Name)
        .ok_or_else(|| shape_err(node, "local declaration name"))?;
    let var = name_node.text().trim().to_string();

    let is_static = node.children().iter().any(|c| c.kind() == Kind::Static);

    // The declared type annotation, if any, drives coercion of the initialiser so
    // `local <Float> x = 1;` stores a Float, not an Integer (the annotation is
    // authoritative per the manual).
    let declared = node
        .child_by_field(Field::TypeAnnotation)
        .and_then(|a| a.child_by_field(Field::Type))
        .and_then(|t| declared_local_type(t.text()));

    if is_static {
        // A static local initialises exactly once: its persisted slot survives
        // every later tick, so do not re-run the initialiser if it already holds a
        // value. Keyed by the owning function symbol so two functions' statics do
        // not collide.
        let fn_symbol = ctx
            .fn_symbol
            .ok_or_else(|| EvalError::UnsupportedConstruct {
                kind: "static local outside a function".to_string(),
                at: node.byte_range().start,
            })?;
        if ctx.env.get_static(fn_symbol, &var).is_none() {
            let init = match node.child_by_field(Field::Value) {
                Some(v) => coerce_to(eval(&v, ctx)?, declared)?,
                None => default_for(declared),
            };
            let fn_symbol = ctx.fn_symbol.unwrap().to_string();
            ctx.env.set_static(&fn_symbol, &var, init);
        }
        return Ok(());
    }

    // A plain local is (re)initialised on every execution.
    let value = match node.child_by_field(Field::Value) {
        Some(v) => coerce_to(eval(&v, ctx)?, declared)?,
        None => default_for(declared),
    };
    ctx.env.set_local(var, value);
    Ok(())
}

/// Coerce an initialiser value to a declared local type. Only the numeric
/// widenings the language allows are applied (an integer literal initialising a
/// `<Float>` local becomes a float); a genuinely incompatible value is left as-is
/// rather than guessed — the type checker is the authority on validity, and the
/// evaluator does not silently change a value's meaning.
fn coerce_to(value: Value, declared: Option<ValueType>) -> Result<Value, EvalError> {
    let Some(ty) = declared else {
        return Ok(value);
    };
    Ok(match (ty, &value) {
        (ValueType::Float, Value::Int(x)) => Value::Float(*x as f64),
        (ValueType::Float, Value::Uint(x)) => Value::Float(*x as f64),
        // Already the right family, or a coercion we do not force: keep the value.
        _ => value,
    })
}

/// The default value for an uninitialised typed local. M1 requires initialisers
/// in practice, but a bare `local x;` with a known type gets a zero of that type
/// so a later read does not fail loud spuriously; an untyped bare local defaults
/// to integer zero.
fn default_for(declared: Option<ValueType>) -> Value {
    match declared {
        Some(ValueType::Float) => Value::Float(0.0),
        Some(ValueType::Unsigned) => Value::Uint(0),
        Some(ValueType::Boolean) => Value::Bool(false),
        Some(ValueType::String) => Value::Str(String::new()),
        _ => Value::Int(0),
    }
}

/// A fail-loud shape error for a malformed statement node.
fn shape_err(node: &Node, what: &str) -> EvalError {
    EvalError::UnsupportedConstruct {
        kind: format!("malformed {what}"),
        at: node.byte_range().start,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calib::Calibration;
    use crate::env::{Env, StateStore};
    use crate::trace::Trace;
    use m1_core::parse;
    use m1_typecheck::Project;
    use std::path::Path;

    fn mini_project() -> Project {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini");
        crate::loader::load(&dir.join("Project.m1prj"), None)
            .expect("mini fixture loads")
            .project
    }

    struct Harness {
        project: Project,
        calib: Calibration,
        env: Env,
        state: StateStore,
        trace: Trace,
    }

    impl Harness {
        fn new() -> Harness {
            Harness {
                project: mini_project(),
                calib: Calibration::default(),
                env: Env::new(),
                state: StateStore::new(),
                trace: Trace::new(),
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
                scripts: &[],
                depth: 0,
                trace: Some(&mut self.trace),
            }
        }

        /// Parse `src` and execute every top-level statement against this harness.
        fn run(&mut self, src: &str) -> Result<(), EvalError> {
            let cst = parse(src);
            let root = cst.root();
            let mut ctx = self.ctx();
            for child in root.children() {
                if is_statement(child.kind()) {
                    exec(&child, &mut ctx)?;
                }
            }
            Ok(())
        }
    }

    // ---- Task 20: assignment + expression statement ----

    #[test]
    fn plain_assignment_writes_channel() {
        let mut h = Harness::new();
        h.run("Output = 42.0;\n").unwrap();
        assert_eq!(h.env.get("Root.Demo.Output"), Some(&Value::Float(42.0)));
        // The channel write is also recorded in the trace.
        assert_eq!(
            h.trace.channels.get("Root.Demo.Output"),
            Some(&vec![Value::Float(42.0)])
        );
    }

    #[test]
    fn assignment_evaluates_rhs_expression() {
        let mut h = Harness::new();
        h.env.set("Root.Demo.Speed", Value::Float(10.0));
        h.calib.params.insert("Demo.Gain".to_string(), 2.5);
        h.run("Output = Speed * Gain;\n").unwrap();
        assert_eq!(h.env.get("Root.Demo.Output"), Some(&Value::Float(25.0)));
    }

    #[test]
    fn this_target_writes_group_channel() {
        let mut h = Harness::new();
        h.run("This.Output = 7.0;\n").unwrap();
        assert_eq!(h.env.get("Root.Demo.Output"), Some(&Value::Float(7.0)));
    }

    #[test]
    fn assignment_to_local_writes_local() {
        let mut h = Harness::new();
        // A local must be declared first to be a known local name.
        h.run("local acc = 1;\nacc = 5;\n").unwrap();
        assert_eq!(h.env.get_local("acc"), Some(&Value::Int(5)));
        // It is NOT mistaken for a project channel write.
        assert!(!h.trace.channels.contains_key("Root.Demo.acc"));
    }

    #[test]
    fn compound_add_reads_then_writes() {
        let mut h = Harness::new();
        h.env.set("Root.Demo.Output", Value::Int(10));
        h.run("Output += 5;\n").unwrap();
        assert_eq!(h.env.get("Root.Demo.Output"), Some(&Value::Int(15)));
    }

    #[test]
    fn compound_on_local() {
        let mut h = Harness::new();
        h.run("local n = 3;\nn *= 4;\n").unwrap();
        assert_eq!(h.env.get_local("n"), Some(&Value::Int(12)));
    }

    #[test]
    fn compound_on_unset_channel_fails_loud() {
        let mut h = Harness::new();
        // `Output += 1` with no current value cannot read-then-write.
        match h.run("Output += 1;\n") {
            Err(EvalError::MissingInput { .. }) => {}
            other => panic!("expected MissingInput, got {other:?}"),
        }
    }

    #[test]
    fn out_assignment_writes_the_out_slot() {
        let mut h = Harness::new();
        // `Out = <expr>;` is the user-function return assignment: it writes the
        // env out-slot, not a project channel (and does not fail loud as an
        // unresolved target the way it did before P15-D).
        h.run("Out = 3 * 2;\n").unwrap();
        assert_eq!(h.env.get_out(), Some(&Value::Int(6)));
        // It is NOT recorded as a project channel write.
        assert!(!h.trace.channels.contains_key("Out"));
        assert!(!h.trace.channels.contains_key("Root.Demo.Out"));
    }

    #[test]
    fn out_compound_assignment_reads_then_writes_the_out_slot() {
        let mut h = Harness::new();
        // A compound `Out +=` reads the current out-slot first, then writes back.
        h.run("Out = 1;\nOut += 4;\n").unwrap();
        assert_eq!(h.env.get_out(), Some(&Value::Int(5)));
    }

    #[test]
    fn unresolved_target_fails_loud() {
        let mut h = Harness::new();
        match h.run("NoSuchChannel = 1;\n") {
            Err(EvalError::UnresolvedSymbol { name }) => assert_eq!(name, "NoSuchChannel"),
            other => panic!("expected UnresolvedSymbol, got {other:?}"),
        }
    }

    #[test]
    fn expression_statement_runs_for_side_effects() {
        let mut h = Harness::new();
        // A System.* IO call is a documented stub; as an expression statement it
        // evaluates (for its side effect / value) and is discarded without error.
        h.run("System.Reset();\n")
            .expect_err("Reset is not stubbed -> fail loud");
        // A stubbed System.TickPeriod() call as a statement does not error.
        h.run("System.TickPeriod();\n")
            .expect("TickPeriod is a documented stub");
    }

    // ---- Task 21: if/else ----

    #[test]
    fn if_true_runs_consequence() {
        let mut h = Harness::new();
        h.run("if (true)\n{\n\tOutput = 1.0;\n}\n").unwrap();
        assert_eq!(h.env.get("Root.Demo.Output"), Some(&Value::Float(1.0)));
    }

    #[test]
    fn if_false_skips_consequence() {
        let mut h = Harness::new();
        h.run("if (false)\n{\n\tOutput = 1.0;\n}\n").unwrap();
        assert_eq!(h.env.get("Root.Demo.Output"), None);
    }

    #[test]
    fn if_else_runs_alternative() {
        let mut h = Harness::new();
        h.run("if (false)\n{\n\tOutput = 1.0;\n}\nelse\n{\n\tOutput = 2.0;\n}\n")
            .unwrap();
        assert_eq!(h.env.get("Root.Demo.Output"), Some(&Value::Float(2.0)));
    }

    #[test]
    fn else_if_chain_picks_middle() {
        let mut h = Harness::new();
        h.env.set("Root.Demo.Speed", Value::Float(5.0));
        h.run(
            "if (Speed > 10.0)\n{\n\tOutput = 1.0;\n}\nelse if (Speed > 3.0)\n{\n\tOutput = 2.0;\n}\nelse\n{\n\tOutput = 3.0;\n}\n",
        )
        .unwrap();
        assert_eq!(h.env.get("Root.Demo.Output"), Some(&Value::Float(2.0)));
    }

    #[test]
    fn if_non_bool_condition_fails_loud() {
        let mut h = Harness::new();
        match h.run("if (1)\n{\n\tOutput = 1.0;\n}\n") {
            Err(EvalError::TypeError { .. }) => {}
            other => panic!("expected TypeError, got {other:?}"),
        }
    }

    // ---- Task 21: when/is ----

    #[test]
    fn when_matches_enum_member() {
        let mut h = Harness::new();
        // Seed the subject as an enum value; the matching is-clause runs.
        h.env.set(
            "Root.Demo.Speed",
            Value::Enum {
                id: 1,
                member: "On".to_string(),
            },
        );
        h.run("when (Speed)\n{\n\tis (On)\n\t{\n\t\tOutput = 1.0;\n\t}\n\tis (Off)\n\t{\n\t\tOutput = 2.0;\n\t}\n}\n")
            .unwrap();
        assert_eq!(h.env.get("Root.Demo.Output"), Some(&Value::Float(1.0)));
    }

    #[test]
    fn when_matches_qualified_member() {
        let mut h = Harness::new();
        h.env.set(
            "Root.Demo.Speed",
            Value::Enum {
                id: 1,
                member: "Off".to_string(),
            },
        );
        // `is (State.Off)` matches by the trailing member segment.
        h.run("when (Speed)\n{\n\tis (State.On)\n\t{\n\t\tOutput = 1.0;\n\t}\n\tis (State.Off)\n\t{\n\t\tOutput = 2.0;\n\t}\n}\n")
            .unwrap();
        assert_eq!(h.env.get("Root.Demo.Output"), Some(&Value::Float(2.0)));
    }

    #[test]
    fn when_pattern_list_matches_any() {
        let mut h = Harness::new();
        h.env.set(
            "Root.Demo.Speed",
            Value::Enum {
                id: 1,
                member: "B".to_string(),
            },
        );
        h.run("when (Speed)\n{\n\tis (A or B)\n\t{\n\t\tOutput = 9.0;\n\t}\n\tis (C)\n\t{\n\t\tOutput = 1.0;\n\t}\n}\n")
            .unwrap();
        assert_eq!(h.env.get("Root.Demo.Output"), Some(&Value::Float(9.0)));
    }

    #[test]
    fn when_no_match_runs_nothing() {
        let mut h = Harness::new();
        h.env.set(
            "Root.Demo.Speed",
            Value::Enum {
                id: 1,
                member: "Z".to_string(),
            },
        );
        h.run("when (Speed)\n{\n\tis (A)\n\t{\n\t\tOutput = 1.0;\n\t}\n}\n")
            .unwrap();
        assert_eq!(h.env.get("Root.Demo.Output"), None);
    }

    // ---- Task 21: expand/to ----

    #[test]
    fn expand_loop_binds_interpolation() {
        let mut h = Harness::new();
        // Expand writes to locals named Out1..Out3, each set to its index.
        h.run("expand (i = 1 to 3)\n{\n\tlocal Out$(i) = i;\n}\n")
            .unwrap();
        assert_eq!(h.env.get_local("Out1"), Some(&Value::Int(1)));
        assert_eq!(h.env.get_local("Out2"), Some(&Value::Int(2)));
        assert_eq!(h.env.get_local("Out3"), Some(&Value::Int(3)));
    }

    #[test]
    fn expand_accumulates_into_static() {
        let mut h = Harness::new();
        // Sum 1+2+3 = 6 into a static accumulator.
        h.run("static local total = 0;\nexpand (i = 1 to 3)\n{\n\ttotal += i;\n}\n")
            .unwrap();
        assert_eq!(
            h.env.get_static("Root.Demo.Update", "total"),
            Some(&Value::Int(6))
        );
    }

    // ---- Task 21: local / static local ----

    #[test]
    fn local_declaration_initialises() {
        let mut h = Harness::new();
        h.run("local scaled = 3 * 4;\n").unwrap();
        assert_eq!(h.env.get_local("scaled"), Some(&Value::Int(12)));
    }

    #[test]
    fn typed_local_coerces_initialiser() {
        let mut h = Harness::new();
        // `local <Float> x = 1;` stores a Float, not an Integer (annotation wins).
        h.run("local <Float> x = 1;\n").unwrap();
        assert_eq!(h.env.get_local("x"), Some(&Value::Float(1.0)));
    }

    #[test]
    fn static_local_accumulates_and_reads_back() {
        let mut h = Harness::new();
        // A canonical stateful pattern: a static accumulator updated and then read
        // back into a channel. The static read must resolve to its persisted slot,
        // not fail loud as unresolved.
        h.run("static local accum = 0.0;\naccum += 2.5;\nOutput = accum;\n")
            .unwrap();
        assert_eq!(
            h.env.get_static("Root.Demo.Update", "accum"),
            Some(&Value::Float(2.5))
        );
        assert_eq!(h.env.get("Root.Demo.Output"), Some(&Value::Float(2.5)));
    }

    #[test]
    fn static_local_initialises_once_and_persists() {
        let mut h = Harness::new();
        // First declaration seeds the static to 0; a later write changes it; a
        // second declaration (re-execution) does NOT reset it.
        h.run("static local accum = 0.0;\naccum = 10.0;\n").unwrap();
        assert_eq!(
            h.env.get_static("Root.Demo.Update", "accum"),
            Some(&Value::Float(10.0))
        );
        // Re-running the declaration keeps the persisted value.
        h.run("static local accum = 0.0;\n").unwrap();
        assert_eq!(
            h.env.get_static("Root.Demo.Update", "accum"),
            Some(&Value::Float(10.0))
        );
    }

    // ---- Task 21: block ----

    #[test]
    fn block_runs_children_in_order() {
        let mut h = Harness::new();
        h.run("{\n\tlocal a = 1;\n\tlocal b = 2;\n\tOutput = 3.0;\n}\n")
            .unwrap();
        assert_eq!(h.env.get_local("a"), Some(&Value::Int(1)));
        assert_eq!(h.env.get_local("b"), Some(&Value::Int(2)));
        assert_eq!(h.env.get("Root.Demo.Output"), Some(&Value::Float(3.0)));
    }

    #[test]
    fn empty_statement_is_noop() {
        let mut h = Harness::new();
        h.run(";\n").unwrap();
    }

    // ---- Task 22: whole-script execution ----

    #[test]
    fn exec_script_runs_the_fixture_end_to_end() {
        // The mini fixture's Demo.Update.m1scr:
        //   local scaled = Speed * Gain;
        //   Output = scaled;
        // Seed Speed and Gain; running the whole script writes Output.
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini");
        let loaded = crate::loader::load(
            &dir.join("Project.m1prj"),
            Some(&dir.join("parameters.m1cfg")),
        )
        .expect("mini fixture loads");

        let mut env = Env::new();
        env.set("Root.Demo.Speed", Value::Float(20.0));
        let mut state = StateStore::new();
        let mut trace = Trace::new();
        trace.push_tick(0.0);

        let script = &loaded.scripts[0];
        let root = script.cst.root();
        let mut ctx = EvalCtx {
            project: &loaded.project,
            calib: &loaded.calib,
            env: &mut env,
            state: &mut state,
            group: Some("Root.Demo"),
            fn_symbol: Some("Root.Demo.Update"),
            script_name: &script.name,
            dt: 0.01,
            scripts: &[],
            depth: 0,
            trace: Some(&mut trace),
        };

        exec_script(&root, &mut ctx).expect("script executes");

        // Gain is 2.5 (from parameters.m1cfg, written as Demo.Gain); Speed is 20,
        // so Output = 20 * 2.5 = 50.
        assert_eq!(env.get("Root.Demo.Output"), Some(&Value::Float(50.0)));
        // And the output write landed in the trace's channel column.
        assert_eq!(
            trace.channels.get("Root.Demo.Output"),
            Some(&vec![Value::Float(50.0)])
        );
    }
}
