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
//! Value reads are **fail-loud** for true runtime inputs: an unset channel is a
//! [`EvalError::MissingInput`] and an unresolved name a
//! [`EvalError::UnresolvedSymbol`] — never a guessed number. A parameter is a
//! tunable calibration value: an unseeded one (no `.m1cfg`, no override) defaults
//! to its declared-type zero, flagged externally driven, like the Tier-3 IO stubs
//! (see `read_symbol`). A constant instead comes from its target-typed project
//! value and cannot be replaced by a calibration cell.
//!
//! Identifier paths may contain spaces (`Cooling Fan`); we only ever split paths
//! on `.`, never on whitespace.

use crate::calib::Calibration;
use crate::env::{CallSite, Env, StateStore};
use crate::error::EvalError;
use crate::ident::{Target, classify};
use crate::value::{FixedPoint7dps, M1Scalar, M1ScalarKind, Value};
use m1_core::{Field, Kind, Node};
use m1_typecheck::Project;
use m1_typecheck::symbols::{Symbol, SymbolKind};
use m1_typecheck::types::{ValueType, numeric_join, type_of_number_literal};
use std::collections::{HashMap, HashSet};

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
    /// Every parsed script in the project, so an inline user-function call
    /// ([`crate::builtins::userfn`]) can find the backing `ParsedScript` of the
    /// callee symbol (the reverse of `function_symbol_for_script`). Threaded from
    /// the runner; an empty slice in unit tests that never call a user function.
    pub scripts: &'a [m1_typecheck::parsed::ParsedScript],
    /// Exact numeric families retained from raw project function signatures.
    /// `None` is supported for direct expression users that did not load a
    /// project through [`crate::loader::load`].
    pub signature_m1_types: Option<&'a crate::loader::SignatureM1Types>,
    /// Project-owned validation rules for core object methods. Loader-backed
    /// evaluation always supplies them; small expression unit tests may omit
    /// them when they do not call those methods.
    pub object_rules: Option<&'a crate::builtins::object::ObjectRules>,
    /// Current inline-call nesting depth. `0` at the top of a tick; incremented
    /// each time [`crate::builtins::userfn::call`] enters a callee body, so a
    /// runtime call cycle fails loud past a fixed bound rather than overflowing
    /// the stack (the upstream static check is T097; this is the runtime guard).
    pub depth: u32,
    /// Optional per-expression / external-channel sink. When present, the call
    /// evaluator records each builtin call's result value at its [`CallSite`],
    /// and Tier-3 IO stubs flag the channels they externally drive. `None` in
    /// unit tests that only want the returned value.
    pub trace: Option<&'a mut crate::trace::Trace>,
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
        ValueType::Unsigned => parse_uint(text).map(Value::m1_unsigned),
        ValueType::Float => {
            let narrowed = text.parse::<f32>().map_err(|_| bad_number(text))?;
            if !narrowed.is_finite() {
                Err(bad_number(text))
            } else {
                Ok(Value::m1_float(narrowed))
            }
        }
        // Integer (and any Unknown fallback the literal typer never returns here).
        _ => text
            .parse::<i32>()
            .map(Value::m1_integer)
            .map_err(|_| bad_number(text)),
    }
}

