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
/// M3 provides only the empty [`OpState::Uninit`] slot so [`StateStore::entry`]
/// has something to default-construct; the M6 stateful-builtin milestone extends
/// this enum with one variant per operator family (filter capacitor, integral
/// accumulator, delay/debounce timer, `Change` previous-value, …). Keeping the
/// default variant means a freshly-keyed site starts uninitialised and the
/// operator seeds itself on first tick.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum OpState {
    /// No state has been recorded for this call site yet (first tick).
    #[default]
    Uninit,
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
    fn statics_of_different_functions_do_not_collide() {
        let mut env = Env::new();
        env.set_static("Root.Demo.Update", "x", Value::Int(1));
        env.set_static("Root.Other.Update", "x", Value::Int(2));
        assert_eq!(env.get_static("Root.Demo.Update", "x"), Some(&Value::Int(1)));
        assert_eq!(env.get_static("Root.Other.Update", "x"), Some(&Value::Int(2)));
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
}
