// SPDX-License-Identifier: GPL-3.0-or-later
//! Inline user-function call evaluation (P15-D).
//!
//! A call to a *user* function or method — `Slip Control.Update(...)`,
//! `Torque Vectoring.Update(...)`, or a bare `Helper(...)` — is evaluated by
//! executing the callee's backing `.m1scr` body inline, in a fresh local frame,
//! and reading back whatever the body assigned to its `Out` return object.
//!
//! This is distinct from a *library* builtin (`Calculate.Max`, `Filter.*`): a
//! user function is a project [`SymbolKind::Function`]/[`SymbolKind::Method`]
//! symbol (`BuiltIn.FuncUser*`/`BuiltIn.CalFuncUser*`/`BuiltIn.FuncGenerated`/
//! `BuiltIn.MethodUser`) with a backing script discovered at load time, not a
//! firmware-library object. [`call`] returns `Ok(None)` for anything that is not
//! such a function-with-a-script, so the dispatcher can fall through.
//!
//! ## Frame discipline
//!
//! Calling a user function is a real frame switch:
//!
//! 1. **Save** the caller frame: a snapshot of `env.locals` and the `Out` slot.
//! 2. **Enter** a fresh local frame (`Env::enter_function`) and bind each
//!    declared parameter `(name, type)` as an M1-width local `In.<name>` (exactly the key
//!    `m1-typecheck`'s `resolve` hands `In.<Param>` references, and that
//!    `ident::classify` maps to `Target::Local("In.<param>")`).
//! 3. **Execute** the callee body with `ctx.group`/`ctx.fn_symbol`/
//!    `ctx.script_name` retargeted to the callee, so its group-relative names,
//!    its `static local`s (keyed by the callee symbol), and its stateful operator
//!    state (keyed by the callee script's call sites) are all correct and
//!    isolated from the caller.
//! 4. **Read** the `Out` slot for the return value (a unit value when the body
//!    assigns no `Out`).
//! 5. **Restore** the caller frame: `env.leave_function`, then the saved locals
//!    and `Out` slot, so the caller's variables survive the call unclobbered.
//!
//! Statics persist (that is their purpose) — the callee's `static local`s key off
//! its own `fn_symbol`, the caller's off theirs, so they never collide. Channel
//! writes the callee makes also persist (a user function may write project
//! channels as a side effect), which is the intended M1 semantics.
//!
//! ## Recursion guard
//!
//! A static cycle check is an upstream concern (T097), but the evaluator must
//! still never infinite-loop on a runtime cycle. [`call`] increments
//! `ctx.depth` per nested entry and fails loud past `MAX_CALL_DEPTH` rather than
//! overflowing the stack.

use crate::error::EvalError;
use crate::expr::{EvalCtx, coerce_for_declared_type};
use crate::ident::{Target, classify};
use crate::stmt::exec_script;
use crate::value::Value;
use m1_typecheck::parsed::ParsedScript;
use m1_typecheck::symbols::SymbolKind;

/// The maximum inline user-function call depth before a runtime cycle fails loud.
/// Generous enough for any real EV-M1 control-call chain, low enough to abort a
/// pathological recursion long before the native stack overflows.
const MAX_CALL_DEPTH: u32 = 64;

/// The unit value a void user function (one that assigns no `Out`) returns. Reuse
/// the same convention the IO void writers and the channel setter use, so an
/// expression-statement call (`Slip Control.Update(...)`) succeeds with a value
/// that is simply discarded.
fn unit() -> Value {
    Value::Bool(true)
}

