// SPDX-License-Identifier: GPL-3.0-or-later
//! Typed boundary between script hardware calls and an external adapter.
//!
//! The evaluator resolves a receiver before it builds a [`HardwareCall`]. An
//! adapter therefore sees the canonical library object or project-object path,
//! the source spelling, the exact [`CallSite`], evaluated
//! arguments, and deterministic evaluator time. It never has to parse a dotted
//! string or consult wall-clock time.
//!
//! ```no_run
//! use m1_eval::{AdapterReply, EvalError, HardwareAdapter, HardwareCall, Value};
//!
//! struct BoardMetadata;
//!
//! impl HardwareAdapter for BoardMetadata {
//!     fn call(&mut self, call: &HardwareCall) -> Result<AdapterReply, EvalError> {
//!         match call.canonical_name().as_str() {
//!             "System.FlashSize" => Ok(Value::m1_unsigned(8 * 1024 * 1024).into()),
//!             "System.FlashFree" => Ok(Value::m1_unsigned(2 * 1024 * 1024).into()),
//!             _ => Ok(AdapterReply::Unhandled),
//!         }
//!     }
//! }
//! ```

use crate::{CallSite, EvalError, Value};

/// The receiver identity after project and intrinsic name resolution.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResolvedReceiver {
    /// A firmware library object, such as `System` or `CanComms`.
    Library { object: String },
    /// A project-owned object at its canonical `Root.*` path.
    Project { path: String },
    /// A project-style receiver that the loaded symbol model could not resolve.
    ///
    /// Existing projects contain package-generated hardware receivers which are
    /// absent from incomplete project exports. Keeping this state explicit lets
    /// an adapter handle them without pretending the receiver resolved.
    Unresolved { spelling: String },
}

impl ResolvedReceiver {
    /// The stable receiver name used in canonical hardware-call keys.
    pub fn name(&self) -> &str {
        match self {
            ResolvedReceiver::Library { object } => object,
            ResolvedReceiver::Project { path } => path,
            ResolvedReceiver::Unresolved { spelling } => spelling,
        }
    }

    /// Whether project resolution found a concrete receiver.
    pub fn is_resolved(&self) -> bool {
        !matches!(self, ResolvedReceiver::Unresolved { .. })
    }
}

/// Which evaluator phase produced a hardware call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EvalPhase {
    /// The once-only startup pass before the periodic grid opens.
    Startup,
    /// A periodic base-grid tick.
    Periodic,
}

/// Deterministic time context for one script execution.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EvalTime {
    /// Startup or periodic execution.
    pub phase: EvalPhase,
    /// Zero-based base-grid tick. Startup uses tick zero.
    pub base_tick: u64,
    /// Seconds from the start of the run. Startup and periodic tick zero are 0.
    pub elapsed_s: f64,
    /// Period of the evaluator's base grid in seconds.
    pub base_period_s: f64,
    /// Period of the function currently executing. A slower scheduled function
    /// can have a larger step than the base period.
    pub step_s: f64,
}

impl EvalTime {
    /// Time context for a normal periodic execution.
    pub fn periodic(base_tick: u64, elapsed_s: f64, base_period_s: f64, step_s: f64) -> EvalTime {
        EvalTime {
            phase: EvalPhase::Periodic,
            base_tick,
            elapsed_s,
            base_period_s,
            step_s,
        }
    }

    /// Time context for the once-only startup pass.
    pub fn startup(base_period_s: f64) -> EvalTime {
        EvalTime {
            phase: EvalPhase::Startup,
            base_tick: 0,
            elapsed_s: 0.0,
            base_period_s,
            step_s: base_period_s,
        }
    }

    /// A compact context for direct evaluator users and unit tests.
    ///
    /// It represents periodic tick zero on a grid whose base and function
    /// periods are both `step_s`.
    pub fn at_start(step_s: f64) -> EvalTime {
        EvalTime::periodic(0, 0.0, step_s, step_s)
    }
}

