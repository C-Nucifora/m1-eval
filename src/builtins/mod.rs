// SPDX-License-Identifier: GPL-3.0-or-later
//! Builtin call dispatch.
//!
//! Every `Object.Method(...)` builtin call in an M1 script routes through
//! [`dispatch`]. M5 wires the *pure* builtins:
//!
//! - the math/clamp/convert library objects `Calculate.*`, `Limit.*`,
//!   `Convert.*` (see the [`calculate`], [`limit`], [`convert`] submodules), and
//! - table `.Lookup()` interpolation over the loaded calibration.
//!
//! Arity is validated up front against `m1_typecheck::intrinsics` (the builtin
//! *signature* registry): a call whose argument count matches no overload of the
//! named method is a fail-loud [`EvalError::BadCall`]; a method the registry does
//! not list on the object is an [`EvalError::UnsupportedBuiltin`].
//!
//! The stateful operators (`Filter`/`Integral`/`Delay`/… and the stateful
//! `Calculate.{Stable,Hysteresis,Between,Beyond}`) and the Tier-3 IO objects
//! arrive in later milestones; until then they match no implemented branch here
//! and fall through to the fail-loud default. That default is the whole point:
//! an unimplemented builtin must surface as an error, never a guessed number.

pub mod calculate;
pub mod convert;
pub mod enum_conv;
pub mod io_stub;
pub mod limit;
pub mod object;
pub mod stateful;
pub mod userfn;

use crate::env::CallSite;
use crate::error::EvalError;
use crate::expr::EvalCtx;
use crate::ident::{Target, classify};
use crate::value::{M1Scalar, Value};
use m1_typecheck::Project;
use m1_typecheck::intrinsics;
use m1_typecheck::parsed::ParsedScript;
use m1_typecheck::symbols::SymbolKind;
use std::collections::HashMap;

/// Dispatch one builtin call `object.method(args)`.
///
/// `object`/`method` are the source spellings of the callee's member parts;
/// `args` are the already-evaluated arguments (left to right). `site` is the
/// stable [`CallSite`] of the call node, which the stateful operators (M6) use
/// to key per-occurrence state. `ctx` carries the evaluation environment
/// (project model, calibration, value/state stores, `dt`, lexical context).
///
/// The shared capability model first resolves the receiver and selects the
/// concrete route: user function, library implementation, project table/enum/
/// channel/timer, documented IO stub, or fail-loud unsupported call.
pub fn dispatch(
    object: &str,
    method: &str,
    args: &[Value],
    site: CallSite,
    ctx: &mut EvalCtx,
) -> Result<Value, EvalError> {
    let capability = {
        let scope = CapabilityScope {
            project: Some(ctx.project),
            group: ctx.group,
            fn_symbol: ctx.fn_symbol,
            locals: Some(&ctx.env.locals),
            scripts: ctx.scripts,
        };
        classify_member_call(object, method, &scope)
    };

    match capability.route {
        CallRoute::UserFunction(canon) => match userfn::call(&canon, args, ctx)? {
            Some(value) => Ok(value),
            None => Err(unsupported(object, method)),
        },
        CallRoute::TableLookup => match try_table_lookup(object, args, ctx)? {
            Some(value) => Ok(value),
            None => Err(unsupported(object, method)),
        },
        CallRoute::TableGet => match try_table_get(object, args, ctx)? {
            Some(value) => Ok(value),
            None => Err(unsupported(object, method)),
        },
        CallRoute::PureLibrary(library_object) => {
            validate_arity(&library_object, object, method, args.len())?;
            match library_object.as_str() {
                "Calculate" => {
                    calculate::call(method, args)?.ok_or_else(|| unsupported(object, method))
                }
                "Convert" => {
                    convert::call(method, args)?.ok_or_else(|| unsupported(object, method))
                }
                "Limit" => limit::call(method, args)?.ok_or_else(|| unsupported(object, method)),
                _ => unreachable!(),
            }
        }
        CallRoute::StatefulLibrary(library_object) => {
            validate_arity(&library_object, object, method, args.len())?;
            match stateful::call(&library_object, method, args, site, ctx)? {
                Some(v) => Ok(v),
                None => Err(unsupported(object, method)),
            }
        }
        CallRoute::IoLibrary(library_object) => {
            io_stub::call(&library_object, object, method, args, ctx)
        }
        CallRoute::MathAssumption(library_object) => {
            validate_arity(&library_object, object, method, args.len())?;
            match method {
                "atan2" => {
                    let y = args[0].m1_scalar()?.as_f32();
                    let x = args[1].m1_scalar()?.as_f32();
                    Ok(Value::m1_float(y.atan2(x)))
                }
                // `Math.fabs` also appears in real ECU scripts (AV-M1
                // Control.Update): a plain absolute value, same routing
                // rationale as `atan2`.
                "fabs" => Ok(Value::m1_float(args[0].m1_scalar()?.as_f32().abs())),
                _ => Err(unsupported(object, method)),
            }
        }
        CallRoute::EnumAsInteger => {
            validate_object_arity(object, method, args.len())?;
            match enum_conv::as_integer(object, ctx)? {
                Some(value) => Ok(value),
                None => Err(unsupported(object, method)),
            }
        }
        CallRoute::EnumAsString => {
            validate_object_arity(object, method, args.len())?;
            match enum_conv::as_string(object, ctx)? {
                Some(value) => Ok(value),
                None => Err(unsupported(object, method)),
            }
        }
        CallRoute::ObjectValidate => {
            validate_object_arity(object, method, args.len())?;
            object::validate(object, args, ctx)
        }
        CallRoute::ObjectConstrain => {
            validate_object_arity(object, method, args.len())?;
            object::constrain(object, args, ctx)
        }
        CallRoute::ObjectGetUnscheduled => {
            validate_object_arity(object, method, args.len())?;
            object::get_unscheduled(object, ctx)
        }
        CallRoute::ChannelSet => {
            validate_object_arity(object, method, args.len())?;
            object::set(object, args, ctx)
        }
        CallRoute::Timer => {
            validate_object_arity(object, method, args.len())?;
            let object_key = timer_object_key(object, ctx);
            match stateful::timer(method, args, object_key, ctx)? {
                Some(value) => Ok(value),
                None => Err(unsupported(object, method)),
            }
        }
        CallRoute::ProjectIo => io_stub::project_object_call(object, method, args, ctx),
        CallRoute::Unsupported => Err(unsupported(object, method)),
    }
}

/// Dispatch a bare user-function call such as `Update(...)`. Bare callees cannot
/// name a library builtin, so the shared capability model either resolves a
/// script-backed function or rejects the call.
pub(crate) fn dispatch_bare(
    callee: &str,
    args: &[Value],
    site: CallSite,
    ctx: &mut EvalCtx,
) -> Result<Value, EvalError> {
    let capability = {
        let scope = CapabilityScope {
            project: Some(ctx.project),
            group: ctx.group,
            fn_symbol: ctx.fn_symbol,
            locals: Some(&ctx.env.locals),
            scripts: ctx.scripts,
        };
        classify_bare_call(callee, &scope)
    };
    if let CallRoute::UserFunction(canon) = capability.route
        && let Some(value) = userfn::call(&canon, args, ctx)?
    {
        return Ok(value);
    }
    Err(EvalError::UnsupportedConstruct {
        kind: format!("call to non-function {callee:?}"),
        at: site.offset(),
    })
}

/// The state key for a Timer object: a [`CallSite`] whose script slot is the
/// object's canonical path (offset 0), so every method call on the same Timer
/// shares one countdown. Resolves the object spelling against the project for
/// path stability; falls back to the raw spelling when unresolved.
fn timer_object_key(object: &str, ctx: &EvalCtx) -> CallSite {
    let canon = match classify(
        object,
        ctx.group,
        ctx.fn_symbol,
        ctx.project,
        &ctx.env.locals,
    ) {
        Target::Symbol(path) => path,
        _ => object.to_string(),
    };
    CallSite::new(canon, 0)
}

/// Attempt a table `.Lookup()`. Returns `Ok(Some(value))` when `object` resolves
/// to a project table and the calibration carries its cells; `Ok(None)` when
/// `object` is not a table (so the caller continues to library-object dispatch);
/// and an error when the table exists but the lookup cannot proceed (missing
/// calibration values, wrong arity).
fn try_table_lookup(
    object: &str,
    args: &[Value],
    ctx: &mut EvalCtx,
) -> Result<Option<Value>, EvalError> {
    // Resolve the object spelling to a canonical symbol path in the current scope.
    let target = classify(
        object,
        ctx.group,
        ctx.fn_symbol,
        ctx.project,
        &ctx.env.locals,
    );
    let Target::Symbol(canon) = target else {
        return Ok(None);
    };
    // Only a Table symbol has a `.Lookup()`.
    let is_table = ctx
        .project
        .symbols()
        .get(&canon)
        .map(|s| s.kind == SymbolKind::Table)
        .unwrap_or(false);
    if !is_table {
        return Ok(None);
    }

    // The cells live in the calibration, keyed by the name the `.m1cfg` wrote.
    // Real exports omit the implicit leading `Root.` group prefix, so try the
    // canonical path first, then the `Root.`-stripped form (mirrors parameter
    // lookup in the expression evaluator).
    let table = match ctx
        .calib
        .table(&canon)
        .or_else(|| canon.strip_prefix("Root.").and_then(|p| ctx.calib.table(p)))
    {
        Some(table) => table,
        // No calibration cells for this table. In whole-project mode (no `.m1cfg`)
        // the table is an unseeded externally-driven output, like a tunable
        // parameter: it falls back to the documented float default (its `.Value`
        // output type), flagged externally driven, rather than aborting the run. In
        // single-function / cone mode a `.Lookup` with no cells is still fail-loud
        // `MissingCalibration` — the user must supply the calibration.
        None if ctx.env.default_unseeded_channels => {
            if let Some(trace) = ctx.trace.as_deref_mut() {
                trace.mark_external(canon.clone());
            }
            return Ok(Some(Value::m1_float(0.0)));
        }
        None => {
            return Err(EvalError::MissingCalibration {
                path: canon.clone(),
            });
        }
    };

    // Each lookup coordinate must be numeric; collect them then interpolate.
    let inputs = args
        .iter()
        .map(Value::m1_scalar)
        .collect::<Result<Vec<_>, _>>()?;
    // `table::lookup` validates arity (inputs vs axes) and clamps out-of-range
    // coordinates, returning a BadCall on a mismatch.
    let value = crate::table::lookup(table, &inputs)?;
    Ok(Some(Value::M1(value)))
}