/// Parse an unsigned literal: hex (`0x…`, optional trailing `u`) or a decimal
/// with an optional trailing `u`.
fn parse_uint(text: &str) -> Result<u32, EvalError> {
    let lower = text.to_ascii_lowercase();
    let body = lower.strip_suffix('u').unwrap_or(&lower);
    let parsed = if let Some(hex) = body.strip_prefix("0x") {
        u32::from_str_radix(hex, 16)
    } else {
        body.parse::<u32>()
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
pub(crate) fn flatten_member(node: &Node) -> Result<String, EvalError> {
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
pub(crate) fn rewrite_this(path: &str, group: Option<&str>) -> Option<String> {
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

    // A bare name (no dotted path) that names a `static local` of the current
    // function reads its persisted slot. Static locals are not project symbols and
    // do not live in `env.locals`, so the resolver would otherwise miss them; this
    // check comes first so a stateful accumulator reads back the value it holds.
    if let Some(fn_symbol) = ctx.fn_symbol
        && !path.contains('.')
        && let Some(v) = ctx.env.get_static(fn_symbol, path)
    {
        return Ok(v.clone());
    }

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
        // Not a project symbol/local/builtin: it may be an enum-type-qualified
        // member literal used directly as a value (`x eq Universal Switch State.On`,
        // `Drive State.Idle`). Resolve it to the corresponding [`Value::Enum`]
        // before failing loud — these literals are compile-time-constant values.
        Target::Unresolved => {
            enum_member_literal(path, ctx).ok_or_else(|| EvalError::UnresolvedSymbol {
                name: path.to_string(),
            })
        }
    }
}

/// If `path` is an enum-member literal, the corresponding [`Value::Enum`];
/// otherwise `None`. Two qualifier forms appear in real scripts, both split on the
/// **rightmost** `.` only (enum type, member, and symbol names all contain spaces):
///
/// 1. **Enum-type-qualified** `<EnumTypeName>.<Member>` (`Universal Switch State
///    .On`, `Drive State.Idle`): the prefix names an enum type directly.
/// 2. **Value-source-qualified** `<EnumValuedSymbol>.<Member>` (`This.Drive State
///    .Ready To Drive`, where `This.Drive State` is an enum-valued value-compound):
///    the prefix resolves to a project symbol whose `value_type` is that enum, and
///    `<Member>` is one of its members. M1 lets the author qualify a member by the
///    compound/channel that holds the enum, not just by the bare type name.
///
/// A prefix that is a real enum source but whose leaf is not one of its members is
/// *not* a literal (returns `None` → the caller fails loud as unresolved), as a
/// non-member would be an undefined name.
fn enum_member_literal(path: &str, ctx: &EvalCtx) -> Option<Value> {
    let (prefix, leaf) = path.rsplit_once('.')?;
    let symbols = ctx.project.symbols();

    // Form 1: the prefix is an enum type name.
    let id = symbols.enum_by_name(prefix).or_else(|| {
        // Form 2: the prefix resolves to an enum-valued project symbol; the member
        // is qualified by the value source rather than the bare enum type name. A
        // value-compound (`GroupCompound`) carries its enum on its `.Value` child,
        // so consult that child's type when the symbol itself is untyped.
        let Target::Symbol(canon) = classify(
            prefix,
            ctx.group,
            ctx.fn_symbol,
            ctx.project,
            &ctx.env.locals,
        ) else {
            return None;
        };
        let enum_id_of = |path: &str| match symbols.get(path).map(|s| s.value_type) {
            Some(ValueType::Enum(id)) => Some(id),
            _ => None,
        };
        enum_id_of(&canon).or_else(|| enum_id_of(&format!("{canon}.Value")))
    })?;

    symbols.enum_has_member(id, leaf).then(|| Value::Enum {
        id,
        member: leaf.to_string(),
    })
}

/// Read a resolved project symbol's current value. The store depends on the
/// symbol kind: channels come from the value store (fail loud if unset), while
/// parameters come from calibration and constants come from their `.m1prj`
/// value (with an explicit `Env` override taking precedence). A table or group
/// has no scalar value.
///
/// A parameter is a tunable calibration value: its real value lives in a
/// `.m1cfg` export. When neither an `Env` override nor a loaded calibration
/// supplies one, it is an unseeded externally-driven input, like a CAN read, so
/// it resolves to the type-correct default for its declared type (flagged
/// externally driven in the trace), not a fail-loud abort. A constant's raw
/// `<Props Value>` is fixed by the project and is parsed directly into its
/// declared M1 storage family. A same-name calibration cell cannot replace it.
pub(crate) fn read_symbol(canon: &str, ctx: &mut EvalCtx) -> Result<Value, EvalError> {
    read_symbol_inner(canon, ctx, &mut HashSet::new())
}

fn read_symbol_inner(
    canon: &str,
    ctx: &mut EvalCtx,
    seen: &mut HashSet<String>,
) -> Result<Value, EvalError> {
    if !seen.insert(canon.to_string()) {
        return Err(EvalError::TypeError {
            detail: format!("cyclic default-value path while reading symbol {canon:?}"),
        });
    }

    // An explicit `Env` override (a pinned channel, a previously written value)
    // always wins — that is how computed channels read back what an earlier
    // statement wrote, and how scenario inputs are seeded.
    if let Some(v) = ctx.env.get(canon) {
        return Ok(v.clone());
    }

    let symbol = ctx.project.symbols().get(canon);
    let kind = symbol.map(|s| s.kind);
    match kind {
        Some(SymbolKind::Constant) => {
            let symbol = symbol.expect("a matched symbol kind has a symbol");
            if let Some(value) = project_constant_value(symbol, ctx.project)? {
                return Ok(value);
            }
            // Preserve the historical zero for a project that declares a
            // constant without a Value. It is project-owned, so it is not marked
            // as an external input and no calibration cell is consulted.
            Ok(typed_io_input_default(
                symbol.value_type,
                symbol.declared_type.as_deref(),
                ctx.project,
            ))
        }
        Some(SymbolKind::Parameter) => {
            // A loaded calibration value wins; otherwise default to the
            // parameter's declared-type zero (externally driven), so a
            // no-calibration run does not abort on the first tunable read. An
            // enum-typed parameter defaults to its enum's initial member, so an
            // `eq <Enum>.<Member>` comparison is type-correct.
            if let Some(v) = calib_param(canon, ctx.calib) {
                return coerce_for_channel(canon, v, ctx.project);
            }
            let symbol = symbol.expect("a matched symbol kind has a symbol");
            let default = typed_io_input_default(
                symbol.value_type,
                symbol.declared_type.as_deref(),
                ctx.project,
            );
            if let Some(trace) = ctx.trace.as_deref_mut() {
                trace.mark_external(canon);
            }
            Ok(default)
        }
        Some(SymbolKind::Channel) => {
            // An unseeded channel is a missing runtime input. In single-function /
            // cone mode the scenario must drive it, so this is fail-loud. In
            // whole-project mode (no scenario), it is an externally-driven input
            // (sensor/CAN/table-output/state channel) that falls back to its
            // type-correct startup default, flagged externally driven — never a
            // guessed *meaningful* value, only the determinate zero of its type.
            if ctx.env.default_unseeded_channels {
                let symbol = symbol.expect("a matched symbol kind has a symbol");
                let default = typed_io_input_default(
                    symbol.value_type,
                    symbol.declared_type.as_deref(),
                    ctx.project,
                );
                if let Some(trace) = ctx.trace.as_deref_mut() {
                    // Report the substitution honestly: the channel, the value
                    // GUESSED for it, and the script whose read triggered it.
                    trace.mark_defaulted(canon, default.clone(), ctx.script_name);
                    trace.mark_external(canon);
                }
                Ok(default)
            } else {
                Err(EvalError::MissingInput {
                    channel: canon.to_string(),
                })
            }
        }
        // A package *object* read directly as a value — a hardware IO input device
        // (`_IOMethod.av_switch` switch read `Driver.AUX Switch eq …`) whose value
        // is a documented state enum. The typechecker assigns it that enum type
        // (#173); offline it is an unseeded externally-driven hardware input, so it
        // resolves to that enum's initial state (`Off`/first member), flagged
        // externally driven — never a fail-loud abort. An object with no determinate
        // value type (a CAN message, a bare group) still has no scalar value.
        Some(SymbolKind::Object | SymbolKind::Reference | SymbolKind::Other)
            if symbol.map(has_determinate_default).unwrap_or(false) =>
        {
            let symbol = symbol.expect("a matched symbol kind has a symbol");
            let default = typed_io_input_default(
                symbol.value_type,
                symbol.declared_type.as_deref(),
                ctx.project,
            );
            if let Some(trace) = ctx.trace.as_deref_mut() {
                trace.mark_external(canon);
            }
            Ok(default)
        }
        // A GroupCompound may explicitly declare which nested symbol supplies
        // its scalar value (`UseDefValue="true" DefValue="This.Sensor"`). Resolve
        // that path in the group's own scope, then read through to the target.
        // The target may itself be a value-providing object or group, so retain
        // the cycle guard across the recursive read.
        Some(SymbolKind::Group) if symbol.and_then(|s| s.default_value.as_deref()).is_some() => {
            let default_value = symbol
                .and_then(|s| s.default_value.as_deref())
                .expect("guarded above")
                .to_string();
            let locals = HashMap::new();
            let Target::Symbol(value_path) =
                classify(&default_value, Some(canon), None, ctx.project, &locals)
            else {
                return Err(EvalError::UnresolvedSymbol {
                    name: format!("{canon} default value {default_value:?}"),
                });
            };
            read_symbol_inner(&value_path, ctx, seen)
        }
        // A symbol read directly by name whose value lives on its auto-created
        // `.Value` child: a `GroupCompound` value-compound (`Driveline.Accumulator
        // .Maximum Cell Temp`, marked `DefValue="This.Value"`), a `Table`
        // (`Control.Rear Torque Bias`, whose generated `Table.Lookup` writes
        // `.Value`), or a sensor/package `Object` (`Throttle Position.Tracking
        // .Discrete`, a `MoTeC Input.Sensor` whose reading is its `.Value`
        // channel). Reading the symbol reads through to that `.Value` child (the
        // same convention `enum_conv` uses for `.AsInteger`). This is reached only
        // after the typed-value Object arm above, so an enum-valued switch object
        // (read as its enum directly) is not diverted here. Recurse on
        // `<canon>.Value` when that child exists; a symbol with no `.Value` child
        // has no scalar value.
        Some(SymbolKind::Group | SymbolKind::Table | SymbolKind::Object)
            if ctx
                .project
                .symbols()
                .get(&format!("{canon}.Value"))
                .is_some() =>
        {
            let value_path = format!("{canon}.Value");
            read_symbol_inner(&value_path, ctx, seen)
        }
        // Tables/groups/untyped objects/functions are not scalar values.
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

/// Parse a constant's project-owned `<Props Value>` directly against its target
/// storage type. This is deliberately separate from source-literal inference:
/// for example, a `u32` constant may use `4294967295` without a trailing `u`.
fn project_constant_value(symbol: &Symbol, project: &Project) -> Result<Option<Value>, EvalError> {
    let Some(text) = symbol
        .static_value
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    else {
        return Ok(None);
    };

    let invalid = || EvalError::TypeError {
        detail: format!(
            "constant {:?} value {text:?} does not fit declared type {:?}",
            symbol.path, symbol.declared_type
        ),
    };

    if let ValueType::Enum(id) = symbol.value_type {
        let enum_type = project.symbols().enum_type(id);
        if enum_type.members.iter().any(|(member, _)| member == text) {
            return Ok(Some(Value::Enum {
                id,
                member: text.to_string(),
            }));
        }
        return Err(invalid());
    }

    if let Some(declared) = symbol.declared_type.as_deref() {
        let normalized = declared.to_ascii_lowercase();
        let value = match normalized.as_str() {
            "f32" | "f64" => {
                let narrowed = text.parse::<f32>().map_err(|_| invalid())?;
                if !narrowed.is_finite() {
                    return Err(invalid());
                }
                Value::m1_float(narrowed)
            }
            "s8" | "s16" | "s32" | "s64" => text
                .parse::<i32>()
                .ok()
                .map(Value::m1_integer)
                .ok_or_else(invalid)?,
            "u8" | "u16" | "u32" | "u64" => parse_uint(text)
                .map(Value::m1_unsigned)
                .map_err(|_| invalid())?,
            "fixedpoint7dps" | "fixed7dps" => {
                let fixed = FixedPoint7dps::parse_decimal(text).ok_or_else(invalid)?;
                Value::M1(M1Scalar::FixedPoint7dps(fixed))
            }
            "bool" => match text.to_ascii_lowercase().as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => return Err(invalid()),
            },
            "string" => Value::Str(text.to_string()),
            _ => {
                return Err(EvalError::TypeError {
                    detail: format!(
                        "constant {:?} has unsupported declared type {declared:?}",
                        symbol.path
                    ),
                });
            }
        };
        return Ok(Some(value));
    }

    // Older projects sometimes omit Type on numeric constants. Preserve that
    // input shape through a named compatibility rule while still narrowing the
    // inferred source-literal family to M1 width.
    if symbol.value_type == ValueType::Boolean {
        return match text.to_ascii_lowercase().as_str() {
            "true" => Ok(Some(Value::Bool(true))),
            "false" => Ok(Some(Value::Bool(false))),
            _ => Err(invalid()),
        };
    }
    if symbol.value_type == ValueType::String {
        return Ok(Some(Value::Str(text.to_string())));
    }
    let value = match type_of_number_literal(text) {
        ValueType::Unsigned => parse_uint(text).map(Value::m1_unsigned),
        ValueType::Float => {
            let narrowed = text.parse::<f32>().map_err(|_| invalid())?;
            if !narrowed.is_finite() {
                return Err(invalid());
            }
            Ok(Value::m1_float(narrowed))
        }
        _ => text
            .parse::<i32>()
            .map(Value::m1_integer)
            .map_err(|_| bad_number(text)),
    }
    .map_err(|_| invalid())?;
    Ok(Some(value))
}

fn has_determinate_default(symbol: &Symbol) -> bool {
    symbol.value_type.is_known()
        || symbol.declared_type.as_deref().is_some_and(|declared| {
            is_fixed_point_type(declared) || declared.eq_ignore_ascii_case("string")
        })
}

/// The type-correct externally-driven default for an unseeded parameter of
/// declared type `value_type`. A determinate zero/false/empty, never a guessed
/// reading. An `Unknown`/`Enum`-typed tunable (no determinate scalar zero) falls
/// back to M1 `FloatingPoint(0.0)`, the numeric default real calibration cells
/// take. The raw declaration retains FixedPoint7dps when the upstream type
/// lattice reports it as `Unknown`.
fn typed_param_default(value_type: ValueType, declared_type: Option<&str>) -> Value {
    if declared_type.is_some_and(is_fixed_point_type) {
        return Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::ZERO));
    }
    if declared_type.is_some_and(|declared| declared.eq_ignore_ascii_case("string")) {
        return Value::Str(String::new());
    }
    match value_type {
        ValueType::Boolean => Value::Bool(false),
        ValueType::Integer => Value::m1_integer(0),
        ValueType::Unsigned => Value::m1_unsigned(0),
        ValueType::Float => Value::m1_float(0.0),
        ValueType::String => Value::Str(String::new()),
        // An enum-typed or untyped tunable has no determinate scalar zero; a
        // calibration cell is numeric, so default to the float zero.
        ValueType::Enum(_) | ValueType::Unknown => Value::m1_float(0.0),
    }
}

/// The type-correct initial value for an unseeded externally-driven IO-input
/// object (a hardware switch/sensor object read directly). Unlike a numeric
/// tunable, an enum-typed hardware input resolves to a proper [`Value::Enum`] of
/// its enum's initial state (the declared `default` member, else the first
/// member), so an `eq <Enum>.<Member>` comparison is type-correct. Scalar types
/// reuse [`typed_param_default`].
fn typed_io_input_default(
    value_type: ValueType,
    declared_type: Option<&str>,
    project: &Project,
) -> Value {
    match value_type {
        ValueType::Enum(id) => {
            let enum_type = project.symbols().enum_type(id);
            let member = enum_type
                .default
                .clone()
                .or_else(|| enum_type.members.first().map(|(name, _)| name.clone()));
            match member {
                Some(member) => Value::Enum { id, member },
                // A member-less (open firmware) enum has no determinate offline
                // member; fall back to the numeric zero rather than invent one.
                None => Value::m1_float(0.0),
            }
        }
        other => typed_param_default(other, declared_type),
    }
}

/// Coerce a value being written to channel/parameter `canon` to that symbol's
/// declared M1 storage family. Integer/unsigned assignment conversions operate
/// on the same 32-bit pattern, integral values widen to binary32 for float
/// targets, and an invalid float-to-integral narrowing fails loud. Numeric enum
/// writes resolve their exact declared member value.
pub(crate) fn coerce_for_channel(
    canon: &str,
    value: Value,
    project: &Project,
) -> Result<Value, EvalError> {
    let Some(symbol) = project.symbols().get(canon) else {
        return Ok(value);
    };
    if symbol
        .declared_type
        .as_deref()
        .is_some_and(is_fixed_point_type)
    {
        return coerce_for_scalar_kind(canon, value, M1ScalarKind::FixedPoint7dps);
    }
    coerce_for_declared_type(canon, value, symbol.value_type, project)
}

/// Coerce a value at an assignment or call boundary to its declared M1 type.
/// This is shared by project channels, typed locals, user-function parameters,
/// and return slots so a value cannot change storage family between them.
pub(crate) fn coerce_for_declared_type(
    target: &str,
    value: Value,
    declared: ValueType,
    project: &Project,
) -> Result<Value, EvalError> {
    match (&value, declared) {
        (_, ValueType::Unknown) => return Ok(value),
        (Value::Bool(_), ValueType::Boolean) | (Value::Str(_), ValueType::String) => {
            return Ok(value);
        }
        (Value::Enum { id, .. }, ValueType::Enum(expected)) if *id == expected => {
            return Ok(value);
        }
        _ => {}
    }

    let scalar = match &value {
        Value::M1(scalar) => *scalar,
        _ => {
            return Err(declared_type_error(
                target,
                &value,
                declared_type_name(declared),
            ));
        }
    };

    match declared {
        ValueType::Integer => coerce_for_scalar_kind(target, value, M1ScalarKind::Integer),
        ValueType::Unsigned => coerce_for_scalar_kind(target, value, M1ScalarKind::UnsignedInteger),
        ValueType::Float => coerce_for_scalar_kind(target, value, M1ScalarKind::FloatingPoint),
        ValueType::Enum(id) => {
            let member = project
                .symbols()
                .enum_type(id)
                .members
                .iter()
                .find(|(_, declared)| scalar_matches_enum_value(scalar, *declared))
                .map(|(name, _)| name.clone());
            match member {
                Some(member) => Ok(Value::Enum { id, member }),
                None => Err(declared_type_error(
                    target,
                    &value,
                    "a declared enum member",
                )),
            }
        }
        ValueType::Boolean | ValueType::String | ValueType::Unknown => Err(declared_type_error(
            target,
            &value,
            declared_type_name(declared),
        )),
    }
}

fn scalar_matches_enum_value(scalar: M1Scalar, declared: i64) -> bool {
    match scalar {
        M1Scalar::Integer(value) => i64::from(value) == declared,
        M1Scalar::UnsignedInteger(value) => u32::try_from(declared) == Ok(value),
        M1Scalar::FloatingPoint(value) => i32::try_from(declared)
            .ok()
            .is_some_and(|declared| f64::from(declared) == f64::from(value)),
        M1Scalar::FixedPoint7dps(value) => declared
            .checked_mul(FixedPoint7dps::SCALE)
            .is_some_and(|raw| raw == i64::from(value.raw())),
    }
}

/// Coerce a value to an exact runtime scalar family. Existing-value assignment
/// uses this when the static type model cannot distinguish `FloatingPoint` from
/// `FixedPoint7dps`.
pub(crate) fn coerce_for_scalar_kind(
    target: &str,
    value: Value,
    kind: M1ScalarKind,
) -> Result<Value, EvalError> {
    let scalar = value.m1_scalar()?;
    let converted = match (kind, scalar) {
        (M1ScalarKind::Integer, M1Scalar::Integer(value)) => M1Scalar::Integer(value),
        (M1ScalarKind::Integer, M1Scalar::UnsignedInteger(value)) => {
            M1Scalar::Integer(value as i32)
        }
        (M1ScalarKind::UnsignedInteger, M1Scalar::Integer(value)) => {
            M1Scalar::UnsignedInteger(value as u32)
        }
        (M1ScalarKind::UnsignedInteger, M1Scalar::UnsignedInteger(value)) => {
            M1Scalar::UnsignedInteger(value)
        }
        (M1ScalarKind::FloatingPoint, scalar) => M1Scalar::FloatingPoint(scalar.as_f32()),
        (M1ScalarKind::FixedPoint7dps, M1Scalar::FixedPoint7dps(value)) => {
            M1Scalar::FixedPoint7dps(value)
        }
        _ => return Err(declared_type_error(target, &value, scalar_kind_name(kind))),
    };
    Ok(Value::M1(converted))
}

fn declared_type_name(value_type: ValueType) -> &'static str {
    match value_type {
        ValueType::Boolean => "Boolean",
        ValueType::Integer => "Integer",
        ValueType::Unsigned => "UnsignedInteger",
        ValueType::Float => "FloatingPoint",
        ValueType::String => "String",
        ValueType::Enum(_) => "Enumeration",
        ValueType::Unknown => "a known type",
    }
}

