// SPDX-License-Identifier: GPL-3.0-or-later
//! m1-eval: a stepped evaluator for the MoTeC M1 scripting language.
pub mod error;
pub use error::EvalError;

pub mod value;
pub use value::{FixedPoint7dps, M1Scalar, M1ScalarKind, Value};

pub mod calib;
pub use calib::{CalTable, Calibration};

pub mod table;

pub mod loader;
pub use loader::{Loaded, load};

pub mod triggers;
pub use triggers::{TriggerMap, TriggerStatus};

pub mod env;
pub use env::{CallSite, Env, OpState, StateStore};

pub mod hardware;
pub use hardware::{
    AdapterReply, EvalPhase, EvalTime, HardwareAdapter, HardwareCall, HardwareProvenance,
    HardwareValueSource, ResolvedReceiver,
};

pub mod ident;
pub use ident::{Target, classify};

pub mod trace;
pub use trace::{SerialDirection, SerialEvent, Trace};

mod virtual_serial;

pub mod expr;
pub use expr::{EvalCtx, eval};

pub mod builtins;

pub mod stmt;
pub use stmt::{exec, exec_script};

pub mod scenario;
pub use scenario::{InputKind, InputSeries, IoSeries, RunMode, Scenario, SerialRx, SerialScenario};

pub mod log;
pub use log::{Log, LogMeta};

pub mod counterfactual;
pub use counterfactual::Override;

pub mod diff;
pub use diff::{ChannelDiff, Counterfactual, Diff};

pub mod summary;
pub use summary::{IoSets, io_sets};

pub mod runner;
pub use runner::{
    CounterfactualCfg, run_counterfactual, run_counterfactual_with_adapter, run_with_adapter,
};

pub mod coverage;
pub use coverage::{CoverageItem, CoverageReport, ItemKind, UnresolvedTrigger};

pub mod engine;
pub use engine::Engine;