/// Attempt a table `.Get(site)` — a raw read of one body cell by flat site
/// index (row-major, matching [`crate::calib::CalTable`]'s documented layout),
/// with no interpolation. Returns `Ok(Some(value))` when `object` resolves to a
/// project table with calibration cells; `Ok(None)` when `object` is not a
/// table (so the caller continues to library-object dispatch). The site must be
/// a single non-negative integral index inside the body — out of range fails
/// loud as a [`EvalError::BadCall`] (a raw site read has no clamping semantics
/// to borrow), and a missing calibration follows the same rules as `.Lookup()`.
fn try_table_get(
    object: &str,
    args: &[Value],
    ctx: &mut EvalCtx,
) -> Result<Option<Value>, EvalError> {
    // Resolve the object spelling to a canonical symbol path in the current
    // scope; only a Table symbol has a `.Get()`.
    let target = classify(
        object,
        ctx.group,
        ctx.fn_symbol,
        ctx.project,
        &ctx.env.locals,
    );
    let Target::Symbol(canon) = target else {
        return Ok(None);
    };
    let is_table = ctx
        .project
        .symbols()
        .get(&canon)
        .map(|s| s.kind == SymbolKind::Table)
        .unwrap_or(false);
    if !is_table {
        return Ok(None);
    }

    // The cells live in the calibration, keyed by the name the `.m1cfg` wrote —
    // canonical path first, then the `Root.`-stripped form (mirrors `.Lookup()`).
    let table = match ctx
        .calib
        .table(&canon)
        .or_else(|| canon.strip_prefix("Root.").and_then(|p| ctx.calib.table(p)))
    {
        Some(table) => table,
        // Same calibration-fallback rules as `.Lookup()`: whole-project mode
        // reads the externally-driven default rather than aborting the run;
        // strict modes fail loud.
        None if ctx.env.default_unseeded_channels => {
            if let Some(trace) = ctx.trace.as_deref_mut() {
                trace.mark_external(canon.clone());
            }
            return Ok(Some(Value::m1_float(0.0)));
        }
        None => {
            return Err(EvalError::MissingCalibration {
                path: canon.clone(),
            });
        }
    };

    // Exactly one site argument, integral and inside the body.
    if args.len() != 1 {
        return Err(EvalError::BadCall {
            detail: format!("{object}.Get expects 1 site argument, got {}", args.len()),
        });
    }
    let index = table_site_index(&args[0]).ok_or_else(|| EvalError::BadCall {
        detail: format!(
            "{object}.Get site must be a non-negative M1 integer, got {:?}",
            args[0]
        ),
    })?;
    match table.body.get(index) {
        Some(v) => Ok(Some(Value::M1(*v))),
        None => Err(EvalError::BadCall {
            detail: format!(
                "{object}.Get site {index} out of range for a {}-cell table body",
                table.body.len()
            ),
        }),
    }
}

fn table_site_index(value: &Value) -> Option<usize> {
    match value.m1_scalar().ok()? {
        M1Scalar::Integer(value) => usize::try_from(value).ok(),
        M1Scalar::UnsignedInteger(value) => usize::try_from(value).ok(),
        M1Scalar::FloatingPoint(value)
            if value.is_finite() && value >= 0.0 && value.fract() == 0.0 =>
        {
            Some(value as usize)
        }
        M1Scalar::FixedPoint7dps(value) if value.raw() >= 0 => {
            let raw = value.raw();
            let scale = i32::try_from(crate::value::FixedPoint7dps::SCALE).ok()?;
            (raw % scale == 0).then_some((raw / scale) as usize)
        }
        _ => None,
    }
}

/// Validate the argument count of a library-object call against the intrinsic
/// signature registry. The method must exist on the object (else
/// [`EvalError::UnsupportedBuiltin`]) and `argc` must match some overload's
/// parameter count (else [`EvalError::BadCall`]).
fn validate_arity(
    library_object: &str,
    source_object: &str,
    method: &str,
    argc: usize,
) -> Result<(), EvalError> {
    let overloads = intrinsics::get().library_overloads(library_object, method);
    if overloads.is_empty() {
        // The registry lists no such method on this object.
        return Err(unsupported(source_object, method));
    }
    let accepted: Vec<usize> = overloads.iter().map(|o| o.params.len()).collect();
    if accepted.contains(&argc) {
        Ok(())
    } else {
        Err(EvalError::BadCall {
            detail: format!(
                "{source_object}.{method} expects {} argument(s), got {argc}",
                arities_display(&accepted)
            ),
        })
    }
}

/// Validate a resolved project-object method against the object-method catalog.
/// Receiver eligibility has already been decided by the capability model; this
/// check only enforces the declared argument count.
fn validate_object_arity(object: &str, method: &str, argc: usize) -> Result<(), EvalError> {
    let overloads = intrinsics::get().object_method(method);
    if overloads.is_empty() {
        return Err(unsupported(object, method));
    }
    let accepted: Vec<usize> = overloads.iter().map(|o| o.params.len()).collect();
    if accepted.contains(&argc) {
        Ok(())
    } else {
        Err(EvalError::BadCall {
            detail: format!(
                "{object}.{method} expects {} argument(s), got {argc}",
                arities_display(&accepted)
            ),
        })
    }
}

/// Render the accepted arities for a `BadCall` message, deduplicated and sorted
/// (an overloaded method may accept several counts).
fn arities_display(accepted: &[usize]) -> String {
    let mut counts: Vec<usize> = accepted.to_vec();
    counts.sort_unstable();
    counts.dedup();
    counts
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(" or ")
}

fn unsupported(object: &str, method: &str) -> EvalError {
    EvalError::UnsupportedBuiltin {
        object: object.to_string(),
        method: method.to_string(),
    }
}

/// What the evaluator does with a resolved call. Runtime dispatch and coverage
/// both consume this classification, so the report describes the route that will
/// actually run for this receiver in this project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinSupport {
    /// Runs through a direct evaluator implementation.
    ///
    /// This is an execution-route label, not the README's evidence maturity:
    /// direct implementations are still `Assumed` maturity until compared with
    /// captured M1 output.
    Direct,
    /// Runs through an explicit offline model, such as a time-domain update law.
    /// The coverage report renders this operational category as `Assumed`.
    Modeled,
    /// A Tier-3 IO object handled as a documented/scenario-fed stub.
    Stubbed,
    /// Not implemented — fails loud at runtime.
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CallRoute {
    UserFunction(String),
    PureLibrary(String),
    StatefulLibrary(String),
    IoLibrary(String),
    MathAssumption(String),
    TableLookup,
    TableGet,
    EnumAsInteger,
    EnumAsString,
    ObjectValidate,
    ObjectConstrain,
    ObjectGetUnscheduled,
    ChannelSet,
    Timer,
    ProjectIo,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallCapability {
    pub(crate) support: BuiltinSupport,
    route: CallRoute,
}

/// The lexical and project information needed to resolve a call. Runtime supplies
/// the active frame for bare callees. Member calls resolve the complete dotted
/// callee first, matching the type checker: a scalar local cannot shadow a
/// qualified library call such as `Calculate.Max`.
pub(crate) struct CapabilityScope<'a> {
    pub(crate) project: Option<&'a Project>,
    pub(crate) group: Option<&'a str>,
    pub(crate) fn_symbol: Option<&'a str>,
    pub(crate) locals: Option<&'a HashMap<String, Value>>,
    pub(crate) scripts: &'a [ParsedScript],
}

fn capability(support: BuiltinSupport, route: CallRoute) -> CallCapability {
    CallCapability { support, route }
}

fn unsupported_capability() -> CallCapability {
    capability(BuiltinSupport::Unsupported, CallRoute::Unsupported)
}

/// Direct pure-library implementations. The method catalog lives here, not in
/// coverage, and dispatch refuses any library call that this model does not
/// route.
const SUPPORTED_PURE_METHODS: &[(&str, &str)] = &[
    // Calculate.* pure math.
    ("Calculate", "Max"),
    ("Calculate", "Min"),
    ("Calculate", "Absolute"),
    ("Calculate", "Average"),
    ("Calculate", "Modulo"),
    ("Calculate", "Bias"),
    ("Calculate", "PI"),
    ("Calculate", "NAN"),
    ("Calculate", "Infinity"),
    ("Calculate", "MaximumFloat"),
    ("Calculate", "Floor"),
    ("Calculate", "Ceiling"),
    ("Calculate", "Power"),
    ("Calculate", "FastSquareRoot"),
    ("Calculate", "IsNAN"),
    ("Calculate", "IsFinite"),
    ("Calculate", "FastSin"),
    ("Calculate", "FastCos"),
    ("Calculate", "FastTan"),
    ("Calculate", "InverseSin"),
    ("Calculate", "InverseCos"),
    ("Calculate", "InverseTan"),
    ("Calculate", "InverseTan2"),
    // Convert.* M1-width conversions.
    ("Convert", "ToInteger"),
    ("Convert", "ToUnsignedInteger"),
    ("Convert", "ToFixed7DP"),
    // Limit.*.
    ("Limit", "Range"),
    ("Limit", "Max"),
    ("Limit", "Min"),
];

/// Time-domain implementations based on the evaluator's documented Phase-1
/// model. They run deterministically, but remain assumptions until checked
/// against M1 Sim. Keep this list aligned with `stateful::call`.
const MODELED_STATEFUL_METHODS: &[(&str, &str)] = &[
    ("Calculate", "Stable"),
    ("Calculate", "Hysteresis"),
    ("Calculate", "Between"),
    ("Calculate", "Beyond"),
    ("Filter", "FirstOrder"),
    ("Filter", "Maximum"),
    ("Filter", "Minimum"),
    ("Integral", "Normal"),
    ("Derivative", "Normal"),
    ("Derivative", "Filtered"),
    ("Derivative", "Adaptive"),
    // Delay/Debounce/Change.
    ("Delay", "Rising"),
    ("Delay", "Falling"),
    ("Delay", "Stable"),
    ("Debounce", "Stable"),
    ("Debounce", "Fast"),
    ("Debounce", "Verify"),
    ("Debounce", "Filter"),
    ("Change", "By"),
    ("Change", "Up"),
    ("Change", "Down"),
    ("Change", "To"),
    ("Change", "From"),
    ("Change", "Either"),
];

