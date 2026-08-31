// SPDX-License-Identifier: GPL-3.0-or-later
//! m1-eval: a stepped evaluator for the MoTeC M1 scripting language.
pub mod error;
pub use error::EvalError;

pub mod value;
pub use value::{FixedPoint7dps, M1Scalar, M1ScalarKind, Value};

pub mod calib;
pub use calib::{AxisExtrapolation, CalAxis, CalAxisValues, CalTable, Calibration};

pub mod table;
pub use table::TableInput;

pub mod loader;
pub use loader::{Loaded, load};

pub mod triggers;
pub use triggers::{TriggerMap, TriggerStatus};

pub mod schedule;
pub use schedule::{
    ReadyTiePolicy, ScheduleDependency, ScheduleMaturity, SchedulePlan, SchedulePlanEntry,
    build_schedule_plan,
};

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
pub use trace::{
    CanEvent, CanTransferDirection, ScheduleExecution, ScheduleInputProvenance,
    ScheduleInputSource, SerialDirection, SerialEvent, Trace,
};

pub use m1_can::{CanDbcSource, CanFrameFormat, CanRuntimeModel, runtime_model_loaded};

mod virtual_can;
mod virtual_serial;

pub mod expr;
pub use expr::{EvalCtx, eval, eval_at_time};

pub mod builtins;

pub mod stmt;
pub use stmt::{exec, exec_at_time, exec_script, exec_script_at_time};

pub mod scenario;
pub use scenario::{
    CanRx, CanScenario, InitialValue, InputKind, InputSeries, IoSeries, RunMode, Scenario,
    SerialRx, SerialScenario,
};

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

pub mod conformance;
pub use conformance::{
    CONFORMANCE_SCHEMA_VERSION, ConformanceError, ConformanceFixture, ConformanceMismatch,
    ConformanceOptions, ConformanceReport, ExpectedChannelValue, ExpectedScheduleExecution,
    FixtureChannelValue, FixtureProvenance, FixtureRun, FixtureRunMode, FixtureStep, ProjectBundle,
    ProjectFileHash, ProvenanceKind, ValueTolerance, WireValue, run_conformance_fixture,
    run_conformance_suite,
};