/// One fully-resolved hardware call offered to a [`HardwareAdapter`].
#[derive(Debug, Clone, PartialEq)]
pub struct HardwareCall {
    /// Canonical or explicitly unresolved receiver identity.
    pub receiver: ResolvedReceiver,
    /// Receiver text as written in the script. This preserves `Library.*` and
    /// relative spellings for diagnostics and compatibility scenario keys.
    pub source_receiver: String,
    /// Method name without its receiver.
    pub method: String,
    /// Exact script and byte offset of this call occurrence.
    pub site: CallSite,
    /// Arguments after expression evaluation, in source order.
    pub arguments: Vec<Value>,
    /// Deterministic evaluator time for this execution.
    pub time: EvalTime,
}

impl HardwareCall {
    /// Canonical `receiver.method` key.
    pub fn canonical_name(&self) -> String {
        format!("{}.{}", self.receiver.name(), self.method)
    }

    /// Source-spelled `receiver.method` key.
    pub fn source_name(&self) -> String {
        format!("{}.{}", self.source_receiver, self.method)
    }
}

/// An adapter either supplies the call's value or declines it so the evaluator
/// can continue through its deterministic model and documented fallback rules.
#[derive(Debug, Clone, PartialEq)]
pub enum AdapterReply {
    /// The adapter did not handle this receiver and method.
    Unhandled,
    /// The adapter supplied the call result. The evaluator restores the
    /// method's declared M1 return family before script execution.
    Value(Value),
}

impl From<Value> for AdapterReply {
    fn from(value: Value) -> Self {
        AdapterReply::Value(value)
    }
}

/// External implementation of hardware-backed calls.
pub trait HardwareAdapter {
    /// Handle one call or return [`AdapterReply::Unhandled`] to let evaluation
    /// continue through the built-in offline model and fallback rules.
    fn call(&mut self, call: &HardwareCall) -> Result<AdapterReply, EvalError>;
}

/// How a hardware call obtained its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HardwareValueSource {
    /// A scenario entry matched the exact call site.
    ScenarioExact,
    /// A scenario entry matched every site of one call spelling.
    ScenarioWildcard,
    /// An external [`HardwareAdapter`] supplied the value.
    Adapter,
    /// The evaluator's deterministic `System` model supplied the value.
    SystemModel,
    /// A documented type-correct offline stub supplied the value.
    GenericStub,
}

impl HardwareValueSource {
    /// Stable lowercase spelling used in JSON trace output.
    pub fn as_str(self) -> &'static str {
        match self {
            HardwareValueSource::ScenarioExact => "scenario-exact",
            HardwareValueSource::ScenarioWildcard => "scenario-wildcard",
            HardwareValueSource::Adapter => "adapter",
            HardwareValueSource::SystemModel => "system-model",
            HardwareValueSource::GenericStub => "generic-stub",
        }
    }

    /// Whether this result came from outside evaluator computation.
    pub fn is_external(self) -> bool {
        !matches!(self, HardwareValueSource::SystemModel)
    }
}

/// De-duplicated trace record for one call site and result source.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HardwareProvenance {
    /// Resolved receiver offered to the adapter.
    pub receiver: ResolvedReceiver,
    /// Source-spelled call name.
    pub source_call: String,
    /// Method name.
    pub method: String,
    /// Exact call occurrence.
    pub site: CallSite,
    /// Rule which supplied the value.
    pub source: HardwareValueSource,
}

impl HardwareProvenance {
    /// Build a trace record from a hardware call and its selected route.
    pub(crate) fn new(call: &HardwareCall, source: HardwareValueSource) -> Self {
        HardwareProvenance {
            receiver: call.receiver.clone(),
            source_call: call.source_name(),
            method: call.method.clone(),
            site: call.site.clone(),
            source,
        }
    }

    /// Canonical `receiver.method` name.
    pub fn canonical_call(&self) -> String {
        format!("{}.{}", self.receiver.name(), self.method)
    }
}
