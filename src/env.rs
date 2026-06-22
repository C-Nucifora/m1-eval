// SPDX-License-Identifier: GPL-3.0-or-later
//! The runtime value store ([`Env`]) and per-call-site state map ([`StateStore`]).
//!
//! ## [`Env`] — the value store
//!
//! Three flat maps keyed by canonical path / variable name:
//!
//! - `values` — channel/parameter values addressed by their canonical symbol
//!   path (e.g. `"Root.Demo.Cooling Fan.Output"`). Persists for the whole run.
//! - `locals` — function-local `local` variables for the *currently executing*
//!   function. Cleared on [`Env::leave_function`].
//! - `statics` — `static local` variables, keyed by their owning function +
//!   variable name so two functions' `static local x` do not collide. Persist
//!   across [`Env::enter_function`]/[`Env::leave_function`] for the whole run.
//!
//! Paths may contain spaces (M1 identifiers like `Cooling Fan` are legal); the
//! store treats the whole path as one opaque key and never splits on whitespace.
//!
//! ## [`StateStore`] — per-call-site stateful-operator state
//!
//! Stateful builtins (`Filter.FirstOrder`, `Integral.Normal`, `Delay.Rising`,
//! `static local` timers, …) keep state across ticks. Each occurrence in the
//! source is one independent state machine, identified by its [`CallSite`]
//! (script basename + byte offset of the call node — stable across ticks for a
//! fixed parse). The concrete per-operator state lives in [`OpState`], filled in
//! by the M6 stateful-builtin milestone; this module provides the keyed slot.

use crate::value::Value;
use m1_core::Node;
use std::collections::HashMap;

/// A stable identity for one stateful-operator occurrence in the source: the
/// script basename and the byte offset of its call node. Stable across ticks for
/// a fixed parse, so a `Filter`/`Integral`/`Delay` keeps its state between ticks.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CallSite(pub String, pub usize);

impl CallSite {
    /// Construct a call site from a script name and the byte offset of the call
    /// node's start. Centralised so callers do not hand-build the tuple.
    pub fn new(script: impl Into<String>, byte_offset: usize) -> CallSite {
        CallSite(script.into(), byte_offset)
    }

    /// The stable call-site identity of `node` in `script_name`: the script
    /// basename plus the byte offset of the node's start. For a fixed parse this
    /// is identical across every tick that re-evaluates the same node, so a
    /// stateful operator (M6) keeps its per-occurrence state between ticks.
    /// Identifiers may contain spaces, but a byte offset never does — this key is
    /// whitespace-agnostic by construction.
    pub fn of(script_name: &str, node: &Node) -> CallSite {
        CallSite(script_name.to_string(), node.byte_range().start)
    }

    /// The script basename component.
    pub fn script(&self) -> &str {
        &self.0
    }

    /// The byte-offset component.
    pub fn offset(&self) -> usize {
        self.1
    }
}

/// Per-call-site state for a stateful builtin.
///
/// A freshly-keyed site starts [`OpState::Uninit`]; the operator seeds the right
/// variant on its first tick (so the discretisation has a defined previous
/// value). One variant per M6 operator family. The variants are simple data
/// holders — the update laws live in [`crate::builtins::stateful`], documented in
/// our own words. The state is keyed by [`CallSite`], so two textual occurrences
/// of the same operator keep independent state, and re-evaluating the same node
/// each tick advances the same state machine.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum OpState {
    /// No state has been recorded for this call site yet (first tick).
    #[default]
    Uninit,
    /// First-order filter family (`Filter.FirstOrder/Maximum/Minimum`): the
    /// previous filtered output `y[n-1]`.
    Filter { y: f64 },
    /// `Integral.Normal`: the running clamped accumulator and the previous input
    /// (for trapezoidal area).
    Integral { acc: f64, prev_x: f64 },
    /// `Derivative.{Normal,Filtered}`: the previous input, plus the previous
    /// filtered-derivative output for the `Filtered` variant.
    Derivative { prev_x: f64, prev_d: f64 },
    /// `Derivative.Adaptive`: the input value at the last accepted update, the
    /// previous emitted derivative, and the time elapsed since the last update.
    DerivativeAdaptive {
        last_x: f64,
        prev_d: f64,
        elapsed: f64,
    },
    /// Debounce/Delay timer family (`Delay.Rising/Falling`, `Debounce.*`,
    /// `Calculate.Stable/Hysteresis/Between/Beyond`, `Change.*` filtered):
    /// the currently-held output, the candidate condition being timed, and the
    /// time the candidate has been held.
    Timed {
        output: bool,
        candidate: bool,
        held: f64,
    },
    /// `Change.{By,Up,Down}`: the previous numeric argument value, plus a timer
    /// and pending flag for the filtered overloads.
    ChangeBy {
        prev_x: f64,
        held: f64,
        pending: bool,
    },
    /// `Change.{To,From,Either}`: the previous boolean condition, plus a timer
    /// for the filtered overloads.
    ChangeEdge { prev: bool, held: f64 },
    /// A countdown `Timer` object: the remaining time and whether it is running.
    Timer { remaining: f64, running: bool },
}