/// Evaluate an inline user-function call `callee_path(args)`.
///
/// Returns:
/// - `Ok(None)` when `callee_path` is not a user `Function`/`Method` symbol with a
///   backing script — so the dispatcher can fall through to other routing;
/// - `Ok(Some(return_value))` when the call evaluates (the callee body's `Out`
///   value, or the unit value when it assigns no `Out`);
/// - a fail-loud [`EvalError`] on an arity mismatch ([`EvalError::BadCall`]), a
///   too-deep recursion ([`EvalError::UnsupportedConstruct`]), or any error the
///   callee body itself surfaces.
pub fn call(
    callee_path: &str,
    args: &[Value],
    ctx: &mut EvalCtx,
) -> Result<Option<Value>, EvalError> {
    // The callee must resolve to a project `Function`/`Method` symbol. Anything
    // else (a channel, a library object, an unresolved name) is not a user-function
    // call — fall through.
    let Target::Symbol(canon) = classify(
        callee_path,
        ctx.group,
        ctx.fn_symbol,
        ctx.project,
        &ctx.env.locals,
    ) else {
        return Ok(None);
    };
    let Some(symbol) = ctx.project.symbols().get(&canon) else {
        return Ok(None);
    };
    if !matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method) {
        return Ok(None);
    }

    // Find the backing script: the one whose `function_symbol_for_script` maps to
    // this callee path (the reverse direction of the load-time association).
    let Some(script) = backing_script(&canon, ctx.scripts, ctx) else {
        // A function symbol with no backing script (e.g. a firmware function with
        // no `.m1scr` in this project) is not inline-evaluable here — fall through
        // so the dispatcher can fail loud with a clearer unsupported error.
        return Ok(None);
    };

    // Arity check: when the callee declares a `<Signature>`, the argument count
    // must match its parameter count exactly — a mismatch is a fail-loud BadCall,
    // never a silent truncation/padding. A `None` signature leaves args unchecked
    // (opaque), matching how `resolve` treats an unsigned function's `In.*`.
    let in_params = ctx
        .project
        .symbols()
        .get(&canon)
        .and_then(|s| s.in_params.clone());
    if let Some(params) = &in_params
        && params.len() != args.len()
    {
        return Err(EvalError::BadCall {
            detail: format!(
                "user function {canon:?} expects {} argument(s), got {}",
                params.len(),
                args.len()
            ),
        });
    }

    // Recursion guard: refuse to enter past a fixed depth so a runtime call cycle
    // fails loud instead of overflowing the stack.
    if ctx.depth >= MAX_CALL_DEPTH {
        return Err(EvalError::UnsupportedConstruct {
            kind: format!(
                "recursive user-function call (depth {} exceeded) into {canon:?}",
                MAX_CALL_DEPTH
            ),
            at: 0,
        });
    }

    // Apply the same target-family conversion as assignment before switching
    // frames. Computing every binding first means a conversion error cannot
    // leave the caller frame half-replaced.
    let bound_args = match &in_params {
        Some(params) => params
            .iter()
            .zip(args.iter())
            .map(|((name, ty), value)| {
                let target = format!("{canon}.In.{name}");
                match ctx
                    .signature_m1_types
                    .and_then(|types| types.param_kind(&canon, name))
                {
                    Some(kind) => crate::expr::coerce_for_scalar_kind(&target, value.clone(), kind),
                    None => coerce_for_declared_type(&target, value.clone(), *ty, ctx.project),
                }
            })
            .collect::<Result<Vec<_>, _>>()?,
        None => args.to_vec(),
    };

    // The callee's lexical context.
    let callee_group = ctx.project.group_for_script(&script.name);
    let callee_script_name = script.name.clone();
    let callee_root = script.cst.root();

    // 1. Save the caller frame: its locals and its `Out` slot. The callee runs in
    //    a fresh frame; both are restored verbatim afterwards.
    let saved_locals = std::mem::take(&mut ctx.env.locals);
    let saved_out = ctx.env.clear_out();

    // 2. Enter the callee frame (empty locals) and bind each parameter as the
    //    local `In.<name>` the callee body reads.
    ctx.env.enter_function();
    if let Some(params) = &in_params {
        for ((name, _ty), value) in params.iter().zip(bound_args) {
            ctx.env.set_local(format!("In.{name}"), value);
        }
    }

    // 3. Execute the callee body with the context retargeted to the callee. The
    //    `env`/`state`/`trace` are shared (channel writes, statics, and stateful
    //    operator state all persist across the call, keyed by the callee's own
    //    symbol/script — so isolation is automatic). The result is captured so the
    //    caller frame is always restored, even on an error.
    // A trace tick is already open for normal and counterfactual execution.
    // Capture its instant before borrowing the trace mutably for the callee.
    // During the once-only startup pass no trace exists, so `None` retains the
    // explicit `at startup` rendering.
    let t = ctx
        .trace
        .as_deref()
        .and_then(|trace| trace.time.last().copied());
    let exec_result = {
        let mut callee_ctx = EvalCtx {
            project: ctx.project,
            calib: ctx.calib,
            env: ctx.env,
            state: ctx.state,
            group: callee_group.as_deref(),
            fn_symbol: Some(canon.as_str()),
            script_name: &callee_script_name,
            dt: ctx.dt,
            scripts: ctx.scripts,
            signature_m1_types: ctx.signature_m1_types,
            depth: ctx.depth + 1,
            trace: ctx.trace.as_deref_mut(),
        };
        exec_script(&callee_root, &mut callee_ctx).map_err(|e| e.in_script(&callee_script_name, t))
    };

    // 4. Read the return value (the `Out` slot), then 5. restore the caller frame.
    let return_value = ctx.env.get_out().cloned();
    ctx.env.leave_function();
    ctx.env.locals = saved_locals;
    ctx.env.out = saved_out;

    // Surface a callee error only after the caller frame is restored. It already
    // carries the callee script context; outer inline-call and runner boundaries
    // preserve that deepest context rather than replacing or nesting it.
    exec_result?;

    Ok(Some(return_value.unwrap_or_else(unit)))
}

