// SPDX-License-Identifier: GPL-3.0-or-later
//! m1-eval: a stepped evaluator for the MoTeC M1 scripting language.
pub mod error;
pub use error::EvalError;

pub mod value;
pub use value::Value;

pub mod calib;
pub use calib::{CalTable, Calibration};

pub mod table;

pub mod loader;
pub use loader::{Loaded, load};

pub mod env;
pub use env::{CallSite, Env, OpState, StateStore};

pub mod ident;
pub use ident::{Target, classify};

pub mod trace;
pub use trace::Trace;

pub mod expr;
pub use expr::{EvalCtx, eval};

pub mod builtins;

pub mod stmt;
pub use stmt::{exec, exec_script};