/// The per-call-site state map for stateful builtins. A new site defaults to
/// [`OpState::Uninit`] the first time it is touched.
#[derive(Debug, Clone, Default)]
pub struct StateStore(pub HashMap<CallSite, OpState>);

impl StateStore {
    /// An empty state store.
    pub fn new() -> StateStore {
        StateStore(HashMap::new())
    }

    /// A stable mutable slot for `site`, default-constructing [`OpState::Uninit`]
    /// on first access. The same `site` returns the same slot every tick, so a
    /// stateful operator accumulates correctly.
    pub fn entry(&mut self, site: CallSite) -> &mut OpState {
        self.0.entry(site).or_default()
    }

    /// The current state for `site`, if any has been recorded.
    pub fn get(&self, site: &CallSite) -> Option<&OpState> {
        self.0.get(site)
    }
}

/// Compose the static-storage key for a `static local` variable: its owning
/// function's canonical symbol path plus the variable name. Two functions each
/// declaring `static local x` therefore get independent slots.
fn static_key(fn_symbol: &str, var: &str) -> String {
    format!("{fn_symbol}\u{1f}{var}")
}

/// The runtime value store: channel/parameter values, function locals, and
/// `static local` persistence.
#[derive(Debug, Clone, Default)]
pub struct Env {
    /// Channel/parameter values by canonical path. Persists for the whole run.
    pub values: HashMap<String, Value>,
    /// Locals for the currently executing function. Cleared on
    /// [`Env::leave_function`].
    pub locals: HashMap<String, Value>,
    /// `static local` values, keyed by owning-function path + variable name.
    /// Persist across function entry/exit for the whole run.
    pub statics: HashMap<String, Value>,
    /// Scenario-fed values for Tier-3 IO calls, keyed by the call spelling
    /// `"Object.Method"` (e.g. `"CanComms.GetFloat"`). When a key is present the
    /// IO stub returns it instead of a documented default — this is how a
    /// scenario externally drives a hardware-backed builtin. Empty by default.
    pub io_overrides: HashMap<String, Value>,
    /// The current function frame's return-value slot — the `Out` object an M1
    /// user function assigns to (`Out = <expr>;`). A single slot per *active*
    /// frame: `userfn::call` saves the caller's slot, clears it, runs the callee
    /// (whose `Out =` statements write here), reads the result, then restores the
    /// caller's slot. `None` means the current function has not assigned `Out`.
    pub out: Option<Value>,
    /// Whether an unseeded *channel* read returns its type-correct external
    /// default instead of failing loud [`crate::error::EvalError::MissingInput`].
    ///
    /// `false` by default (the single-function / cone modes): the scenario must
    /// drive every input channel a function reads, and an unprovided input is a
    /// fail-loud error so a missing input is never silently a guessed value.
    ///
    /// The **whole-project runner** sets it `true`: there is no scenario driving
    /// the sensor/CAN inputs, so an unseeded channel read (a hardware input, a
    /// table output the auto-`Lookup` would compute, a state channel before its
    /// writer's first run) falls back to its determinate startup default, flagged
    /// externally driven — the channel-side analogue of the Tier-3 IO stubs. This
    /// is what lets a whole-project run complete offline without a calibration or
    /// a log. It propagates to inline user-function callees, which share this env.
    pub default_unseeded_channels: bool,
}

impl Env {
    /// An empty environment.
    pub fn new() -> Env {
        Env::default()
    }