/// Find the [`ParsedScript`] whose backing function symbol is `callee_path`. This
/// is the reverse of `function_symbol_for_script`: for each script, compare its
/// derived function-symbol path to the callee. Returns the first match in the
/// project's deterministic (sorted-by-name) script order.
fn backing_script<'a>(
    callee_path: &str,
    scripts: &'a [ParsedScript],
    ctx: &EvalCtx,
) -> Option<&'a ParsedScript> {
    scripts
        .iter()
        .find(|s| ctx.project.function_symbol_for_script(&s.name).as_deref() == Some(callee_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calib::Calibration;
    use crate::env::{Env, StateStore};
    use crate::loader::Loaded;
    use crate::value::M1Scalar;
    use std::path::Path;

    /// A harness over the synthetic `userfn` fixture: a caller (`Caller.Update`)
    /// and a helper `FuncUserParam` (`Root.Helper.Compute`, `<Param x:f32>`, body
    /// `Out = In.x * 2.0;`). Owns the stores so a fresh retargetable `EvalCtx` can
    /// be built per call — and crucially threads `loaded.scripts` so `call` can
    /// find the helper's backing script.
    struct Harness {
        loaded: Loaded,
        calib: Calibration,
        env: Env,
        state: StateStore,
    }

    impl Harness {
        fn new() -> Harness {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/userfn");
            let loaded = crate::loader::load(&dir.join("Project.m1prj"), None)
                .expect("userfn fixture loads");
            Harness {
                loaded,
                calib: Calibration::default(),
                env: Env::new(),
                state: StateStore::new(),
            }
        }

        /// Call the helper through `userfn::call`, with the caller's lexical
        /// context (group `Root.Caller`, fn `Root.Caller.Update`).
        fn call(&mut self, callee: &str, args: &[Value]) -> Result<Option<Value>, EvalError> {
            // Split the immutable project/scripts borrow from the mutable stores.
            let project = &self.loaded.project;
            let scripts = &self.loaded.scripts;
            let mut ctx = EvalCtx {
                project,
                calib: &self.calib,
                env: &mut self.env,
                state: &mut self.state,
                group: Some("Root.Caller"),
                fn_symbol: Some("Root.Caller.Update"),
                script_name: "Caller.Update.m1scr",
                dt: 0.01,
                scripts,
                signature_m1_types: Some(&self.loaded.signature_m1_types),
                depth: 0,
                trace: None,
            };
            call(callee, args, &mut ctx)
        }
    }

    #[test]
    fn calls_helper_and_reads_out_return() {
        let mut h = Harness::new();
        // Helper.Compute(3.0) → Out = In.x * 2.0 = 6.0.
        assert_eq!(
            h.call("Root.Helper.Compute", &[Value::m1_float(3.0)])
                .unwrap(),
            Some(Value::m1_float(6.0))
        );
        // And the group-relative spelling the caller would write resolves too.
        assert_eq!(
            h.call("Helper.Compute", &[Value::m1_float(5.0)]).unwrap(),
            Some(Value::m1_float(10.0))
        );
    }

    #[test]
    fn signature_types_coerce_parameters_and_returns_to_binary32() {
        let mut h = Harness::new();
        assert_eq!(
            h.call("Root.Helper.Echo", &[Value::m1_integer(7)]).unwrap(),
            Some(Value::m1_float(7.0))
        );
        assert_eq!(
            h.call("Root.Helper.ReturnInt", &[]).unwrap(),
            Some(Value::m1_float(7.0))
        );
    }

    #[test]
    fn raw_signature_metadata_directs_fixed_point_boundaries() {
        let mut h = Harness::new();
        let fixed = Value::M1(M1Scalar::FixedPoint7dps(
            crate::value::FixedPoint7dps::from_raw(12_345_678),
        ));
        assert_eq!(
            h.call("Root.Helper.FixedEcho", std::slice::from_ref(&fixed))
                .unwrap(),
            Some(fixed)
        );
        assert!(
            h.call("Root.Helper.FixedEcho", &[Value::m1_float(1.0)])
                .is_err()
        );
        assert_eq!(
            h.call("Root.Helper.FixedToFloat", &[Value::m1_float(0.0)])
                .unwrap(),
            Some(Value::m1_float(0.0))
        );
        assert_eq!(
            h.call("Root.Helper.InferredFixed", &[Value::m1_float(0.0)])
                .unwrap(),
            Some(Value::M1(M1Scalar::FixedPoint7dps(
                crate::value::FixedPoint7dps::ZERO,
            )))
        );
    }

    #[test]
    fn caller_locals_survive_the_call() {
        let mut h = Harness::new();
        // Seed a caller local; calling the helper (which binds its own In.x and
        // runs in a fresh frame) must not clobber it.
        h.env.set_local("y", Value::m1_integer(42));
        let _ = h
            .call("Root.Helper.Compute", &[Value::m1_float(1.0)])
            .unwrap();
        assert_eq!(h.env.get_local("y"), Some(&Value::m1_integer(42)));
        // The callee's In.x binding did NOT leak into the caller frame.
        assert_eq!(h.env.get_local("In.x"), None);
        // Nor did the callee's Out slot.
        assert_eq!(h.env.get_out(), None);
    }

    #[test]
    fn this_anchored_nested_callee_resolves() {
        let mut h = Harness::new();
        // `This.Sub.Checkup()` from group Root.Caller names the FuncUserParam
        // Root.Caller.Sub.Checkup (no Signature, zero args) — the exact shape of
        // AV-M1's `This.AV.DFMM.Checkup();`. The `This` anchor must expand to
        // the enclosing group before classification.
        assert_eq!(
            h.call("This.Sub.Checkup", &[]).unwrap(),
            Some(Value::m1_float(7.0))
        );
    }

    #[test]
    fn non_function_path_falls_through() {
        let mut h = Harness::new();
        // A project channel is not a user function — Ok(None) so dispatch falls
        // through rather than trying to execute it as a body.
        assert_eq!(h.call("Root.Caller.Output", &[]).unwrap(), None);
        // An unresolved name likewise falls through.
        assert_eq!(h.call("No.Such.Function", &[]).unwrap(), None);
    }

    #[test]
    fn arity_mismatch_is_bad_call() {
        let mut h = Harness::new();
        // The helper declares one parameter; zero or two args is a fail-loud
        // BadCall, never a silent pad/truncate.
        match h.call("Root.Helper.Compute", &[]) {
            Err(EvalError::BadCall { .. }) => {}
            other => panic!("expected BadCall on zero args, got {other:?}"),
        }
        match h.call(
            "Root.Helper.Compute",
            &[Value::m1_float(1.0), Value::m1_float(2.0)],
        ) {
            Err(EvalError::BadCall { .. }) => {}
            other => panic!("expected BadCall on two args, got {other:?}"),
        }
    }

    #[test]
    fn unbounded_recursion_fails_loud_at_depth_guard() {
        let mut h = Harness::new();
        // `Recur.Loop` calls itself unconditionally (`Out = Recur.Loop(In.n)`).
        // Without a guard this overflows the stack; the depth guard turns it into
        // a fail-loud UnsupportedConstruct instead.
        let err = h
            .call("Root.Recur.Loop", &[Value::m1_float(1.0)])
            .expect_err("unbounded recursion fails loud");
        assert!(
            err.to_string().contains("Recur.Loop.m1scr"),
            "callee context names the recursively failing script: {err}"
        );
        match err.root_cause() {
            EvalError::UnsupportedConstruct { kind, .. } => {
                assert!(
                    kind.contains("recursive"),
                    "expected a recursion error, got {kind:?}"
                );
            }
            other => panic!("expected fail-loud recursion error, got {other:?}"),
        }
    }

    #[test]
    fn nested_call_state_is_isolated_by_frame() {
        let mut h = Harness::new();
        // Two calls with different arguments compute independently — the second
        // does not see the first's In.x (fresh frame each time).
        assert_eq!(
            h.call("Root.Helper.Compute", &[Value::m1_float(2.0)])
                .unwrap(),
            Some(Value::m1_float(4.0))
        );
        assert_eq!(
            h.call("Root.Helper.Compute", &[Value::m1_float(7.0)])
                .unwrap(),
            Some(Value::m1_float(14.0))
        );
    }
}