/// The Tier-3 IO library objects: their methods are handled as documented/
/// scenario-fed stubs (flagged externally driven), not evaluated as hardware.
const STUB_OBJECTS: &[&str] = &["CanComms", "Serial", "System", "Logging"];

/// Calibration-only `Math` methods that real ECU scripts nevertheless use. They
/// have deterministic standard-library implementations here, but remain explicit
/// assumptions because their ECU-script validity and exact M1 behavior are not
/// established.
const MODELED_MATH_METHODS: &[(&str, &str)] = &[("Math", "atan2"), ("Math", "fabs")];

/// Classify a member call with the same receiver resolution runtime dispatch
/// uses. A user function is checked first because `Service Bits.Update` and a
/// script-backed `Slip Control.Update` share a method spelling but take different
/// routes.
pub(crate) fn classify_member_call(
    object: &str,
    method: &str,
    scope: &CapabilityScope<'_>,
) -> CallCapability {
    let full_path = format!("{object}.{method}");
    if let Some(canon) = script_backed_user_function(&full_path, scope) {
        return capability(BuiltinSupport::Direct, CallRoute::UserFunction(canon));
    }

    let Some(project) = scope.project else {
        return classify_without_project(object, method);
    };

    // Resolve the complete callee before the receiver. The M1 resolver only
    // applies local shadowing to a single-segment path, so a local named
    // `Calculate` does not turn `Calculate.Max` into a call on that scalar. This
    // also normalizes the explicit `Library.Calculate.Max` spelling to the
    // canonical `Calculate` catalog object.
    let no_locals = HashMap::new();
    let locals = scope.locals.unwrap_or(&no_locals);
    if let Target::Builtin {
        object: library_object,
    } = classify(&full_path, scope.group, scope.fn_symbol, project, locals)
    {
        return classify_library(&library_object, method);
    }

    // A resolved enum member is never a project IO object. Handle both
    // supported conversions here and reject every other method before the
    // unresolved-project fallback can mistake spellings such as `.Set()` for
    // a hardware stub.
    if is_enum_literal(object, project) {
        return match method {
            "AsInteger" => capability(BuiltinSupport::Direct, CallRoute::EnumAsInteger),
            "AsString" => capability(BuiltinSupport::Direct, CallRoute::EnumAsString),
            _ => unsupported_capability(),
        };
    }

    // `Library.` explicitly selects the intrinsic namespace. An unknown object
    // under that anchor cannot fall through to the project-object stub catalog.
    if object == "Library" || object.starts_with("Library.") {
        return unsupported_capability();
    }

    match classify(object, scope.group, scope.fn_symbol, project, locals) {
        Target::Builtin { object: builtin } => classify_library(&builtin, method),
        Target::Symbol(canon) => classify_project_method(&canon, method, project),
        Target::Unresolved => classify_unresolved_project_method(method),
        Target::Local(_) => unsupported_capability(),
    }
}

/// Classify a bare callee. Only a project function or method with a discovered
/// script is executable through this syntax.
pub(crate) fn classify_bare_call(callee: &str, scope: &CapabilityScope<'_>) -> CallCapability {
    match script_backed_user_function(callee, scope) {
        Some(canon) => capability(BuiltinSupport::Direct, CallRoute::UserFunction(canon)),
        None => unsupported_capability(),
    }
}

fn classify_without_project(object: &str, method: &str) -> CallCapability {
    if let Some(library_object) = canonical_library_object(object) {
        return classify_library(library_object, method);
    }
    if object == "Library" || object.starts_with("Library.") {
        return unsupported_capability();
    }
    classify_unresolved_project_method(method)
}

/// Normalize a source receiver that directly names a library object. The
/// explicit `Library.` anchor is source syntax, not part of the intrinsic
/// catalog key.
fn canonical_library_object(object: &str) -> Option<&'static str> {
    let candidate = object.strip_prefix("Library.").unwrap_or(object);
    if candidate.contains('.') {
        return None;
    }
    intrinsics::get().library_object_name(candidate)
}

fn classify_library(object: &str, method: &str) -> CallCapability {
    let pair = (object, method);
    if SUPPORTED_PURE_METHODS.contains(&pair) {
        return capability(
            BuiltinSupport::Direct,
            CallRoute::PureLibrary(object.to_string()),
        );
    }
    if MODELED_STATEFUL_METHODS.contains(&pair) {
        return capability(
            BuiltinSupport::Modeled,
            CallRoute::StatefulLibrary(object.to_string()),
        );
    }
    if MODELED_MATH_METHODS.contains(&pair) {
        return capability(
            BuiltinSupport::Modeled,
            CallRoute::MathAssumption(object.to_string()),
        );
    }
    if STUB_OBJECTS.contains(&object)
        && !intrinsics::get()
            .library_overloads(object, method)
            .is_empty()
    {
        return capability(
            BuiltinSupport::Stubbed,
            CallRoute::IoLibrary(object.to_string()),
        );
    }
    unsupported_capability()
}

fn classify_project_method(canon: &str, method: &str, project: &Project) -> CallCapability {
    let Some(symbol) = project.symbols().get(canon) else {
        return unsupported_capability();
    };

    if symbol.kind == SymbolKind::Table {
        match method {
            "Lookup" => return capability(BuiltinSupport::Modeled, CallRoute::TableLookup),
            "Get" => return capability(BuiltinSupport::Direct, CallRoute::TableGet),
            // A table may also expose generic numeric accessors through its
            // generated `.Value` channel. Continue to receiver-aware object
            // method classification for every other method.
            _ => {}
        }
    }
    if method == "AsInteger" && is_enum_source(canon, project) {
        return capability(BuiltinSupport::Direct, CallRoute::EnumAsInteger);
    }
    if method == "AsString" && is_enum_source(canon, project) {
        return capability(BuiltinSupport::Direct, CallRoute::EnumAsString);
    }
    if method == "Set" && object::writable_value_path(canon, project).is_some() {
        return capability(BuiltinSupport::Direct, CallRoute::ChannelSet);
    }
    if method == "GetUnscheduled" && object::unscheduled_value_path(canon, project).is_some() {
        return capability(BuiltinSupport::Direct, CallRoute::ObjectGetUnscheduled);
    }
    // GetUnscheduled never falls through to a similarly named hardware stub.
    if method == "GetUnscheduled" {
        return unsupported_capability();
    }
    // A project value that cannot resolve to a writable Channel must not be
    // reinterpreted as a generic hardware `Set` call. Non-value package
    // objects and references retain their separately catalogued IO stubs.
    let generated_value = format!("{canon}.Value");
    let is_value_group = symbol.kind == SymbolKind::Group
        && (symbol.default_value.is_some() || project.symbols().get(&generated_value).is_some());
    if method == "Set"
        && (matches!(
            symbol.kind,
            SymbolKind::Channel | SymbolKind::Parameter | SymbolKind::Constant | SymbolKind::Table
        ) || is_value_group)
    {
        return unsupported_capability();
    }
    if object::is_numeric_source(canon, project) {
        match method {
            "Validate" => {
                return capability(BuiltinSupport::Direct, CallRoute::ObjectValidate);
            }
            "Constrain" => {
                return capability(BuiltinSupport::Direct, CallRoute::ObjectConstrain);
            }
            // A typed hardware object also carries a numeric value. Core value
            // methods claim only their exact names; every other method must
            // continue to the receiver-specific timer/IO routes below.
            _ => {}
        }
    }
    if symbol.classname.as_deref() == Some("BuiltIn.Timer")
        && matches!(method, "Start" | "Stop" | "Reset" | "Remaining")
    {
        return capability(BuiltinSupport::Modeled, CallRoute::Timer);
    }
    if matches!(
        symbol.kind,
        SymbolKind::Object | SymbolKind::Group | SymbolKind::Reference | SymbolKind::Other
    ) && io_stub::PROJECT_OBJECT_STUB_METHODS.contains(&method)
    {
        return capability(BuiltinSupport::Stubbed, CallRoute::ProjectIo);
    }
    unsupported_capability()
}

fn classify_unresolved_project_method(method: &str) -> CallCapability {
    if io_stub::PROJECT_OBJECT_STUB_METHODS.contains(&method) {
        capability(BuiltinSupport::Stubbed, CallRoute::ProjectIo)
    } else {
        unsupported_capability()
    }
}

fn is_enum_literal(object: &str, project: &Project) -> bool {
    object.rsplit_once('.').is_some_and(|(prefix, member)| {
        project
            .symbols()
            .enum_by_name(prefix)
            .is_some_and(|id| project.symbols().enum_has_member(id, member))
    })
}

fn is_enum_source(canon: &str, project: &Project) -> bool {
    enum_conv::enum_value_path(canon, project).is_some()
}

fn script_backed_user_function(callee: &str, scope: &CapabilityScope<'_>) -> Option<String> {
    let project = scope.project?;
    let no_locals = HashMap::new();
    let locals = scope.locals.unwrap_or(&no_locals);
    let Target::Symbol(canon) = classify(callee, scope.group, scope.fn_symbol, project, locals)
    else {
        return None;
    };
    let symbol = project.symbols().get(&canon)?;
    if !matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method) {
        return None;
    }
    scope
        .scripts
        .iter()
        .any(|script| project.function_symbol_for_script(&script.name).as_deref() == Some(&canon))
        .then_some(canon)
}