    /// The current value at a canonical path (channel/parameter), if set.
    pub fn get(&self, path: &str) -> Option<&Value> {
        self.values.get(path)
    }

    /// Set a channel/parameter value by canonical path. Spaces in the path are
    /// part of the key; nothing is split on whitespace.
    pub fn set(&mut self, path: impl Into<String>, value: Value) {
        self.values.insert(path.into(), value);
    }

    /// The current value of a function-local variable, if set.
    pub fn get_local(&self, name: &str) -> Option<&Value> {
        self.locals.get(name)
    }

    /// Set a function-local variable.
    pub fn set_local(&mut self, name: impl Into<String>, value: Value) {
        self.locals.insert(name.into(), value);
    }

    /// The current value of a `static local`, addressed by its owning function
    /// path and variable name.
    pub fn get_static(&self, fn_symbol: &str, var: &str) -> Option<&Value> {
        self.statics.get(&static_key(fn_symbol, var))
    }

    /// Set a `static local` value for the given owning function + variable name.
    pub fn set_static(&mut self, fn_symbol: &str, var: &str, value: Value) {
        self.statics.insert(static_key(fn_symbol, var), value);
    }

    /// A scenario-fed override for a Tier-3 IO call `"Object.Method"`, if set.
    pub fn io_override(&self, call: &str) -> Option<&Value> {
        self.io_overrides.get(call)
    }

    /// Seed a scenario value for a Tier-3 IO call `"Object.Method"`.
    pub fn set_io_override(&mut self, call: impl Into<String>, value: Value) {
        self.io_overrides.insert(call.into(), value);
    }

    /// Write the current frame's `Out` return slot — the value an `Out = <expr>;`
    /// statement assigns inside a user-function body.
    pub fn set_out(&mut self, value: Value) {
        self.out = Some(value);
    }

    /// The current frame's `Out` return value, if the body has assigned one.
    pub fn get_out(&self) -> Option<&Value> {
        self.out.as_ref()
    }

    /// Clear the current frame's `Out` slot (entering a fresh callee frame so its
    /// own `Out =` writes start from "unassigned", or after reading the return).
    /// Returns the previous slot so a caller can save and restore it across a
    /// nested call.
    pub fn clear_out(&mut self) -> Option<Value> {
        self.out.take()
    }

    /// Begin executing a function: start with a fresh, empty local scope. Statics
    /// and channel values are untouched.
    pub fn enter_function(&mut self) {
        self.locals.clear();
    }