fn scalar_kind_name(kind: M1ScalarKind) -> &'static str {
    match kind {
        M1ScalarKind::FloatingPoint => "FloatingPoint",
        M1ScalarKind::Integer => "Integer",
        M1ScalarKind::UnsignedInteger => "UnsignedInteger",
        M1ScalarKind::FixedPoint7dps => "FixedPoint7dps",
    }
}

fn is_fixed_point_type(declared: &str) -> bool {
    declared.eq_ignore_ascii_case("FixedPoint7dps") || declared.eq_ignore_ascii_case("fixed7dps")
}

fn declared_type_error(target: &str, value: &Value, required: &str) -> EvalError {
    EvalError::TypeError {
        detail: format!("cannot store {value:?} in {target:?}, which requires {required}"),
    }
}

/// Look up a parameter/constant calibration value by its canonical symbol path.
/// Real `.m1cfg` exports omit the implicit leading `Root.` group prefix that the
/// symbol table uses, so try the canonical path first, then the `Root.`-stripped
/// form. Calibration values already carry their M1 storage type.
fn calib_param(canon: &str, calib: &Calibration) -> Option<Value> {
    calib
        .param(canon)
        .or_else(|| canon.strip_prefix("Root.").and_then(|p| calib.param(p)))
        .map(Value::M1)
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
    // `2147483648` is outside positive i32, but its negation is the valid M1
    // minimum. Recognise that one grammar shape before evaluating the positive
    // child literal; every larger magnitude still fails the width check.
    if op.kind() == Kind::Minus
        && operand.kind() == Kind::Number
        && type_of_number_literal(operand.text().trim()) == ValueType::Integer
        && operand.text().trim().parse::<u32>().ok() == Some(i32::MAX as u32 + 1)
    {
        return Ok(Value::m1_integer(i32::MIN));
    }
    let v = eval(&operand, ctx)?;
    match op.kind() {
        Kind::Minus => match v {
            Value::M1(M1Scalar::Integer(x)) => Ok(Value::m1_integer(x.wrapping_neg())),
            Value::M1(M1Scalar::FloatingPoint(x)) => Ok(Value::m1_float(-x)),
            Value::M1(M1Scalar::FixedPoint7dps(x)) => Ok(Value::M1(M1Scalar::FixedPoint7dps(
                crate::value::FixedPoint7dps::from_raw(x.raw().wrapping_neg()),
            ))),
            Value::M1(M1Scalar::UnsignedInteger(x)) => Ok(Value::m1_unsigned(x.wrapping_neg())),
            other => Err(EvalError::TypeError {
                detail: format!("cannot negate {other:?}"),
            }),
        },
        // `not` and `!` are logical negation: boolean only (M1 is strongly typed).
        Kind::Not | Kind::Bang => Ok(Value::Bool(!v.as_bool()?)),
        // `~` is bitwise complement: integral only.
        Kind::Tilde => match v {
            Value::M1(M1Scalar::Integer(x)) => Ok(Value::m1_integer(!x)),
            Value::M1(M1Scalar::UnsignedInteger(x)) => Ok(Value::m1_unsigned(!x)),
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

    // The remaining operators (arithmetic, comparison, equality, bitwise/shift)
    // operate on the two evaluated values; share that core with the compound
    // assignment operators. An unhandled token reports its byte offset.
    apply_binary_values(kind, &l, &r).map_err(|e| match e {
        EvalError::UnsupportedConstruct { kind, .. } => EvalError::UnsupportedConstruct {
            kind,
            at: op.byte_range().start,
        },
        other => other,
    })
}

/// Apply a binary operator to two already-evaluated values, reusing the same
/// arithmetic/comparison/equality/bitwise semantics as the expression evaluator.
/// This is the shared core behind the binary-expression branch and the compound
/// assignment operators (`+=`, `&=`, …). Logical/short-circuit operators are
/// intentionally excluded — they are handled in [`eval_binary`] before both
/// operands are evaluated, and compound assignment never targets them.
/// Integer `/` and `%` by zero always fail loud, in every mode: an arithmetic
/// error is not a missing input, so it is never converted into a value — not
/// even under the whole-project `allow_default_inputs` opt-in (whose scope is
/// unseeded *reads* only). No firmware documentation substantiates a
/// divide-by-zero-yields-zero behaviour; if that evidence ever appears, the
/// behaviour belongs here unconditionally with the citation, not gated on
/// offline defaulting.
pub(crate) fn apply_binary_values(op: Kind, l: &Value, r: &Value) -> Result<Value, EvalError> {
    match op {
        Kind::Plus | Kind::Minus | Kind::Star | Kind::Slash | Kind::Percent => arithmetic(op, l, r),
        Kind::Lt | Kind::Gt | Kind::LtEq | Kind::GtEq => compare(op, l, r),
        Kind::Eq | Kind::EqEq => Ok(Value::Bool(values_equal(l, r)?)),
        Kind::Neq | Kind::BangEq => Ok(Value::Bool(!values_equal(l, r)?)),
        Kind::Amp | Kind::Pipe | Kind::Caret | Kind::LtLt | Kind::GtGt => bitwise(op, l, r),
        other => Err(EvalError::UnsupportedConstruct {
            kind: format!("binary operator {other:?}"),
            at: 0,
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
            let a = l.m1_scalar()?.as_f32();
            let b = r.m1_scalar()?.as_f32();
            let out = match op {
                Kind::Plus => a + b,
                Kind::Minus => a - b,
                Kind::Star => a * b,
                Kind::Slash => a / b,
                Kind::Percent => a % b,
                _ => unreachable!("arithmetic called with non-arith op"),
            };
            Ok(Value::m1_float(out))
        }
        ValueType::Unsigned => {
            let a = as_u32_bits(l)?;
            let b = as_u32_bits(r)?;
            int_op_u32(op, a, b)
        }
        ValueType::Integer => {
            let a = as_i32(l)?;
            let b = as_i32(r)?;
            int_op_i32(op, a, b)
        }
        // One operand is non-numeric (Bool/Enum/String) or Unknown.
        _ => Err(EvalError::TypeError {
            detail: format!("arithmetic on non-numeric operands {l:?} and {r:?}"),
        }),
    }
}

fn int_op_i32(op: Kind, a: i32, b: i32) -> Result<Value, EvalError> {
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
    Ok(Value::m1_integer(out))
}

fn int_op_u32(op: Kind, a: u32, b: u32) -> Result<Value, EvalError> {
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
    Ok(Value::m1_unsigned(out))
}

fn div_by_zero() -> EvalError {
    EvalError::TypeError {
        detail: "division or modulo by zero".to_string(),
    }
}

/// Apply an ordered comparison (`< > <= >=`) after the same M1-width promotion
/// used by arithmetic. Mixed signed/unsigned operands compare as `u32`; an M1
/// float in either position makes both operands binary32.
fn compare(op: Kind, l: &Value, r: &Value) -> Result<Value, EvalError> {
    if let (Value::M1(M1Scalar::FixedPoint7dps(left)), Value::M1(M1Scalar::FixedPoint7dps(right))) =
        (l, r)
    {
        return Ok(Value::Bool(compare_values(op, left.raw(), right.raw())));
    }
    let out = match numeric_join(value_type(l), value_type(r)) {
        ValueType::Float => compare_values(op, l.m1_scalar()?.as_f32(), r.m1_scalar()?.as_f32()),
        ValueType::Unsigned => compare_values(op, as_u32_bits(l)?, as_u32_bits(r)?),
        ValueType::Integer => compare_values(op, as_i32(l)?, as_i32(r)?),
        _ => {
            return Err(EvalError::TypeError {
                detail: format!("comparison on non-numeric operands {l:?} and {r:?}"),
            });
        }
    };
    Ok(Value::Bool(out))
}

fn compare_values<T: PartialOrd>(op: Kind, a: T, b: T) -> bool {
    match op {
        Kind::Lt => a < b,
        Kind::Gt => a > b,
        Kind::LtEq => a <= b,
        Kind::GtEq => a >= b,
        _ => unreachable!("compare_values called with non-comparison op"),
    }
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
        (M1(M1Scalar::FixedPoint7dps(left)), M1(M1Scalar::FixedPoint7dps(right))) => {
            Ok(left.raw() == right.raw())
        }
        (M1(_), M1(_)) => match numeric_join(value_type(l), value_type(r)) {
            ValueType::Float => Ok(l.m1_scalar()?.as_f32() == r.m1_scalar()?.as_f32()),
            ValueType::Unsigned => Ok(as_u32_bits(l)? == as_u32_bits(r)?),
            ValueType::Integer => Ok(as_i32(l)? == as_i32(r)?),
            _ => unreachable!("M1 scalars always have a numeric ValueType"),
        },
        _ => Err(EvalError::TypeError {
            detail: format!("cannot compare {l:?} with {r:?} for equality"),
        }),
    }
}

/// Apply a bitwise/shift operator. Operands must be integral (signed or unsigned);
/// a non-integral operand is a type error. Mixed signed/unsigned operands are
/// allowed — real M1 code freely combines them (`(Status Word >> 8) & 0x01`, an
/// `s32` masked with a hex `u32`). Bit operations act on the two's-complement bit
/// pattern. M1's result type follows the left operand, including the signedness
/// of a right shift; the right operand contributes only its 32-bit pattern or
/// shift count.
fn bitwise(op: Kind, l: &Value, r: &Value) -> Result<Value, EvalError> {
    let right = as_u32_bits(r)?;
    match l {
        Value::M1(M1Scalar::UnsignedInteger(left)) => {
            Ok(Value::m1_unsigned(bit_u32(op, *left, right)))
        }
        Value::M1(M1Scalar::Integer(left)) => Ok(Value::m1_integer(bit_i32(op, *left, right))),
        other => Err(EvalError::TypeError {
            detail: format!("bitwise operator requires integral operands, got {other:?} and {r:?}"),
        }),
    }
}

fn bit_u32(op: Kind, a: u32, b: u32) -> u32 {
    match op {
        Kind::Amp => a & b,
        Kind::Pipe => a | b,
        Kind::Caret => a ^ b,
        Kind::LtLt => a.wrapping_shl(b),
        Kind::GtGt => a.wrapping_shr(b),
        _ => unreachable!("bit_u32 called with non-bitwise op"),
    }
}

fn bit_i32(op: Kind, a: i32, b: u32) -> i32 {
    match op {
        Kind::Amp => a & b as i32,
        Kind::Pipe => a | b as i32,
        Kind::Caret => a ^ b as i32,
        Kind::LtLt => a.wrapping_shl(b),
        Kind::GtGt => a.wrapping_shr(b),
        _ => unreachable!("bit_i32 called with non-bitwise op"),
    }
}

/// The [`ValueType`] of a runtime value, for `numeric_join`-driven arithmetic
/// result typing. Non-numeric values map to their lattice type.
fn value_type(v: &Value) -> ValueType {
    match v {
        Value::Bool(_) => ValueType::Boolean,
        Value::M1(M1Scalar::Integer(_)) => ValueType::Integer,
        Value::M1(M1Scalar::UnsignedInteger(_)) => ValueType::Unsigned,
        Value::M1(M1Scalar::FloatingPoint(_) | M1Scalar::FixedPoint7dps(_)) => ValueType::Float,
        Value::Enum { id, .. } => ValueType::Enum(*id),
        Value::Str(_) => ValueType::String,
    }
}

/// Extract a signed M1 integer.
fn as_i32(v: &Value) -> Result<i32, EvalError> {
    match v {
        Value::M1(M1Scalar::Integer(x)) => Ok(*x),
        other => Err(EvalError::TypeError {
            detail: format!("{other:?} is not an integer"),
        }),
    }
}

/// Read an integral M1 value as its 32-bit bit pattern. Signed operands use the
/// language's mixed-integer conversion to unsigned.
fn as_u32_bits(v: &Value) -> Result<u32, EvalError> {
    match v {
        Value::M1(M1Scalar::UnsignedInteger(x)) => Ok(*x),
        Value::M1(M1Scalar::Integer(x)) => Ok(*x as u32),
        other => Err(EvalError::TypeError {
            detail: format!("{other:?} is not an integral M1 value"),
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

    let result = match callee.kind() {
        Kind::MemberExpression => {
            let object_node = callee
                .child_by_field(Field::Object)
                .ok_or_else(|| op_shape_err(&callee, "call object"))?;
            let method_node = callee
                .child_by_field(Field::Property)
                .ok_or_else(|| op_shape_err(&callee, "call method"))?;
            let method = method_node.text();

            // Runtime and coverage share the project-aware capability model in
            // `builtins::dispatch`. It resolves a script-backed user function
            // before library and project-object routes, so a user `Update` does
            // not collapse into the similarly named IO stub.
            let object = match object_node.kind() {
                Kind::MemberExpression => flatten_member(&object_node)?,
                _ => object_node.text().to_string(),
            };
            crate::builtins::dispatch(&object, method, &args, site.clone(), ctx)?
        }
        // A bare-identifier callee `Update(...)` is an inline user-function call
        // (the callee names a project `Function`/`Method` symbol directly). Route
        // it through `userfn::call`; a name that is not a user function fails loud
        // rather than guessing (it is neither a library object nor a value).
        Kind::Identifier => {
            let name = callee.text();
            crate::builtins::dispatch_bare(name, &args, site.clone(), ctx)?
        }
        _ => {
            return Err(EvalError::UnsupportedConstruct {
                kind: "unsupported call callee".to_string(),
                at: node.byte_range().start,
            });
        }
    };

    // Record the call's value at its call site for the value overlay.
    if let Some(trace) = ctx.trace.as_deref_mut() {
        trace.record_expr((site.script().to_string(), site.offset()), result.clone());
    }
    Ok(result)
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
                scripts: &[],
                signature_m1_types: None,
                object_rules: None,
                depth: 0,
                trace: None,
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
        assert_eq!(rhs_value("2.5", &mut h).unwrap(), Value::m1_float(2.5));
        assert_eq!(rhs_value("7", &mut h).unwrap(), Value::m1_integer(7));
        assert_eq!(rhs_value("0xFF", &mut h).unwrap(), Value::m1_unsigned(255));
        assert_eq!(rhs_value("10u", &mut h).unwrap(), Value::m1_unsigned(10));
        assert_eq!(rhs_value("1e3", &mut h).unwrap(), Value::m1_float(1000.0));
    }

    #[test]
    fn exact_target_directs_fixed_point_assignment() {
        let h = Harness::new();
        let fixed = Value::M1(M1Scalar::FixedPoint7dps(
            crate::value::FixedPoint7dps::from_raw(12_345_678),
        ));
        assert_eq!(
            coerce_for_declared_type("target", fixed.clone(), ValueType::Float, &h.project)
                .unwrap(),
            Value::m1_float(1.2345678)
        );
        assert_eq!(
            coerce_for_scalar_kind("target", fixed.clone(), M1ScalarKind::FixedPoint7dps).unwrap(),
            fixed
        );
    }

    #[test]
    fn float_to_enum_coercion_requires_an_exact_integer_value() {
        assert!(scalar_matches_enum_value(
            M1Scalar::FloatingPoint(16_777_216.0),
            16_777_216
        ));
        assert!(!scalar_matches_enum_value(
            M1Scalar::FloatingPoint(16_777_216.0),
            16_777_217
        ));
        assert!(!scalar_matches_enum_value(
            M1Scalar::FloatingPoint(i32::MAX as f32),
            i64::from(i32::MAX)
        ));
    }

    #[test]
    fn project_target_metadata_distinguishes_float_and_fixed_point() {
        let h = Harness::new();
        let fixed = Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(
            12_345_678,
        )));
        assert_eq!(
            coerce_for_channel("Root.Demo.FloatTarget", fixed.clone(), &h.project).unwrap(),
            Value::m1_float(1.2345678)
        );
        assert_eq!(
            coerce_for_channel("Root.Demo.FixedTarget", fixed.clone(), &h.project).unwrap(),
            fixed
        );
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
        assert_eq!(rhs_value("(2.5)", &mut h).unwrap(), Value::m1_float(2.5));
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
        h.env.set("Root.Demo.Speed", Value::m1_float(42.0));
        assert_eq!(rhs_value("Speed", &mut h).unwrap(), Value::m1_float(42.0));
    }

    #[test]
    fn parameter_identifier_reads_calibration() {
        let mut h = Harness::new();
        // No calibration value: a parameter is a tunable calibration value, so an
        // unseeded read defaults to its declared-type zero (externally driven),
        // rather than aborting a no-calibration run. `Gain` is a float parameter.
        assert_eq!(rhs_value("Gain", &mut h).unwrap(), Value::m1_float(0.0));
        // Provide it under the Root-stripped name real exports use; calibration
        // now wins over the default.
        h.calib
            .params
            .insert("Demo.Gain".to_string(), M1Scalar::Integer(2));
        assert_eq!(rhs_value("Gain", &mut h).unwrap(), Value::m1_float(2.0));
        h.calib
            .params
            .insert("Demo.Gain".to_string(), M1Scalar::FloatingPoint(2.5));
        assert_eq!(rhs_value("Gain", &mut h).unwrap(), Value::m1_float(2.5));
    }

    #[test]
    fn fixed_point_defaults_retain_the_declared_project_family() {
        let mut h = Harness::new();
        assert_eq!(
            rhs_value("FixedGain", &mut h).unwrap(),
            Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::ZERO))
        );

        h.env.default_unseeded_channels = true;
        assert_eq!(
            rhs_value("FixedTarget", &mut h).unwrap(),
            Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::ZERO))
        );

        // An untyped calibration cell parses as binary32. It cannot
        // silently change an explicitly fixed parameter's storage family.
        h.calib
            .params
            .insert("Demo.FixedGain".to_string(), M1Scalar::FloatingPoint(1.0));
        assert!(matches!(
            rhs_value("FixedGain", &mut h),
            Err(EvalError::TypeError { .. })
        ));
    }

    #[test]
    fn project_constants_are_target_typed_and_outrank_calibration() {
        let mut h = Harness::new();
        h.calib.params.insert(
            "Demo.UnsignedConstant".to_string(),
            M1Scalar::UnsignedInteger(7),
        );

        assert_eq!(
            rhs_value("SignedConstant", &mut h).unwrap(),
            Value::m1_integer(i32::MIN)
        );
        assert_eq!(
            rhs_value("UnsignedConstant", &mut h).unwrap(),
            Value::m1_unsigned(u32::MAX)
        );
        assert_eq!(
            rhs_value("FloatConstant", &mut h).unwrap(),
            Value::m1_float(16_777_216.0)
        );
        assert_eq!(
            rhs_value("FixedConstant", &mut h).unwrap(),
            Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(
                12_345_678,
            )))
        );
        assert_eq!(
            rhs_value("LegacyConstant", &mut h).unwrap(),
            Value::m1_integer(100)
        );
        assert_eq!(
            rhs_value("BooleanConstant", &mut h).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn out_of_range_project_constants_fail_loud() {
        let h = Harness::new();
        let mut symbol = h
            .project
            .symbols()
            .get("Root.Demo.SignedConstant")
            .unwrap()
            .clone();
        for (declared, raw) in [
            ("s32", "2147483648"),
            ("u32", "4294967296"),
            ("f32", "1e39"),
            ("f32", "1e9999"),
            ("FixedPoint7dps", "214.7483648"),
        ] {
            symbol.declared_type = Some(declared.to_string());
            symbol.static_value = Some(raw.to_string());
            assert!(
                project_constant_value(&symbol, &h.project).is_err(),
                "unexpectedly accepted {declared} constant {raw}"
            );
        }
    }

    #[test]
    fn group_default_value_reads_declared_nested_symbol() {
        let mut h = Harness::new();
        h.env
            .set("Root.Demo.Sensor Compound.Sensor", Value::m1_float(87.5));
        assert_eq!(
            rhs_value("Sensor Compound", &mut h).unwrap(),
            Value::m1_float(87.5)
        );
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
        h.env.set_local("scaled", Value::m1_integer(9));
        assert_eq!(rhs_value("scaled", &mut h).unwrap(), Value::m1_integer(9));
    }

    // ---- Task 9: member expressions ----

    #[test]
    fn this_member_rewrites_to_group() {
        let mut h = Harness::new();
        // `This.Output` from group Root.Demo resolves to Root.Demo.Output.
        h.env.set("Root.Demo.Output", Value::m1_float(3.0));
        assert_eq!(
            rhs_value("This.Output", &mut h).unwrap(),
            Value::m1_float(3.0)
        );
    }

    #[test]
    fn absolute_member_path_reads() {
        let mut h = Harness::new();
        h.env.set("Root.Sibling", Value::m1_float(11.0));
        assert_eq!(
            rhs_value("Root.Sibling", &mut h).unwrap(),
            Value::m1_float(11.0)
        );
    }

    #[test]
    fn parent_member_walks_up() {
        let mut h = Harness::new();
        h.env.set("Root.Sibling", Value::m1_float(5.0));
        // From Root.Demo, Parent.Sibling is Root.Sibling.
        assert_eq!(
            rhs_value("Parent.Sibling", &mut h).unwrap(),
            Value::m1_float(5.0)
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
        assert_eq!(rhs_value("2 + 3", &mut h).unwrap(), Value::m1_integer(5));
        assert_eq!(rhs_value("2 * 3", &mut h).unwrap(), Value::m1_integer(6));
        assert_eq!(rhs_value("7 % 3", &mut h).unwrap(), Value::m1_integer(1));
        // A float operand promotes the result to float (numeric_join).
        assert_eq!(rhs_value("2 + 1.5", &mut h).unwrap(), Value::m1_float(3.5));
        assert_eq!(
            rhs_value("3.0 / 2.0", &mut h).unwrap(),
            Value::m1_float(1.5)
        );
    }

    #[test]
    fn unsigned_arithmetic_stays_unsigned() {
        let mut h = Harness::new();
        assert_eq!(
            rhs_value("10u + 5u", &mut h).unwrap(),
            Value::m1_unsigned(15)
        );
    }

    #[test]
    fn m1_integer_arithmetic_wraps_at_32_bits() {
        let mut h = Harness::new();
        assert_eq!(
            rhs_value("2147483647 + 1", &mut h).unwrap(),
            Value::m1_integer(i32::MIN)
        );
        assert_eq!(
            rhs_value("0u - 1u", &mut h).unwrap(),
            Value::m1_unsigned(u32::MAX)
        );
        assert_eq!(
            rhs_value("-1 + 0u", &mut h).unwrap(),
            Value::m1_unsigned(u32::MAX)
        );
        assert_eq!(rhs_value("-1 < 0u", &mut h).unwrap(), Value::Bool(false));
    }

    #[test]
    fn binary32_rounding_happens_at_each_expression_operation() {
        let mut h = Harness::new();
        assert_eq!(
            rhs_value("16777216.0 + 1.0", &mut h).unwrap(),
            Value::m1_float(16_777_216.0)
        );
        assert_eq!(rhs_value("1e-50", &mut h).unwrap(), Value::m1_float(0.0));
        assert!(rhs_value("1e39", &mut h).is_err());
        assert!(rhs_value("1e9999", &mut h).is_err());
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
    fn division_by_zero_fails_loud_even_with_default_inputs_enabled() {
        // The allow-default-inputs opt-in covers unseeded READS only. An
        // arithmetic error is not a missing input: integer divide/modulo by
        // zero fails loud in every mode — it is never converted to 0 because
        // offline defaulting happens to be on (2026-07-19 review, B6).
        let mut h = Harness::new();
        h.env.default_unseeded_channels = true;
        assert!(matches!(
            rhs_value("1 / 0", &mut h),
            Err(EvalError::TypeError { .. })
        ));
        assert!(matches!(
            rhs_value("7 % 0", &mut h),
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
    fn fixed_point_comparison_uses_exact_raw_storage() {
        let left = Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(
            1_000_000_000,
        )));
        let right = Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(
            1_000_000_001,
        )));
        assert_eq!(compare(Kind::Lt, &left, &right).unwrap(), Value::Bool(true));
        assert!(!values_equal(&left, &right).unwrap());
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
        assert_eq!(rhs_value("-5", &mut h).unwrap(), Value::m1_integer(-5));
        assert_eq!(
            rhs_value("-2147483648", &mut h).unwrap(),
            Value::m1_integer(i32::MIN)
        );
        assert!(rhs_value("-2147483649", &mut h).is_err());
        assert_eq!(rhs_value("-2.5", &mut h).unwrap(), Value::m1_float(-2.5));
        assert_eq!(rhs_value("not true", &mut h).unwrap(), Value::Bool(false));
        assert_eq!(rhs_value("!false", &mut h).unwrap(), Value::Bool(true));
    }

    #[test]
    fn bitwise_and_shift() {
        let mut h = Harness::new();
        assert_eq!(
            rhs_value("12u & 10u", &mut h).unwrap(),
            Value::m1_unsigned(8)
        );
        assert_eq!(
            rhs_value("12u | 1u", &mut h).unwrap(),
            Value::m1_unsigned(13)
        );
        assert_eq!(rhs_value("6u ^ 3u", &mut h).unwrap(), Value::m1_unsigned(5));
        assert_eq!(
            rhs_value("1u << 4u", &mut h).unwrap(),
            Value::m1_unsigned(16)
        );
        assert_eq!(
            rhs_value("16u >> 2u", &mut h).unwrap(),
            Value::m1_unsigned(4)
        );
        assert_eq!(
            rhs_value("~0u", &mut h).unwrap(),
            Value::m1_unsigned(u32::MAX)
        );
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
    fn bitwise_mixed_signed_unsigned_is_allowed() {
        let mut h = Harness::new();
        // Real M1 code masks a signed value with a hex (unsigned) literal, e.g.
        // `(Status Word >> 8) & 0x01`. Mixed integral operands are allowed; the
        // result follows the left operand, and the bit pattern is preserved.
        assert_eq!(rhs_value("13 & 6u", &mut h).unwrap(), Value::m1_integer(4));
        assert_eq!(rhs_value("6u & 13", &mut h).unwrap(), Value::m1_unsigned(4));
        // A shift of a signed value by an unsigned count stays signed, including
        // arithmetic right-shift behavior for a negative left operand.
        assert_eq!(
            rhs_value("256 >> 4u", &mut h).unwrap(),
            Value::m1_integer(16)
        );
        assert_eq!(
            rhs_value("-8 >> 1u", &mut h).unwrap(),
            Value::m1_integer(-4)
        );
    }

    #[test]
    fn operator_precedence_via_grammar() {
        let mut h = Harness::new();
        // 2 + 3 * 4 = 14 (the grammar nests the multiply tighter).
        assert_eq!(
            rhs_value("2 + 3 * 4", &mut h).unwrap(),
            Value::m1_integer(14)
        );
        // Parentheses override: (2 + 3) * 4 = 20.
        assert_eq!(
            rhs_value("(2 + 3) * 4", &mut h).unwrap(),
            Value::m1_integer(20)
        );
    }

    // ---- Task 11: ternary + call dispatch ----

    #[test]
    fn ternary_picks_branch() {
        let mut h = Harness::new();
        assert_eq!(
            rhs_value("true ? 1 : 2", &mut h).unwrap(),
            Value::m1_integer(1)
        );
        assert_eq!(
            rhs_value("false ? 1 : 2", &mut h).unwrap(),
            Value::m1_integer(2)
        );
        // The non-taken branch is not evaluated: an undefined channel there is fine.
        assert_eq!(
            rhs_value("true ? 7 : Speed", &mut h).unwrap(),
            Value::m1_integer(7)
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
            Value::m1_integer(3)
        );
    }

    #[test]
    fn corrected_pure_builtins_preserve_m1_scalars_end_to_end() {
        let mut h = Harness::new();
        assert_eq!(
            rhs_value("Calculate.Average(1, 2)", &mut h).unwrap(),
            Value::m1_integer(1)
        );
        assert_eq!(
            rhs_value("Calculate.Average(1.0, 2.0)", &mut h).unwrap(),
            Value::m1_float(1.5)
        );
        assert_eq!(
            rhs_value("Calculate.Bias(20.0, 10.0, -0.5)", &mut h).unwrap(),
            Value::m1_float(12.5)
        );
        assert_eq!(
            rhs_value("Calculate.MaximumFloat()", &mut h).unwrap(),
            Value::m1_float(f32::MAX)
        );
        assert_eq!(
            rhs_value("Convert.ToInteger(-2.5)", &mut h).unwrap(),
            Value::m1_integer(-3)
        );
        assert_eq!(
            rhs_value("Convert.ToUnsignedInteger(-2.6)", &mut h).unwrap(),
            Value::m1_unsigned(0)
        );
        assert_eq!(
            rhs_value("Convert.ToFixed7DP(1)", &mut h).unwrap(),
            Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(
                10_000_000,
            )))
        );
    }

    #[test]
    fn unimplemented_builtin_call_still_fails_loud() {
        let mut h = Harness::new();
        // A buffered sample-delay (Delay.Signal15) is intentionally not
        // implemented in Phase 1, so a call to it must fail loud rather than
        // no-op — the stateful object is recognised but this method is not.
        match rhs_value("Delay.Signal15(1.0, 3)", &mut h) {
            Err(EvalError::UnsupportedBuiltin { object, method }) => {
                assert_eq!(object, "Delay");
                assert_eq!(method, "Signal15");
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