/// Project-free compatibility helper. New runtime and coverage paths call the
/// project-aware classifier above. Without a project, receiver-specific methods
/// are conservative: an unresolved IO writer is a stub, while table, enum,
/// channel, and timer methods are unsupported because their receiver kind cannot
/// be proven.
pub fn classify_builtin(object: &str, method: &str) -> BuiltinSupport {
    let scope = CapabilityScope {
        project: None,
        group: None,
        fn_symbol: None,
        locals: None,
        scripts: &[],
    };
    classify_member_call(object, method, &scope).support
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calib::Calibration;
    use crate::env::{Env, StateStore};
    use m1_typecheck::Project;
    use m1_typecheck::parsed::parse_all;
    use std::path::Path;

    /// Load the synthetic mini fixture project (with calibration) for the
    /// table-lookup and resolution-backed tests.
    fn mini_loaded() -> crate::loader::Loaded {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini");
        crate::loader::load(
            &dir.join("Project.m1prj"),
            Some(&dir.join("parameters.m1cfg")),
        )
        .expect("mini fixture loads")
    }

    fn support_in(
        loaded: &crate::loader::Loaded,
        group: Option<&str>,
        fn_symbol: Option<&str>,
        object: &str,
        method: &str,
    ) -> BuiltinSupport {
        let scope = CapabilityScope {
            project: Some(&loaded.project),
            group,
            fn_symbol,
            locals: None,
            scripts: &loaded.scripts,
        };
        classify_member_call(object, method, &scope).support
    }

    /// A harness owning the stores so a fresh `EvalCtx` can be built per call.
    struct Harness {
        project: Project,
        calib: Calibration,
        env: Env,
        state: StateStore,
    }

    impl Harness {
        fn new() -> Harness {
            let loaded = mini_loaded();
            Harness {
                project: loaded.project,
                calib: loaded.calib,
                env: Env::new(),
                state: StateStore::new(),
            }
        }

        fn empty_calib() -> Harness {
            let mut h = Harness::new();
            h.calib = Calibration::default();
            h
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

        fn call(&mut self, object: &str, method: &str, args: &[Value]) -> Result<Value, EvalError> {
            let site = CallSite::new("Demo.Update.m1scr", 0);
            let mut ctx = self.ctx();
            dispatch(object, method, args, site, &mut ctx)
        }
    }

    // ---- pure library dispatch ----

    #[test]
    fn calculate_max_dispatches() {
        let mut h = Harness::new();
        assert_eq!(
            h.call(
                "Calculate",
                "Max",
                &[Value::m1_integer(2), Value::m1_integer(3)]
            )
            .unwrap(),
            Value::m1_integer(3)
        );
    }

    #[test]
    fn local_named_calculate_does_not_shadow_qualified_builtin_call() {
        let mut h = Harness::new();
        h.env.set_local("Calculate", Value::m1_integer(0));

        assert_eq!(
            h.call(
                "Calculate",
                "Max",
                &[Value::m1_integer(1), Value::m1_integer(2)]
            )
            .unwrap(),
            Value::m1_integer(2)
        );
    }

    #[test]
    fn library_qualified_calls_dispatch_through_canonical_objects() {
        let mut h = Harness::new();

        assert_eq!(
            h.call(
                "Library.Calculate",
                "Max",
                &[Value::m1_integer(1), Value::m1_integer(2)]
            )
            .unwrap(),
            Value::m1_integer(2)
        );
        assert_eq!(
            h.call(
                "Library.Debounce",
                "Filter",
                &[Value::Bool(true), Value::m1_float(0.1)]
            )
            .unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            h.call("Library.Math", "fabs", &[Value::m1_float(-3.5)])
                .unwrap(),
            Value::m1_float(3.5)
        );
        assert_eq!(
            h.call(
                "Library.CanComms",
                "GetFloat",
                &[Value::m1_unsigned(1), Value::m1_integer(2)]
            )
            .unwrap(),
            Value::m1_float(0.0)
        );
    }

    #[test]
    fn unknown_library_anchored_object_does_not_fall_through_to_project_stub() {
        assert_eq!(
            classify_builtin("Library.NoSuchObject", "Set"),
            BuiltinSupport::Unsupported
        );

        let mut h = Harness::new();
        assert!(matches!(
            h.call("Library.NoSuchObject", "Set", &[Value::m1_integer(1)]),
            Err(EvalError::UnsupportedBuiltin { .. })
        ));
    }

    #[test]
    fn limit_range_dispatches() {
        let mut h = Harness::new();
        assert_eq!(
            h.call(
                "Limit",
                "Range",
                &[
                    Value::m1_float(9.0),
                    Value::m1_float(0.0),
                    Value::m1_float(5.0)
                ]
            )
            .unwrap(),
            Value::m1_float(5.0)
        );
    }

    #[test]
    fn convert_to_integer_dispatches() {
        let mut h = Harness::new();
        assert_eq!(
            h.call("Convert", "ToInteger", &[Value::m1_float(2.9)])
                .unwrap(),
            Value::m1_integer(3)
        );
    }

    #[test]
    fn convert_dispatch_stays_on_m1_scalar_values() {
        let mut h = Harness::new();
        assert_eq!(
            h.call("Convert", "ToUnsignedInteger", &[Value::m1_float(-2.6)])
                .unwrap(),
            Value::m1_unsigned(0)
        );
        assert_eq!(
            h.call("Convert", "ToFixed7DP", &[Value::m1_integer(1)])
                .unwrap(),
            Value::M1(M1Scalar::FixedPoint7dps(
                crate::value::FixedPoint7dps::from_raw(10_000_000)
            ))
        );
    }

    // ---- arity validation against intrinsics ----

    #[test]
    fn wrong_arity_is_bad_call() {
        let mut h = Harness::new();
        // Calculate.Max takes two arguments; one is a BadCall, not a guess.
        match h.call("Calculate", "Max", &[Value::m1_integer(1)]) {
            Err(EvalError::BadCall { .. }) => {}
            other => panic!("expected BadCall, got {other:?}"),
        }
        // Limit.Range takes three.
        match h.call(
            "Limit",
            "Range",
            &[Value::m1_integer(1), Value::m1_integer(2)],
        ) {
            Err(EvalError::BadCall { .. }) => {}
            other => panic!("expected BadCall, got {other:?}"),
        }
    }

    #[test]
    fn unknown_method_on_pure_object_is_unsupported() {
        let mut h = Harness::new();
        match h.call("Calculate", "NotAMethod", &[Value::m1_integer(1)]) {
            Err(EvalError::UnsupportedBuiltin { object, method }) => {
                assert_eq!(object, "Calculate");
                assert_eq!(method, "NotAMethod");
            }
            other => panic!("expected UnsupportedBuiltin, got {other:?}"),
        }
    }

    #[test]
    fn stateful_calculate_method_dispatches_to_state_engine() {
        let mut h = Harness::new();
        // Calculate.Stable (arity 2) is a stateful predicate: M6 routes it to the
        // state engine. Its first tick has not yet been stable, so it is false —
        // a real evaluated value, not a fail-loud error.
        match h.call(
            "Calculate",
            "Stable",
            &[Value::m1_float(1.0), Value::m1_float(0.1)],
        ) {
            Ok(Value::Bool(false)) => {}
            other => panic!("expected Ok(Bool(false)) on first tick, got {other:?}"),
        }
    }

    #[test]
    fn filter_first_order_dispatches_to_state_engine() {
        let mut h = Harness::new();
        // A stateful library object routes through dispatch with arity validation;
        // the first tick of FirstOrder seeds to the input (1.0).
        match h.call(
            "Filter",
            "FirstOrder",
            &[Value::m1_float(1.0), Value::m1_float(0.1)],
        ) {
            Ok(Value::M1(M1Scalar::FloatingPoint(x))) => assert!((x - 1.0).abs() < 1e-6),
            other => panic!("expected seeded Float(1.0), got {other:?}"),
        }
    }

    #[test]
    fn debounce_filter_is_classified_and_dispatched_as_assumed() {
        let mut h = Harness::new();
        assert_eq!(
            classify_builtin("Debounce", "Filter"),
            BuiltinSupport::Modeled
        );
        assert_eq!(
            h.call(
                "Debounce",
                "Filter",
                &[Value::Bool(true), Value::m1_float(0.1)]
            )
            .unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn stateful_wrong_arity_is_bad_call() {
        let mut h = Harness::new();
        // Integral.Normal needs five arguments; fewer is a BadCall, not a guess.
        match h.call("Integral", "Normal", &[Value::m1_float(1.0)]) {
            Err(EvalError::BadCall { .. }) => {}
            other => panic!("expected BadCall, got {other:?}"),
        }
    }

    #[test]
    fn unimplemented_stateful_method_fails_loud() {
        let mut h = Harness::new();
        // Delay.Signal15 is a buffered sample delay we do not implement; the
        // object is recognised but the method falls through to fail loud.
        match h.call(
            "Delay",
            "Signal15",
            &[Value::m1_float(1.0), Value::m1_integer(3)],
        ) {
            Err(EvalError::UnsupportedBuiltin { object, method }) => {
                assert_eq!(object, "Delay");
                assert_eq!(method, "Signal15");
            }
            other => panic!("expected UnsupportedBuiltin, got {other:?}"),
        }
    }

    #[test]
    fn unknown_object_is_unsupported() {
        let mut h = Harness::new();
        match h.call("NoSuchObject", "Whatever", &[]) {
            Err(EvalError::UnsupportedBuiltin { .. }) => {}
            other => panic!("expected UnsupportedBuiltin, got {other:?}"),
        }
    }

    // ---- Math.atan2 (calibration-only object surfaced in ECU scripts) ----

    #[test]
    fn math_atan2_routes_to_atan2() {
        let mut h = Harness::new();
        // atan2(1, 1) = pi/4. The calibration-only `Math` object is surfaced in
        // real ECU scripts; we route its `atan2` to the same evaluation as
        // Calculate.InverseTan2. Coverage marks the calibration-only route as an
        // assumption, not a hardware stub.
        match h.call(
            "Math",
            "atan2",
            &[Value::m1_float(1.0), Value::m1_float(1.0)],
        ) {
            Ok(Value::M1(M1Scalar::FloatingPoint(x))) => {
                assert!((x - std::f32::consts::FRAC_PI_4).abs() < 1e-6)
            }
            other => panic!("expected Float(pi/4), got {other:?}"),
        }
    }

    #[test]
    fn math_atan2_wrong_arity_is_bad_call() {
        let mut h = Harness::new();
        // Math.atan2 takes two arguments (validated against intrinsics).
        match h.call("Math", "atan2", &[Value::m1_float(1.0)]) {
            Err(EvalError::BadCall { .. }) => {}
            other => panic!("expected BadCall, got {other:?}"),
        }
    }

    #[test]
    fn math_atan2_is_classified_assumed() {
        assert_eq!(classify_builtin("Math", "atan2"), BuiltinSupport::Modeled);
    }

    #[test]
    fn math_fabs_is_classified_assumed() {
        // Same provenance rationale as atan2: routed, but calibration-only.
        assert_eq!(classify_builtin("Math", "fabs"), BuiltinSupport::Modeled);
    }

    #[test]
    fn table_get_is_classified_supported() {
        let loaded = mini_loaded();
        assert_eq!(
            support_in(
                &loaded,
                Some("Root.Demo"),
                Some("Root.Demo.Update"),
                "Map",
                "Get"
            ),
            BuiltinSupport::Direct
        );
    }

    #[test]
    fn math_unknown_method_is_unsupported() {
        let mut h = Harness::new();
        // Only `atan2` is routed from the calibration-only Math object; anything
        // else fails loud rather than being silently evaluated.
        match h.call("Math", "Sqrt", &[Value::m1_float(4.0)]) {
            Err(EvalError::UnsupportedBuiltin { object, method }) => {
                assert_eq!(object, "Math");
                assert_eq!(method, "Sqrt");
            }
            other => panic!("expected UnsupportedBuiltin, got {other:?}"),
        }
    }

    // ---- table .Lookup() ----

    #[test]
    fn table_lookup_interpolates_over_calibration() {
        let mut h = Harness::new();
        // The mini fixture's Demo.Map is 2-D: x in {0,100}, y in {0,1}, body
        // (10,20,30,40). Corner and midpoint values come straight from table.rs.
        assert_eq!(
            h.call(
                "Map",
                "Lookup",
                &[Value::m1_float(0.0), Value::m1_float(0.0)]
            )
            .unwrap(),
            Value::m1_float(10.0)
        );
        assert_eq!(
            h.call(
                "Map",
                "Lookup",
                &[Value::m1_float(100.0), Value::m1_float(1.0)]
            )
            .unwrap(),
            Value::m1_float(40.0)
        );
        // Halfway in x at y=0: between 10 and 30 -> 20.
        assert_eq!(
            h.call(
                "Map",
                "Lookup",
                &[Value::m1_float(50.0), Value::m1_float(0.0)]
            )
            .unwrap(),
            Value::m1_float(20.0)
        );
        // Out-of-range inputs clamp.
        assert_eq!(
            h.call(
                "Map",
                "Lookup",
                &[Value::m1_float(999.0), Value::m1_float(9.0)]
            )
            .unwrap(),
            Value::m1_float(40.0)
        );
    }

    #[test]
    fn table_lookup_wrong_arity_is_bad_call() {
        let mut h = Harness::new();
        // Demo.Map has two axes; one coordinate is a BadCall.
        match h.call("Map", "Lookup", &[Value::m1_float(0.0)]) {
            Err(EvalError::BadCall { .. }) => {}
            other => panic!("expected BadCall, got {other:?}"),
        }
    }

    #[test]
    fn table_lookup_without_calibration_is_missing() {
        let mut h = Harness::empty_calib();
        // The table symbol resolves, but no calibration cells were loaded.
        match h.call(
            "Map",
            "Lookup",
            &[Value::m1_float(0.0), Value::m1_float(0.0)],
        ) {
            Err(EvalError::MissingCalibration { .. }) => {}
            other => panic!("expected MissingCalibration, got {other:?}"),
        }
    }

    #[test]
    fn set_on_a_table_is_unsupported() {
        let mut h = Harness::new();
        match h.call("Map", "Set", &[Value::m1_integer(1)]) {
            Err(EvalError::UnsupportedBuiltin { object, method }) => {
                assert_eq!(object, "Map");
                assert_eq!(method, "Set");
            }
            other => panic!("expected receiver-aware UnsupportedBuiltin, got {other:?}"),
        }
    }

    #[test]
    fn math_fabs_routes_to_abs() {
        let mut h = Harness::new();
        // `Math.fabs` appears in real ECU scripts (Control.Update in AV-M1);
        // route it to a plain absolute value like Calculate.Absolute.
        assert_eq!(
            h.call("Math", "fabs", &[Value::m1_float(-3.5)]).unwrap(),
            Value::m1_float(3.5)
        );
    }

    // ---- table .Get() (raw site read) ----

    #[test]
    fn table_get_reads_flat_site() {
        let mut h = Harness::new();
        // `.Get(i)` is a raw read of body cell i (row-major, no interpolation).
        // Demo.Map's body is (10,20,30,40).
        assert_eq!(
            h.call("Map", "Get", &[Value::m1_integer(0)]).unwrap(),
            Value::m1_float(10.0)
        );
        assert_eq!(
            h.call("Map", "Get", &[Value::m1_integer(2)]).unwrap(),
            Value::m1_float(30.0)
        );
        assert_eq!(
            h.call("Map", "Get", &[Value::m1_unsigned(3)]).unwrap(),
            Value::m1_float(40.0)
        );
    }

    #[test]
    fn table_get_out_of_range_is_bad_call() {
        let mut h = Harness::new();
        // A site past the body, or a negative site, fails loud — never clamps:
        // a raw site read has no clamping semantics to borrow.
        match h.call("Map", "Get", &[Value::m1_integer(4)]) {
            Err(EvalError::BadCall { .. }) => {}
            other => panic!("expected BadCall, got {other:?}"),
        }
        match h.call("Map", "Get", &[Value::m1_integer(-1)]) {
            Err(EvalError::BadCall { .. }) => {}
            other => panic!("expected BadCall, got {other:?}"),
        }
    }

    #[test]
    fn table_get_wrong_arity_is_bad_call() {
        let mut h = Harness::new();
        match h.call("Map", "Get", &[]) {
            Err(EvalError::BadCall { .. }) => {}
            other => panic!("expected BadCall, got {other:?}"),
        }
    }

    #[test]
    fn table_get_without_calibration_is_missing() {
        let mut h = Harness::empty_calib();
        match h.call("Map", "Get", &[Value::m1_integer(0)]) {
            Err(EvalError::MissingCalibration { .. }) => {}
            other => panic!("expected MissingCalibration, got {other:?}"),
        }
    }

    #[test]
    fn lookup_on_non_table_is_not_a_table_lookup() {
        let mut h = Harness::new();
        // `Calculate.Lookup` is not a table lookup; Calculate has no Lookup
        // overload either, so it is UnsupportedBuiltin (fail loud), not a panic.
        match h.call("Calculate", "Lookup", &[Value::m1_float(0.0)]) {
            Err(EvalError::UnsupportedBuiltin { .. }) => {}
            other => panic!("expected UnsupportedBuiltin, got {other:?}"),
        }
    }

    // ---- enum .AsInteger through dispatch (P15-B, Task 5) ----

    /// A harness over the synthetic enums fixture so `.AsInteger` dispatch can
    /// resolve the project-local `Drive State` enum and its enum-typed channel.
    struct EnumHarness {
        project: Project,
        calib: Calibration,
        env: Env,
        state: StateStore,
    }

    impl EnumHarness {
        fn new() -> EnumHarness {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/enums");
            let loaded =
                crate::loader::load(&dir.join("Project.m1prj"), None).expect("enums fixture loads");
            EnumHarness {
                project: loaded.project,
                calib: Calibration::default(),
                env: Env::new(),
                state: StateStore::new(),
            }
        }

        fn enum_id(&self) -> usize {
            self.project.symbols().enum_by_name("Drive State").unwrap()
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

        fn call(&mut self, object: &str, method: &str, args: &[Value]) -> Result<Value, EvalError> {
            let site = CallSite::new("Demo.Update.m1scr", 0);
            let mut ctx = self.ctx();
            dispatch(object, method, args, site, &mut ctx)
        }
    }

    #[test]
    fn dispatch_as_integer_on_enum_literal() {
        let mut h = EnumHarness::new();
        // `Drive State.Idle.AsInteger()` → 0 (ContainerOrder), via the literal form.
        assert_eq!(
            h.call("Drive State.Idle", "AsInteger", &[]).unwrap(),
            Value::m1_integer(0)
        );
        // Precharging is ContainerOrder 2.
        assert_eq!(
            h.call("Drive State.Precharging", "AsInteger", &[]).unwrap(),
            Value::m1_integer(2)
        );
    }

    #[test]
    fn dispatch_as_integer_on_enum_channel() {
        let mut h = EnumHarness::new();
        let id = h.enum_id();
        h.env.set(
            "Root.Demo.Mode",
            Value::Enum {
                id,
                member: "Precharging".to_string(),
            },
        );
        // The value form reads the channel's current enum value and converts it.
        assert_eq!(
            h.call("Root.Demo.Mode", "AsInteger", &[]).unwrap(),
            Value::m1_integer(2)
        );
    }

    #[test]
    fn dispatch_as_string_preserves_enum_member_names() {
        let mut h = EnumHarness::new();
        assert_eq!(
            h.call("Drive State.Precharging", "AsString", &[]).unwrap(),
            Value::Str("Precharging".to_string())
        );

        let id = h.enum_id();
        h.env.set(
            "Root.Demo.Mode",
            Value::Enum {
                id,
                member: "Idle".to_string(),
            },
        );
        assert_eq!(
            h.call("Mode", "AsString", &[]).unwrap(),
            Value::Str("Idle".to_string())
        );

        h.env.set(
            "Root.Demo.Compound.Value",
            Value::Enum {
                id,
                member: "Precharging".to_string(),
            },
        );
        assert_eq!(
            h.call("Compound", "AsString", &[]).unwrap(),
            Value::Str("Precharging".to_string())
        );
    }

    #[test]
    fn dispatch_as_string_rejects_an_invalid_seeded_member() {
        let mut h = EnumHarness::new();
        let id = h.enum_id();
        h.env.set(
            "Root.Demo.Mode",
            Value::Enum {
                id,
                member: "Bogus".to_string(),
            },
        );

        let error = h.call("Mode", "AsString", &[]).unwrap_err();
        assert!(matches!(error, EvalError::TypeError { .. }));
        assert!(error.to_string().contains("Bogus"), "{error}");
        assert!(error.to_string().contains("Drive State"), "{error}");
    }

    #[test]
    fn dispatch_as_integer_on_non_enum_fails_loud() {
        let mut h = EnumHarness::new();
        // A name that is neither an enum literal nor an enum-typed project symbol:
        // `.AsInteger` cannot convert it, so dispatch falls through to the Timer
        // attempt and ultimately fails loud rather than guessing.
        match h.call("No Such Thing", "AsInteger", &[]) {
            Err(EvalError::UnsupportedBuiltin { method, .. }) => {
                assert_eq!(method, "AsInteger");
            }
            other => panic!("expected UnsupportedBuiltin, got {other:?}"),
        }
    }

    #[test]
    fn enum_conversions_are_classified_by_receiver() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/enums");
        let loaded =
            crate::loader::load(&dir.join("Project.m1prj"), None).expect("enums fixture loads");
        assert_eq!(
            support_in(
                &loaded,
                Some("Root.Demo"),
                Some("Root.Demo.Update"),
                "Drive State.Idle",
                "AsInteger"
            ),
            BuiltinSupport::Direct
        );
        assert_eq!(
            support_in(
                &loaded,
                Some("Root.Demo"),
                Some("Root.Demo.Update"),
                "Drive State.Idle",
                "AsString"
            ),
            BuiltinSupport::Direct
        );
        assert_eq!(
            support_in(
                &loaded,
                Some("Root.Demo"),
                Some("Root.Demo.Update"),
                "Mode",
                "AsString"
            ),
            BuiltinSupport::Direct
        );
        assert_eq!(
            support_in(
                &loaded,
                Some("Root.Demo"),
                Some("Root.Demo.Update"),
                "Mode",
                "AsInteger"
            ),
            BuiltinSupport::Direct
        );
        assert_eq!(
            support_in(
                &loaded,
                Some("Root.Demo"),
                Some("Root.Demo.Update"),
                "Precharge State",
                "AsInteger"
            ),
            BuiltinSupport::Unsupported,
            "a non-enum channel is not an AsInteger receiver"
        );
        assert_eq!(
            support_in(
                &loaded,
                Some("Root.Demo"),
                Some("Root.Demo.Update"),
                "Precharge State",
                "AsString"
            ),
            BuiltinSupport::Unsupported,
            "a non-enum channel is not an AsString receiver"
        );
    }

    #[test]
    fn timer_methods_require_a_timer_receiver_and_are_assumed() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/enums");
        let loaded =
            crate::loader::load(&dir.join("Project.m1prj"), None).expect("enums fixture loads");
        for method in ["Start", "Stop", "Reset", "Remaining"] {
            assert_eq!(
                support_in(
                    &loaded,
                    Some("Root.Demo"),
                    Some("Root.Demo.Update"),
                    "Startup Delay",
                    method
                ),
                BuiltinSupport::Modeled,
                "Timer.{method} should use the documented timer assumption"
            );
            assert_eq!(
                support_in(
                    &loaded,
                    Some("Root.Demo"),
                    Some("Root.Demo.Update"),
                    "Precharge State",
                    method
                ),
                BuiltinSupport::Unsupported,
                "a channel cannot use Timer.{method}"
            );
        }
    }

    // ---- project-object method routing (P15-C, Tasks 6-7) ----

    /// A harness over the enums fixture that *owns a trace*, so `.Set` channel
    /// writes and externally-driven IO stubs can assert on the recorded columns.
    /// The fixture carries a plain `Precharge State` channel, a `Service Bits`
    /// value-compound, a `DashVals` CAN message (+ `Aux Switch` signal), and a
    /// `Fan Output` package object — the project-object analogues of the EV-M1
    /// constructs Task 6/7 route.
    struct ProjectObjHarness {
        project: Project,
        calib: Calibration,
        object_rules: object::ObjectRules,
        env: Env,
        state: StateStore,
        trace: crate::trace::Trace,
    }

    impl ProjectObjHarness {
        fn new() -> ProjectObjHarness {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/enums");
            let loaded =
                crate::loader::load(&dir.join("Project.m1prj"), None).expect("enums fixture loads");
            ProjectObjHarness {
                project: loaded.project,
                calib: Calibration::default(),
                object_rules: loaded.object_rules,
                env: Env::new(),
                state: StateStore::new(),
                trace: crate::trace::Trace::new(),
            }
        }

        fn call(&mut self, object: &str, method: &str, args: &[Value]) -> Result<Value, EvalError> {
            let site = CallSite::new("Demo.Update.m1scr", 0);
            let mut ctx = EvalCtx {
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
                object_rules: Some(&self.object_rules),
                depth: 0,
                trace: Some(&mut self.trace),
            };
            dispatch(object, method, args, site, &mut ctx)
        }
    }

    // ---- Task 6: Channel .Set(value) imperative setter ----

    #[test]
    fn channel_set_writes_the_channel_and_records_it() {
        let mut h = ProjectObjHarness::new();
        // `Precharge State.Set(1)` writes the channel under its canonical path and
        // records the write to the trace, returning the unit value.
        let result = h
            .call("Precharge State", "Set", &[Value::m1_integer(1)])
            .expect("Channel.Set succeeds");
        assert_eq!(result, Value::Bool(true), "Set returns the unit value");
        // The canonical path now holds the written value.
        assert_eq!(
            h.env.get("Root.Demo.Precharge State"),
            Some(&Value::m1_unsigned(1))
        );
        // And the write was recorded to the trace.
        assert_eq!(
            h.trace.channels.get("Root.Demo.Precharge State"),
            Some(&vec![Value::m1_unsigned(1)])
        );
    }

    #[test]
    fn channel_set_via_absolute_path_writes_the_channel() {
        let mut h = ProjectObjHarness::new();
        h.call("Root.Demo.Precharge State", "Set", &[Value::m1_unsigned(3)])
            .expect("Channel.Set on absolute path succeeds");
        assert_eq!(
            h.env.get("Root.Demo.Precharge State"),
            Some(&Value::m1_unsigned(3))
        );
    }

    #[test]
    fn channel_set_rejects_float_to_integral_narrowing() {
        let mut h = ProjectObjHarness::new();
        let error = h
            .call("Root.Demo.Precharge State", "Set", &[Value::m1_float(3.5)])
            .unwrap_err();
        assert!(matches!(error, EvalError::TypeError { .. }));
    }

    #[test]
    fn channel_set_wrong_arity_is_bad_call() {
        let mut h = ProjectObjHarness::new();
        // `.Set` is a single-argument setter; zero or many args is a BadCall.
        match h.call("Precharge State", "Set", &[]) {
            Err(EvalError::BadCall { .. }) => {}
            other => panic!("expected BadCall on zero-arg Set, got {other:?}"),
        }
        match h.call(
            "Precharge State",
            "Set",
            &[Value::m1_integer(1), Value::m1_integer(2)],
        ) {
            Err(EvalError::BadCall { .. }) => {}
            other => panic!("expected BadCall on two-arg Set, got {other:?}"),
        }
    }

    #[test]
    fn validate_uses_inclusive_project_ranges_and_positive_rules() {
        let mut h = ProjectObjHarness::new();
        for (value, expected) in [(-3, false), (-2, true), (3, true), (4, false)] {
            assert_eq!(
                h.call("Limited Signed", "Validate", &[Value::m1_integer(value)])
                    .unwrap(),
                Value::Bool(expected),
                "value {value}"
            );
        }
        assert_eq!(
            h.call("Positive Float", "Validate", &[Value::m1_float(-0.5)])
                .unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            h.call("Positive Float", "Validate", &[Value::m1_float(0.0)])
                .unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            h.call("Free Unsigned", "Validate", &[Value::m1_unsigned(u32::MAX)])
                .unwrap(),
            Value::Bool(true),
            "an object without validation accepts its numeric domain"
        );
    }

    #[test]
    fn constrain_clamps_and_preserves_each_m1_scalar_family() {
        let mut h = ProjectObjHarness::new();
        assert_eq!(
            h.call("Limited Signed", "Constrain", &[Value::m1_integer(-10)])
                .unwrap(),
            Value::m1_integer(-2)
        );
        assert_eq!(
            h.call("Limited Float", "Constrain", &[Value::m1_float(8.0)])
                .unwrap(),
            Value::m1_float(2.25)
        );
        assert_eq!(
            h.call(
                "Limited Fixed",
                "Constrain",
                &[Value::M1(M1Scalar::FixedPoint7dps(
                    crate::value::FixedPoint7dps::from_raw(-20_000_000)
                ))]
            )
            .unwrap(),
            Value::M1(M1Scalar::FixedPoint7dps(
                crate::value::FixedPoint7dps::from_raw(-12_345_678)
            ))
        );
        assert_eq!(
            h.call(
                "Limited Unsigned",
                "Constrain",
                &[Value::m1_unsigned(u32::MAX)]
            )
            .unwrap(),
            Value::m1_unsigned(10)
        );
    }

    #[test]
    fn set_coerces_then_clamps_to_the_target_rule() {
        let mut h = ProjectObjHarness::new();
        h.call("Limited Signed", "Set", &[Value::m1_unsigned(20)])
            .unwrap();
        assert_eq!(
            h.env.get("Root.Demo.Limited Signed"),
            Some(&Value::m1_integer(3))
        );

        h.call("Limited Float", "Set", &[Value::m1_integer(-10)])
            .unwrap();
        assert_eq!(
            h.env.get("Root.Demo.Limited Float"),
            Some(&Value::m1_float(-1.5))
        );

        assert_eq!(
            h.call("Positive Float", "Set", &[Value::m1_float(-2.0)]),
            Err(EvalError::UnsupportedBuiltin {
                object: "Positive Float".to_string(),
                method: "Set".to_string(),
            }),
            "parameters are calibration-owned and not firmware-writable"
        );
    }

    #[test]
    fn set_does_not_bypass_a_compounds_parameter_default() {
        let mut h = ProjectObjHarness::new();
        h.env.set(
            "Root.Demo.Calibration Compound.Calibration",
            Value::m1_float(1.0),
        );
        h.env
            .set("Root.Demo.Calibration Compound.Value", Value::m1_float(2.0));

        assert_eq!(
            h.call("Calibration Compound", "Set", &[Value::m1_float(3.0)]),
            Err(EvalError::UnsupportedBuiltin {
                object: "Calibration Compound".to_string(),
                method: "Set".to_string(),
            })
        );
        assert_eq!(
            h.env.get("Root.Demo.Calibration Compound.Calibration"),
            Some(&Value::m1_float(1.0))
        );
        assert_eq!(
            h.env.get("Root.Demo.Calibration Compound.Value"),
            Some(&Value::m1_float(2.0))
        );
    }

    #[test]
    fn get_unscheduled_reads_the_exact_stored_scalar() {
        let mut h = ProjectObjHarness::new();
        h.env.set("Root.Demo.Limited Signed", Value::m1_integer(-1));
        assert_eq!(
            h.call("Limited Signed", "GetUnscheduled", &[]).unwrap(),
            Value::m1_integer(-1)
        );
    }

    #[test]
    fn numeric_value_compounds_follow_their_declared_default() {
        let mut h = ProjectObjHarness::new();
        assert_eq!(
            h.call("Limited Compound", "Validate", &[Value::m1_unsigned(5)])
                .unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            h.call("Limited Compound", "Constrain", &[Value::m1_unsigned(5)])
                .unwrap(),
            Value::m1_unsigned(4)
        );
        assert_eq!(
            h.call("Limited Compound", "GetUnscheduled", &[]),
            Err(EvalError::UnsupportedBuiltin {
                object: "Limited Compound".to_string(),
                method: "GetUnscheduled".to_string(),
            }),
            "GetUnscheduled belongs to channels and generated table values"
        );
        h.call("Limited Compound", "Set", &[Value::m1_unsigned(9)])
            .unwrap();
        assert_eq!(
            h.env.get("Root.Demo.Limited Compound.Value"),
            Some(&Value::m1_unsigned(4)),
            "Set writes the concrete value child after applying the compound's range"
        );
    }

    #[test]
    fn get_unscheduled_reads_a_tables_generated_value_channel() {
        let mut h = Harness::new();
        h.env.set("Root.Demo.Map.Value", Value::m1_float(12.5));
        assert_eq!(
            h.call("Map", "GetUnscheduled", &[]).unwrap(),
            Value::m1_float(12.5)
        );
    }

    #[test]
    fn unsupported_object_method_pairs_name_the_exact_call() {
        let mut h = ProjectObjHarness::new();
        for (object, method, args) in [
            ("Mode", "Validate", vec![Value::m1_integer(1)]),
            ("Limited Signed", "AsString", vec![]),
            ("Limited Compound", "GetUnscheduled", vec![]),
            ("Positive Float", "GetUnscheduled", vec![]),
            ("Numeric Constant", "GetUnscheduled", vec![]),
            ("Typed IO", "GetUnscheduled", vec![]),
            ("Startup Delay", "GetUnscheduled", vec![]),
            ("Drive State.Idle", "Set", vec![Value::m1_integer(1)]),
        ] {
            assert_eq!(
                h.call(object, method, &args),
                Err(EvalError::UnsupportedBuiltin {
                    object: object.to_string(),
                    method: method.to_string(),
                })
            );
        }
    }

    #[test]
    fn typed_numeric_project_io_keeps_its_hardware_stub_route() {
        let loaded = crate::loader::load(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/enums/Project.m1prj"),
            None,
        )
        .unwrap();
        let mut h = ProjectObjHarness::new();
        assert_eq!(
            support_in(
                &loaded,
                Some("Root.Demo"),
                Some("Root.Demo.Update"),
                "Typed IO",
                "GetScaled",
            ),
            BuiltinSupport::Stubbed
        );
        assert_eq!(
            h.call("Typed IO", "GetScaled", &[]).unwrap(),
            Value::m1_float(0.0)
        );
        assert_eq!(
            support_in(
                &loaded,
                Some("Root.Demo"),
                Some("Root.Demo.Update"),
                "Limited Float",
                "GetScaled",
            ),
            BuiltinSupport::Unsupported,
            "ordinary numeric channels must not inherit hardware stubs"
        );
    }

    #[test]
    fn core_object_methods_have_capability_parity() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/enums");
        let loaded =
            crate::loader::load(&dir.join("Project.m1prj"), None).expect("enums fixture loads");
        for (object, method) in [
            ("Mode", "AsString"),
            ("Limited Signed", "Validate"),
            ("Limited Signed", "Constrain"),
            ("Limited Signed", "GetUnscheduled"),
            ("Limited Compound", "Validate"),
            ("Limited Compound", "Constrain"),
            ("Limited Compound", "Set"),
            ("Limited Signed", "Set"),
        ] {
            assert_eq!(
                support_in(
                    &loaded,
                    Some("Root.Demo"),
                    Some("Root.Demo.Update"),
                    object,
                    method,
                ),
                BuiltinSupport::Direct,
                "{object}.{method}"
            );
        }
        for (object, method) in [
            ("Mode", "Constrain"),
            ("Limited Signed", "AsString"),
            ("Limited Compound", "GetUnscheduled"),
            ("Positive Float", "GetUnscheduled"),
            ("Positive Float", "Set"),
            ("Calibration Compound", "Set"),
            ("Numeric Constant", "GetUnscheduled"),
            ("Typed IO", "GetUnscheduled"),
            ("Startup Delay", "GetUnscheduled"),
        ] {
            assert_eq!(
                support_in(
                    &loaded,
                    Some("Root.Demo"),
                    Some("Root.Demo.Update"),
                    object,
                    method,
                ),
                BuiltinSupport::Unsupported,
                "{object}.{method}"
            );
        }
    }

    #[test]
    fn timer_dispatch_requires_a_timer_receiver() {
        let mut h = ProjectObjHarness::new();
        assert_eq!(
            h.call("Startup Delay", "Start", &[Value::m1_float(0.03)])
                .unwrap(),
            Value::Bool(true)
        );
        let remaining = h
            .call("Startup Delay", "Remaining", &[])
            .unwrap()
            .m1_scalar()
            .unwrap()
            .as_f64();
        assert!((remaining - 0.02).abs() < 1e-7);
        match h.call("Precharge State", "Start", &[Value::m1_float(0.03)]) {
            Err(EvalError::UnsupportedBuiltin { .. }) => {}
            other => panic!("expected channel.Start to be unsupported, got {other:?}"),
        }
    }

    #[test]
    fn set_is_classified_supported() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/enums");
        let loaded =
            crate::loader::load(&dir.join("Project.m1prj"), None).expect("enums fixture loads");
        assert_eq!(
            support_in(
                &loaded,
                Some("Root.Demo"),
                Some("Root.Demo.Update"),
                "Precharge State",
                "Set"
            ),
            BuiltinSupport::Direct
        );
        assert_eq!(
            support_in(
                &loaded,
                Some("Root.Demo"),
                Some("Root.Demo.Update"),
                "Fan Output",
                "Set"
            ),
            BuiltinSupport::Stubbed
        );
    }

    #[test]
    fn project_object_catalog_matches_coverage_and_runtime_dispatch() {
        let mut harness = ProjectObjHarness::new();
        let scripts = parse_all(&[(
            "Demo.Update.m1scr".to_string(),
            "Mode.AsInteger();\n\
             Mode.AsString();\n\
             Precharge State.Set(1);\n\
             Limited Signed.Validate(1);\n\
             Limited Signed.Constrain(1);\n\
             Limited Signed.GetUnscheduled();\n\
             Startup Delay.Start(1.0);\n\
             Startup Delay.Stop();\n\
             Startup Delay.Reset();\n\
             Startup Delay.Remaining();\n\
             Service Bits.Update();\n\
             Fan Output.Set(1);\n\
             DashVals.TxOpen();\n\
             DashVals.Aux Switch.GetScaled();\n\
             DashVals.Aux Switch.Receive();\n"
                .to_string(),
        )]);
        let report = crate::coverage::CoverageReport::analyse_in(&scripts, Some(&harness.project));

        for name in [
            "Mode.AsInteger",
            "Mode.AsString",
            "Precharge State.Set",
            "Limited Signed.Validate",
            "Limited Signed.Constrain",
            "Limited Signed.GetUnscheduled",
        ] {
            assert!(
                report.supported.iter().any(|item| item.name == name),
                "{name} should be supported: {report:?}"
            );
        }
        for name in [
            "Startup Delay.Start",
            "Startup Delay.Stop",
            "Startup Delay.Reset",
            "Startup Delay.Remaining",
        ] {
            assert!(
                report.assumed.iter().any(|item| item.name == name),
                "{name} should be assumed: {report:?}"
            );
        }
        for name in [
            "Service Bits.Update",
            "Fan Output.Set",
            "DashVals.TxOpen",
            "DashVals.Aux Switch.GetScaled",
            "DashVals.Aux Switch.Receive",
        ] {
            assert!(
                report.stubbed.iter().any(|item| item.name == name),
                "{name} should be stubbed: {report:?}"
            );
        }

        let enum_id = harness
            .project
            .symbols()
            .enum_by_name("Drive State")
            .unwrap();
        harness.env.set(
            "Root.Demo.Mode",
            Value::Enum {
                id: enum_id,
                member: "Idle".to_string(),
            },
        );
        harness
            .env
            .set("Root.Demo.Limited Signed", Value::m1_integer(1));
        let calls = [
            ("Mode", "AsInteger", vec![]),
            ("Mode", "AsString", vec![]),
            ("Precharge State", "Set", vec![Value::m1_integer(1)]),
            ("Limited Signed", "Validate", vec![Value::m1_integer(1)]),
            ("Limited Signed", "Constrain", vec![Value::m1_integer(1)]),
            ("Limited Signed", "GetUnscheduled", vec![]),
            ("Startup Delay", "Start", vec![Value::m1_float(1.0)]),
            ("Startup Delay", "Stop", vec![]),
            ("Startup Delay", "Reset", vec![]),
            ("Startup Delay", "Remaining", vec![]),
            ("Service Bits", "Update", vec![]),
            ("Fan Output", "Set", vec![Value::m1_integer(1)]),
            ("DashVals", "TxOpen", vec![]),
            ("DashVals.Aux Switch", "GetScaled", vec![]),
            ("DashVals.Aux Switch", "Receive", vec![]),
        ];
        for (object, method, args) in calls {
            if let Err(error) = harness.call(object, method, &args) {
                panic!(
                    "coverage classified {object}.{method} executable, dispatch failed: {error}"
                );
            }
        }

        let mini = mini_loaded();
        let table_scripts = parse_all(&[(
            "Demo.Update.m1scr".to_string(),
            "Map.Lookup(0.0, 0.0);\nMap.Get(0);\n".to_string(),
        )]);
        let table_report =
            crate::coverage::CoverageReport::analyse_in(&table_scripts, Some(&mini.project));
        assert!(
            table_report
                .assumed
                .iter()
                .any(|item| item.name == "Map.Lookup")
        );
        assert!(
            table_report
                .supported
                .iter()
                .any(|item| item.name == "Map.Get")
        );
        let mut table_harness = Harness::new();
        table_harness
            .call(
                "Map",
                "Lookup",
                &[Value::m1_float(0.0), Value::m1_float(0.0)],
            )
            .expect("coverage-classified table lookup dispatches");
        table_harness
            .call("Map", "Get", &[Value::m1_integer(0)])
            .expect("coverage-classified table get dispatches");
    }

    // ---- Task 7: project-object IO stubs ----

    #[test]
    fn dbc_message_tx_open_returns_opaque_handle_and_is_external() {
        let mut h = ProjectObjHarness::new();
        // A CAN message object's `.TxOpen()` cannot be evaluated offline; it
        // returns a documented opaque handle and is flagged externally driven.
        assert_eq!(
            h.call("DashVals", "TxOpen", &[]).unwrap(),
            Value::m1_unsigned(0)
        );
        assert!(h.trace.is_external("DashVals.TxOpen"));
    }

    #[test]
    fn dbc_void_writers_return_unit_value() {
        let mut h = ProjectObjHarness::new();
        // The void CAN writers all return the unit value (a no-op offline).
        for method in ["Tx", "TxInitialise", "Init", "SetBit", "SetUnsignedInteger"] {
            assert_eq!(
                h.call("DashVals", method, &[]).unwrap(),
                Value::Bool(true),
                "{method} should return the unit value"
            );
        }
    }

    #[test]
    fn dbc_signal_receive_is_false_offline() {
        let mut h = ProjectObjHarness::new();
        // No CAN message arrives offline, so `.Receive()` is false.
        assert_eq!(
            h.call("DashVals.Aux Switch", "Receive", &[]).unwrap(),
            Value::Bool(false)
        );
        assert!(h.trace.is_external("DashVals.Aux Switch.Receive"));
    }

    #[test]
    fn dbc_signal_get_scaled_is_zero_offline() {
        let mut h = ProjectObjHarness::new();
        // A CAN signal read has no offline value; the documented stub is 0.0 so a
        // whole-project run does not abort on every CAN read.
        assert_eq!(
            h.call("DashVals.Aux Switch", "GetScaled", &[]).unwrap(),
            Value::m1_float(0.0)
        );
        assert!(h.trace.is_external("DashVals.Aux Switch.GetScaled"));
    }

    #[test]
    fn io_stub_scenario_override_wins() {
        let mut h = ProjectObjHarness::new();
        // A scenario can externally drive a CAN read (e.g. from a log replay).
        h.env
            .set_io_override("DashVals.Aux Switch.GetScaled", Value::m1_float(42.0));
        assert_eq!(
            h.call("DashVals.Aux Switch", "GetScaled", &[]).unwrap(),
            Value::m1_float(42.0)
        );
        assert!(h.trace.is_external("DashVals.Aux Switch.GetScaled"));
    }

    #[test]
    fn group_compound_update_is_a_void_stub() {
        let mut h = ProjectObjHarness::new();
        // `Service Bits.Update()` (a GroupCompound CAN service-bits push) is an
        // externally-driven void writer.
        assert_eq!(
            h.call("Service Bits", "Update", &[]).unwrap(),
            Value::Bool(true)
        );
        assert!(h.trace.is_external("Service Bits.Update"));
    }

    #[test]
    fn output_set_state_is_a_void_stub() {
        let mut h = ProjectObjHarness::new();
        // `Fan Output.SetState(...)` (a package Output object) is a void writer.
        assert_eq!(
            h.call("Fan Output", "SetState", &[Value::Bool(true)])
                .unwrap(),
            Value::Bool(true)
        );
        assert!(h.trace.is_external("Fan Output.SetState"));
    }

    #[test]
    fn output_drive_reference_set_is_a_void_stub() {
        let mut h = ProjectObjHarness::new();
        // `.Set` on a package/reference output member (`ASSI Yellow.Drive.Set(...)`
        // in AV-M1, a `BuiltIn.Reference` with `TargetCreation="AutoChannel"`) is
        // a hardware output-drive command: a void writer offline. A real channel
        // `.Set` never reaches this route — `try_channel_set` claims it first.
        assert_eq!(
            h.call(
                "Fan Output",
                "Set",
                &[Value::Enum {
                    id: 1,
                    member: "High Side".to_string(),
                }],
            )
            .unwrap(),
            Value::Bool(true)
        );
        assert!(h.trace.is_external("Fan Output.Set"));
    }

    #[test]
    fn buzzer_buzze_is_a_void_stub() {
        let mut h = ProjectObjHarness::new();
        // The buzzer's `.Buzze` is an externally-driven void writer (the buzzer is
        // hardware we cannot actuate offline).
        assert_eq!(
            h.call("Fan Output", "Buzze", &[]).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn unknown_object_method_still_fails_loud() {
        let mut h = ProjectObjHarness::new();
        // A project object with a method that is neither a setter, an enum
        // accessor, a Timer method, nor a known IO stub fails loud — never a guess.
        match h.call("Fan Output", "NotAKnownMethod", &[]) {
            Err(EvalError::UnsupportedBuiltin { object, method }) => {
                assert_eq!(object, "Fan Output");
                assert_eq!(method, "NotAKnownMethod");
            }
            other => panic!("expected UnsupportedBuiltin, got {other:?}"),
        }
    }

    #[test]
    fn phase15_calculate_overloads_are_classified_supported() {
        // Every Tier-1 pure Calculate overload P15-A added must report Supported so
        // the coverage report agrees with the dispatch table.
        for method in [
            "Absolute",
            "Average",
            "NAN",
            "Infinity",
            "IsFinite",
            "MaximumFloat",
            "InverseSin",
            "InverseCos",
            "InverseTan",
        ] {
            assert_eq!(
                classify_builtin("Calculate", method),
                BuiltinSupport::Direct,
                "Calculate.{method} should be Supported"
            );
        }
    }

    #[test]
    fn library_catalog_matches_coverage_and_runtime_dispatch() {
        let mut cases = SUPPORTED_PURE_METHODS
            .iter()
            .map(|&(object, method)| (object, method, BuiltinSupport::Direct))
            .chain(
                MODELED_STATEFUL_METHODS
                    .iter()
                    .chain(MODELED_MATH_METHODS.iter())
                    .map(|&(object, method)| (object, method, BuiltinSupport::Modeled)),
            )
            .collect::<Vec<_>>();
        for &object in STUB_OBJECTS {
            let library = intrinsics::get()
                .library_object(object)
                .expect("stub object is in the intrinsic catalog");
            cases.extend(
                library
                    .functions
                    .iter()
                    .map(|overload| (object, overload.name.as_str(), BuiltinSupport::Stubbed)),
            );
        }
        cases.sort_by_key(|(object, method, _)| (*object, *method));
        cases.dedup_by_key(|(object, method, _)| (*object, *method));

        let mut source = String::new();
        for (object, method, _) in &cases {
            let overload = intrinsics::get().library_overloads(object, method)[0];
            let args = overload
                .params
                .iter()
                .map(|param| match param.ty.as_str() {
                    "Boolean" => "true",
                    "String" => "\"x\"",
                    _ => "1",
                })
                .collect::<Vec<_>>()
                .join(", ");
            source.push_str(&format!("{object}.{method}({args});\n"));
        }
        let scripts = parse_all(&[("Demo.Update.m1scr".to_string(), source)]);
        let report = crate::coverage::CoverageReport::analyse(&scripts);

        for (object, method, expected) in cases {
            let name = format!("{object}.{method}");
            let bucket = match expected {
                BuiltinSupport::Direct => &report.supported,
                BuiltinSupport::Modeled => &report.assumed,
                BuiltinSupport::Stubbed => &report.stubbed,
                _ => unreachable!("catalog test covers executable methods"),
            };
            assert!(
                bucket.iter().any(|item| item.name == name),
                "coverage did not put {name} in {expected:?}: {report:?}"
            );

            let overload = intrinsics::get().library_overloads(object, method)[0];
            let args: Vec<Value> = overload
                .params
                .iter()
                .map(|param| match param.ty.as_str() {
                    "Boolean" => Value::Bool(true),
                    "Integer" => Value::m1_integer(1),
                    "UnsignedInteger" => Value::m1_unsigned(1),
                    "String" => Value::Str("x".to_string()),
                    _ => Value::m1_float(1.0),
                })
                .collect();
            let mut harness = Harness::new();
            match harness.call(object, method, &args) {
                Err(EvalError::UnsupportedBuiltin { .. }) => {
                    panic!("coverage says {expected:?}, but dispatch rejects {name}")
                }
                Err(error) => panic!("dispatch failed for classified {name}: {error}"),
                Ok(_) => {}
            }
        }
    }

    #[test]
    fn project_io_stub_catalog_matches_classification_and_dispatch() {
        let mut harness = ProjectObjHarness::new();
        // Exercise every entry in the shared project-object catalog through both
        // coverage's classifier and runtime's dispatcher. This catches drift in
        // either direction when a new IO method is added.
        for &method in io_stub::PROJECT_OBJECT_STUB_METHODS {
            assert_eq!(
                classify_builtin("DashVals", method),
                BuiltinSupport::Stubbed,
                "{method} should be a stub"
            );
            harness
                .call("DashVals", method, &[])
                .unwrap_or_else(|error| panic!("stubbed DashVals.{method} failed: {error}"));
        }
    }

    #[test]
    fn io_library_methods_are_classified_stubbed() {
        // Every method on a Tier-3 IO *library* object (CanComms/Serial/System/
        // Logging) the generic typed-default stub now handles must classify as
        // Stubbed, so coverage stays consistent with what the IO stub returns at
        // runtime — including the `CanComms.*` reads/setup the old design left
        // unstubbed (the EV-M1 whole-project blocker this fix closed).
        let cases = [
            ("CanComms", "RxOpenStandard"),     // Handle -> unit stub
            ("CanComms", "GetFloat"),           // FloatingPoint -> 0.0
            ("CanComms", "GetUnsignedInteger"), // Integer -> 0
            ("CanComms", "RxMessage"),          // Boolean -> false
            ("CanComms", "SetFloat"),           // Void -> unit
            ("Serial", "GetFloat"),
            ("System", "ElapsedTime"),
            ("System", "TickPeriod"),
            ("Logging", "Running"),
        ];
        for (object, method) in cases {
            assert_eq!(
                classify_builtin(object, method),
                BuiltinSupport::Stubbed,
                "{object}.{method} should be a stub"
            );
        }
    }
}