    /// Finish executing a function: discard its locals. Statics persist (that is
    /// the entire point of `static local`), and channel values persist.
    pub fn leave_function(&mut self) {
        self.locals.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_roundtrip_on_spaced_path() {
        let mut env = Env::new();
        // M1 identifiers may contain spaces — the whole path is one key.
        env.set("Root.A.Cooling Fan.Output", Value::Float(1.5));
        assert_eq!(
            env.get("Root.A.Cooling Fan.Output"),
            Some(&Value::Float(1.5))
        );
        // A different spelling (split on space) must NOT collide.
        assert_eq!(env.get("Root.A.Cooling"), None);
    }

    #[test]
    fn locals_clear_on_leave_function_but_statics_persist() {
        let mut env = Env::new();
        env.enter_function();
        env.set_local("scaled", Value::Int(3));
        env.set_static("Root.Demo.Update", "accum", Value::Float(10.0));
        assert_eq!(env.get_local("scaled"), Some(&Value::Int(3)));

        env.leave_function();
        // Locals are gone.
        assert_eq!(env.get_local("scaled"), None);
        // The static survives.
        assert_eq!(
            env.get_static("Root.Demo.Update", "accum"),
            Some(&Value::Float(10.0))
        );

        // Re-entering keeps the static available, fresh locals.
        env.enter_function();
        assert_eq!(env.get_local("scaled"), None);
        assert_eq!(
            env.get_static("Root.Demo.Update", "accum"),
            Some(&Value::Float(10.0))
        );
    }

    #[test]
    fn out_slot_set_get_clear_roundtrip() {
        let mut env = Env::new();
        // Unassigned by default.
        assert_eq!(env.get_out(), None);
        // A body assigns Out; the slot holds it.
        env.set_out(Value::Float(6.0));
        assert_eq!(env.get_out(), Some(&Value::Float(6.0)));
        // Clearing returns the prior value and empties the slot.
        assert_eq!(env.clear_out(), Some(Value::Float(6.0)));
        assert_eq!(env.get_out(), None);
        // Clearing an empty slot returns None.
        assert_eq!(env.clear_out(), None);
    }

    #[test]
    fn out_slot_is_independent_of_locals_and_statics() {
        let mut env = Env::new();
        env.set_local("y", Value::Int(7));
        env.set_out(Value::Int(99));
        // Leaving a function clears locals but does NOT touch the out slot — the
        // caller (`userfn::call`) owns save/restore of the out slot explicitly.
        env.leave_function();
        assert_eq!(env.get_local("y"), None);
        assert_eq!(env.get_out(), Some(&Value::Int(99)));
    }

    #[test]
    fn statics_of_different_functions_do_not_collide() {
        let mut env = Env::new();
        env.set_static("Root.Demo.Update", "x", Value::Int(1));
        env.set_static("Root.Other.Update", "x", Value::Int(2));
        assert_eq!(
            env.get_static("Root.Demo.Update", "x"),
            Some(&Value::Int(1))
        );
        assert_eq!(
            env.get_static("Root.Other.Update", "x"),
            Some(&Value::Int(2))
        );
    }

    #[test]
    fn statestore_entry_is_stable_per_callsite() {
        let mut store = StateStore::new();
        let site = CallSite::new("Demo.Update.m1scr", 42);

        // First access default-constructs Uninit.
        assert_eq!(*store.entry(site.clone()), OpState::Uninit);
        // Mutating through the slot persists for the same site.
        *store.entry(site.clone()) = OpState::Uninit; // (only variant in M3)
        assert!(store.get(&site).is_some());

        // A different site is an independent slot.
        let other = CallSite::new("Demo.Update.m1scr", 99);
        assert!(store.get(&other).is_none());
        let _ = store.entry(other.clone());
        assert_eq!(store.0.len(), 2);
    }

    #[test]
    fn callsite_accessors() {
        let site = CallSite::new("Demo.Update.m1scr", 7);
        assert_eq!(site.script(), "Demo.Update.m1scr");
        assert_eq!(site.offset(), 7);
        // Equality keys on both components.
        assert_eq!(site, CallSite("Demo.Update.m1scr".into(), 7));
        assert_ne!(site, CallSite("Demo.Update.m1scr".into(), 8));
    }

    #[test]
    fn callsite_of_node_is_stable_across_evaluations() {
        use m1_core::{Field, Kind, parse};

        // A script with two distinct calls; locate each CallExpression node and
        // derive its CallSite from the script name + byte offset.
        let src = "a = Calculate.Max(1, 2);\nb = Calculate.Min(3, 4);\n";

        // Helper: find the nth CallExpression node (depth-first) in a fresh parse.
        fn nth_call(src: &str, n: usize) -> (usize, m1_core::Cst) {
            let cst = parse(src);
            let mut found = Vec::new();
            let mut stack: Vec<m1_core::Node> = vec![cst.root()];
            while let Some(node) = stack.pop() {
                if node.kind() == Kind::CallExpression {
                    found.push(node.byte_range().start);
                }
                // Push children; order does not matter, we sort offsets after.
                for child in node.children() {
                    stack.push(child);
                }
            }
            found.sort_unstable();
            (found[n], cst)
        }

        // The same parse evaluated twice yields the same call-site key.
        let cst1 = parse(src);
        let first_call = cst1
            .root()
            .children()
            .into_iter()
            .next()
            .and_then(|stmt| stmt.child_by_field(Field::Value))
            .expect("first assignment value is the call");
        let site_a = CallSite::of("Demo.Update.m1scr", &first_call);
        let site_b = CallSite::of("Demo.Update.m1scr", &first_call);
        assert_eq!(site_a, site_b, "same node -> same key");

        // The two distinct calls have distinct keys; a re-parse reproduces them.
        let (off0, _c0) = nth_call(src, 0);
        let (off1, _c1) = nth_call(src, 1);
        assert_ne!(off0, off1, "two calls live at different offsets");
        let (off0_again, _) = nth_call(src, 0);
        assert_eq!(off0, off0_again, "byte offset is deterministic per parse");
        assert_eq!(site_a.offset(), off0);
    }
}
