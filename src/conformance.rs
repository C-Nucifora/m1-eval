// SPDX-License-Identifier: GPL-3.0-or-later
//! Typed golden-vector fixtures for M1 Sim conformance evidence.
//!
//! A fixture binds its expected values to an exact project bundle through
//! SHA-256 hashes. It records an explicit tick grid, one-time initial channel
//! state, zero-order-held inputs, expected outputs, tolerances, and provenance.
//! Running a fixture always loads a new project and starts a new evaluator run,
//! so stateful operators cannot leak state between fixtures in a suite.

use crate::error::EvalError;
use crate::loader::{Loaded, load};
use crate::runner;
use crate::scenario::{InitialValue, InputKind, InputSeries, RunMode, Scenario};
use crate::trace::Trace;
use crate::value::{FixedPoint7dps, M1Scalar, Value};
use m1_typecheck::Project;
use m1_typecheck::symbols::SymbolKind;
use m1_typecheck::types::ValueType;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// The fixture schema version understood by this release.
pub const CONFORMANCE_SCHEMA_VERSION: u32 = 1;

/// A parsed conformance fixture and the path it was loaded from.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConformanceFixture {
    /// Schema version. It must equal [`CONFORMANCE_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Short name printed in reports and mismatch messages.
    pub name: String,
    /// Exact project files used for the captured calculation.
    pub project: ProjectBundle,
    /// Where the expected values came from.
    pub provenance: FixtureProvenance,
    /// Evaluator mode and target used to replay the capture.
    pub run: FixtureRun,
    /// Tick-grid rate. Every step must be at `index / calculation_rate_hz`.
    pub calculation_rate_hz: f64,
    /// Values placed in the channel store once, before startup and tick zero.
    #[serde(default)]
    pub initial_state: Vec<FixtureChannelValue>,
    /// One record per tick, starting at `time_s = 0`.
    pub steps: Vec<FixtureStep>,
    /// Absolute path of the parsed document. This is not part of the wire data.
    #[serde(skip)]
    source_path: PathBuf,
}

/// Paths and hashes for every evaluator-consumed project file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectBundle {
    /// Bundle directory, relative to the fixture file unless absolute.
    pub root: PathBuf,
    /// Project descriptor path, relative to [`ProjectBundle::root`].
    pub project: PathBuf,
    /// Optional calibration path, relative to [`ProjectBundle::root`].
    #[serde(default)]
    pub config: Option<PathBuf>,
    /// Exact manifest of the descriptor, calibration, and discovered scripts.
    pub files: Vec<ProjectFileHash>,
}

/// One SHA-256 entry in a project bundle manifest.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectFileHash {
    /// File path relative to the bundle root.
    pub path: PathBuf,
    /// Lowercase hexadecimal SHA-256 digest.
    pub sha256: String,
}

/// How a fixture's expected values were produced.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureProvenance {
    /// Whether this is synthetic data or a real M1 Sim capture.
    pub kind: ProvenanceKind,
    /// Concrete origin, such as a test-project name or simulator session ID.
    pub source: String,
    /// Capture procedure or a link to the procedure used.
    pub procedure: String,
    /// M1 Build or M1 Sim version for a genuine capture.
    #[serde(default)]
    pub tool_version: Option<String>,
    /// UTC capture time for a genuine capture.
    #[serde(default)]
    pub captured_at_utc: Option<String>,
    /// Optional free-form facts needed to reproduce the capture.
    #[serde(default)]
    pub notes: Option<String>,
}

/// Evidence classification carried by each fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProvenanceKind {
    /// Hand-derived expected values used to test the runner itself.
    Synthetic,
    /// Expected values checked against a named independent implementation or
    /// published standard, but not captured from M1 Sim.
    Independent,
    /// Values captured from M1 Sim.
    M1Sim,
}

impl fmt::Display for ProvenanceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Synthetic => formatter.write_str("synthetic"),
            Self::Independent => formatter.write_str("independent"),
            Self::M1Sim => formatter.write_str("m1-sim"),
        }
    }
}

/// Evaluator mode recorded by a conformance fixture.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureRun {
    /// Runner mode.
    pub mode: FixtureRunMode,
    /// Function name or cone channel. Whole-project mode must omit it.
    #[serde(default)]
    pub target: Option<String>,
}

/// Supported conformance-run modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FixtureRunMode {
    /// Run one function on every fixture tick.
    Function,
    /// Run the dependency cone for one output channel.
    Cone,
    /// Run the project's periodic schedule on the fixture base grid.
    WholeProject,
}

/// One typed channel value used as initial state or a step input.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureChannelValue {
    /// Canonical project channel path.
    pub channel: String,
    /// Typed value on the fixture wire.
    pub value: WireValue,
}

/// One captured tick.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureStep {
    /// Tick time in seconds. Step `i` must use `i / calculation_rate_hz`.
    pub time_s: f64,
    /// Input changes applied at this tick and held until the next change.
    #[serde(default)]
    pub inputs: Vec<FixtureChannelValue>,
    /// Expected channel values after this tick executes.
    pub expected: Vec<ExpectedChannelValue>,
}

/// One expected output and its comparison rule.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedChannelValue {
    /// Canonical trace channel path.
    pub channel: String,
    /// Typed expected value.
    pub value: WireValue,
    /// Required for floating-point and fixed-point values, forbidden otherwise.
    #[serde(default)]
    pub tolerance: Option<ValueTolerance>,
}

/// A value with its M1 runtime family written explicitly.
///
/// Floating-point values use strings so JSON can carry `NaN`, `Infinity`, and
/// `-Infinity`, and so a capture does not pass through a host-width JSON number.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum WireValue {
    /// M1 Boolean.
    Boolean { value: bool },
    /// M1 binary32, written as a decimal string or an explicit non-finite token.
    FloatingPoint { value: String },
    /// M1 signed 32-bit integer.
    Integer { value: i32 },
    /// M1 unsigned 32-bit integer.
    UnsignedInteger { value: u32 },
    /// M1 signed fixed-point storage, scaled by 10^-7.
    #[serde(rename = "fixed-point-7dps")]
    FixedPoint7dps { raw: i32 },
    /// Enum member with its declared type name.
    Enum {
        /// Project enum type name.
        enum_type: String,
        /// Declared member name.
        member: String,
    },
    /// M1 string.
    String { value: String },
}

/// Tolerance declaration for an approximate numeric family.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ValueTolerance {
    /// Binary32 comparison. At least one bound must be present.
    FloatingPoint {
        /// Absolute error bound, as a finite non-negative decimal string.
        #[serde(default)]
        absolute: Option<String>,
        /// Relative error bound, as a finite non-negative decimal string.
        #[serde(default)]
        relative: Option<String>,
    },
    /// Fixed-point comparison in exact raw storage units.
    #[serde(rename = "fixed-point-7dps")]
    FixedPoint7dps {
        /// Maximum absolute difference between the two signed raw values.
        raw: u32,
    },
}

/// Successful result for one fixture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceReport {
    /// Fixture name.
    pub name: String,
    /// Fixture file path.
    pub path: PathBuf,
    /// Evidence classification declared by the fixture.
    pub provenance: ProvenanceKind,
    /// Number of tick records checked.
    pub steps_checked: usize,
    /// Number of expected channel values checked.
    pub assertions_checked: usize,
}

/// Options for a multi-fixture conformance run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConformanceOptions {
    /// Fail unless at least one passing fixture declares M1 Sim provenance.
    pub require_m1_sim_capture: bool,
}

/// The first output mismatch found while walking a fixture in file order.
#[derive(Debug, Clone, PartialEq)]
pub struct ConformanceMismatch {
    /// Fixture name.
    pub fixture: String,
    /// Zero-based step index.
    pub step: usize,
    /// Captured tick time.
    pub time_s: f64,
    /// Expected channel path.
    pub channel: String,
    /// Human-readable expected value and tolerance.
    pub expected: String,
    /// Actual runtime value, or `None` if no aligned trace value existed.
    pub actual: Option<Value>,
    /// Specific comparison failure.
    pub detail: String,
}

impl fmt::Display for ConformanceMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "fixture {:?} step {} at t = {} s, channel {:?}: {}; expected {}, got {}",
            self.fixture,
            self.step,
            self.time_s,
            self.channel,
            self.detail,
            self.expected,
            self.actual
                .as_ref()
                .map(runtime_value_text)
                .unwrap_or_else(|| "no value".to_string())
        )
    }
}

/// Fixture parsing, integrity, evaluation, or comparison failure.
#[derive(Debug)]
pub enum ConformanceError {
    /// A fixture or project file could not be read.
    Io {
        /// File involved.
        path: PathBuf,
        /// Operating-system error.
        source: std::io::Error,
    },
    /// TOML or JSON did not match the fixture schema.
    Parse {
        /// Fixture file involved.
        path: PathBuf,
        /// Parser detail.
        detail: String,
    },
    /// The document is syntactically valid but violates a fixture invariant.
    InvalidFixture {
        /// Fixture name or path.
        fixture: String,
        /// Failed invariant.
        detail: String,
    },
    /// A project file did not match its declared SHA-256 digest.
    HashMismatch {
        /// Project file involved.
        path: PathBuf,
        /// Digest in the fixture.
        expected: String,
        /// Digest calculated by the runner.
        actual: String,
    },
    /// Project loading or script evaluation failed.
    Evaluation {
        /// Fixture name.
        fixture: String,
        /// Evaluator error.
        source: EvalError,
    },
    /// First expected-output mismatch.
    Mismatch(Box<ConformanceMismatch>),
    /// A suite requested real capture evidence but contained only synthetic data.
    MissingM1SimCapture,
}

impl fmt::Display for ConformanceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::Parse { path, detail } => {
                write!(
                    formatter,
                    "{}: conformance fixture parse error: {detail}",
                    path.display()
                )
            }
            Self::InvalidFixture { fixture, detail } => {
                write!(formatter, "fixture {fixture:?}: {detail}")
            }
            Self::HashMismatch {
                path,
                expected,
                actual,
            } => write!(
                formatter,
                "{}: SHA-256 mismatch, fixture has {expected}, file has {actual}",
                path.display()
            ),
            Self::Evaluation { fixture, source } => {
                write!(formatter, "fixture {fixture:?}: {source}")
            }
            Self::Mismatch(mismatch) => mismatch.fmt(formatter),
            Self::MissingM1SimCapture => formatter
                .write_str("conformance suite passed, but no fixture declared `m1-sim` provenance"),
        }
    }
}

impl std::error::Error for ConformanceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Evaluation { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl ConformanceFixture {
    /// Read a TOML or JSON fixture. The extension selects the parser.
    pub fn from_path(path: &Path) -> Result<Self, ConformanceError> {
        let source_path = fs::canonicalize(path).map_err(|source| ConformanceError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let body = fs::read_to_string(&source_path).map_err(|source| ConformanceError::Io {
            path: source_path.clone(),
            source,
        })?;
        let extension = source_path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        let mut fixture: Self = if extension.eq_ignore_ascii_case("json") {
            serde_json::from_str(&body).map_err(|error| ConformanceError::Parse {
                path: source_path.clone(),
                detail: error.to_string(),
            })?
        } else if extension.eq_ignore_ascii_case("toml") {
            toml::from_str(&body).map_err(|error| ConformanceError::Parse {
                path: source_path.clone(),
                detail: error.to_string(),
            })?
        } else {
            return Err(ConformanceError::Parse {
                path: source_path,
                detail: "expected a `.toml` or `.json` fixture".to_string(),
            });
        };
        fixture.source_path = source_path;
        fixture.validate_document()?;
        Ok(fixture)
    }

    fn invalid(&self, detail: impl Into<String>) -> ConformanceError {
        ConformanceError::InvalidFixture {
            fixture: if self.name.trim().is_empty() {
                self.source_path.display().to_string()
            } else {
                self.name.clone()
            },
            detail: detail.into(),
        }
    }

    fn validate_document(&self) -> Result<(), ConformanceError> {
        if self.schema_version != CONFORMANCE_SCHEMA_VERSION {
            return Err(self.invalid(format!(
                "unsupported schema_version {}, expected {}",
                self.schema_version, CONFORMANCE_SCHEMA_VERSION
            )));
        }
        if self.name.trim().is_empty() {
            return Err(self.invalid("`name` must not be empty"));
        }
        if !self.calculation_rate_hz.is_finite() || self.calculation_rate_hz <= 0.0 {
            return Err(self.invalid(format!(
                "calculation_rate_hz must be finite and positive, got {}",
                self.calculation_rate_hz
            )));
        }
        match self.run.mode {
            FixtureRunMode::Function | FixtureRunMode::Cone => {
                if self
                    .run
                    .target
                    .as_deref()
                    .is_none_or(|target| target.trim().is_empty())
                {
                    return Err(self.invalid("function and cone modes require a non-empty target"));
                }
            }
            FixtureRunMode::WholeProject if self.run.target.is_some() => {
                return Err(self.invalid("whole-project mode must not declare a target"));
            }
            FixtureRunMode::WholeProject => {}
        }
        if self.provenance.source.trim().is_empty() || self.provenance.procedure.trim().is_empty() {
            return Err(self.invalid("provenance source and procedure must not be empty"));
        }
        if self.provenance.kind == ProvenanceKind::M1Sim {
            for (name, value) in [
                ("tool_version", self.provenance.tool_version.as_deref()),
                (
                    "captured_at_utc",
                    self.provenance.captured_at_utc.as_deref(),
                ),
            ] {
                if value.is_none_or(|text| text.trim().is_empty()) {
                    return Err(
                        self.invalid(format!("m1-sim provenance requires a non-empty {name}"))
                    );
                }
            }
        }
        if self.steps.is_empty() {
            return Err(self.invalid("at least one step is required"));
        }

        let mut initial_channels = BTreeSet::new();
        for initial in &self.initial_state {
            validate_channel_name(self, &initial.channel, "initial-state")?;
            if !initial_channels.insert(initial.channel.as_str()) {
                return Err(self.invalid(format!(
                    "initial state declares channel {:?} more than once",
                    initial.channel
                )));
            }
            initial.value.validate(self)?;
        }

        let mut first_input_step: BTreeMap<&str, usize> = BTreeMap::new();
        let mut output_channels: Option<BTreeSet<&str>> = None;
        for (index, step) in self.steps.iter().enumerate() {
            let expected_time = index as f64 / self.calculation_rate_hz;
            if !step.time_s.is_finite()
                || (step.time_s - expected_time).abs() > 1e-12_f64.max(expected_time.abs() * 1e-12)
            {
                return Err(self.invalid(format!(
                    "step {index} time_s {} is off the {} Hz grid; expected {expected_time}",
                    step.time_s, self.calculation_rate_hz
                )));
            }
            if step.expected.is_empty() {
                return Err(self.invalid(format!(
                    "step {index} must declare at least one expected output"
                )));
            }

            let mut input_channels = BTreeSet::new();
            for input in &step.inputs {
                validate_channel_name(self, &input.channel, "input")?;
                if !input_channels.insert(input.channel.as_str()) {
                    return Err(self.invalid(format!(
                        "step {index} declares input channel {:?} more than once",
                        input.channel
                    )));
                }
                first_input_step.entry(&input.channel).or_insert(index);
                input.value.validate(self)?;
            }

            let mut expected_channels = BTreeSet::new();
            for expected in &step.expected {
                validate_channel_name(self, &expected.channel, "expected output")?;
                if !expected_channels.insert(expected.channel.as_str()) {
                    return Err(self.invalid(format!(
                        "step {index} declares expected channel {:?} more than once",
                        expected.channel
                    )));
                }
                expected.value.validate(self)?;
                validate_tolerance(self, index, expected)?;
            }
            if let Some(first) = &output_channels {
                if &expected_channels != first {
                    return Err(self.invalid(format!(
                        "step {index} expected-channel set differs from step 0"
                    )));
                }
            } else {
                output_channels = Some(expected_channels);
            }
        }
        for (&channel, &first_step) in &first_input_step {
            if first_step != 0 && !initial_channels.contains(channel) {
                return Err(self.invalid(format!(
                    "input channel {channel:?} first changes at step {first_step}; seed it in initial_state or step 0"
                )));
            }
        }
        if let Some(outputs) = output_channels {
            for channel in first_input_step.keys() {
                if outputs.contains(channel) {
                    return Err(self.invalid(format!(
                        "expected channel {channel:?} is also a fixture input; expected outputs must be disjoint from externally driven inputs"
                    )));
                }
            }
        }
        Ok(())
    }

    fn resolved_mode(&self) -> RunMode {
        match self.run.mode {
            FixtureRunMode::Function => {
                RunMode::Function(self.run.target.clone().expect("target validated"))
            }
            FixtureRunMode::Cone => {
                RunMode::Cone(self.run.target.clone().expect("target validated"))
            }
            FixtureRunMode::WholeProject => RunMode::WholeProject,
        }
    }
}

impl WireValue {
    fn validate(&self, fixture: &ConformanceFixture) -> Result<(), ConformanceError> {
        match self {
            Self::FloatingPoint { value } => parse_wire_float(value)
                .map(|_| ())
                .map_err(|detail| fixture.invalid(detail)),
            Self::Enum { enum_type, member }
                if enum_type.trim().is_empty() || member.trim().is_empty() =>
            {
                Err(fixture.invalid("enum type and member names must not be empty"))
            }
            _ => Ok(()),
        }
    }

    fn to_value(
        &self,
        fixture: &ConformanceFixture,
        project: &Project,
    ) -> Result<Value, ConformanceError> {
        match self {
            Self::Boolean { value } => Ok(Value::Bool(*value)),
            Self::FloatingPoint { value } => parse_wire_float(value)
                .map(Value::m1_float)
                .map_err(|detail| fixture.invalid(detail)),
            Self::Integer { value } => Ok(Value::m1_integer(*value)),
            Self::UnsignedInteger { value } => Ok(Value::m1_unsigned(*value)),
            Self::FixedPoint7dps { raw } => Ok(Value::M1(M1Scalar::FixedPoint7dps(
                FixedPoint7dps::from_raw(*raw),
            ))),
            Self::Enum { enum_type, member } => {
                let id = project.symbols().enum_by_name(enum_type).ok_or_else(|| {
                    fixture.invalid(format!("project has no enum type {enum_type:?}"))
                })?;
                let declared = project
                    .symbols()
                    .enum_type(id)
                    .members
                    .iter()
                    .any(|(name, _)| name == member);
                if !declared {
                    return Err(fixture
                        .invalid(format!("enum type {enum_type:?} has no member {member:?}")));
                }
                Ok(Value::Enum {
                    id,
                    member: member.clone(),
                })
            }
            Self::String { value } => Ok(Value::Str(value.clone())),
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Boolean { value } => format!("boolean({value})"),
            Self::FloatingPoint { value } => format!("floating-point({value})"),
            Self::Integer { value } => format!("integer({value})"),
            Self::UnsignedInteger { value } => format!("unsigned-integer({value})"),
            Self::FixedPoint7dps { raw } => format!("fixed-point-7dps(raw={raw})"),
            Self::Enum { enum_type, member } => format!("enum({enum_type}.{member})"),
            Self::String { value } => format!("string({value:?})"),
        }
    }
}

/// Run one fixture from disk.
pub fn run_conformance_fixture(path: &Path) -> Result<ConformanceReport, ConformanceError> {
    let fixture = ConformanceFixture::from_path(path)?;
    run_parsed_fixture(&fixture)
}

/// Run fixtures in the supplied order, stopping at the first failure.
///
/// Each path goes through [`run_conformance_fixture`], which reloads the project
/// and creates a new evaluator environment and state store. Repeating a fixture
/// in the same suite is therefore a useful state-reset check.
pub fn run_conformance_suite(
    paths: &[PathBuf],
    options: ConformanceOptions,
) -> Result<Vec<ConformanceReport>, ConformanceError> {
    if paths.is_empty() {
        return Err(ConformanceError::InvalidFixture {
            fixture: "suite".to_string(),
            detail: "at least one fixture path is required".to_string(),
        });
    }
    let mut reports = Vec::with_capacity(paths.len());
    for path in paths {
        reports.push(run_conformance_fixture(path)?);
    }
    if options.require_m1_sim_capture
        && !reports
            .iter()
            .any(|report| report.provenance == ProvenanceKind::M1Sim)
    {
        return Err(ConformanceError::MissingM1SimCapture);
    }
    Ok(reports)
}

fn run_parsed_fixture(fixture: &ConformanceFixture) -> Result<ConformanceReport, ConformanceError> {
    let bundle = resolve_and_verify_bundle(fixture)?;
    let loaded = load(&bundle.project, bundle.config.as_deref()).map_err(|source| {
        ConformanceError::Evaluation {
            fixture: fixture.name.clone(),
            source,
        }
    })?;
    validate_project_bindings(fixture, &loaded.project)?;
    let scenario = fixture_scenario(fixture, &loaded)?;
    let trace = runner::run(&loaded, &scenario).map_err(|source| ConformanceError::Evaluation {
        fixture: fixture.name.clone(),
        source,
    })?;
    compare_trace(fixture, &loaded.project, &trace)?;

    Ok(ConformanceReport {
        name: fixture.name.clone(),
        path: fixture.source_path.clone(),
        provenance: fixture.provenance.kind,
        steps_checked: fixture.steps.len(),
        assertions_checked: fixture.steps.iter().map(|step| step.expected.len()).sum(),
    })
}

fn validate_project_bindings(
    fixture: &ConformanceFixture,
    project: &Project,
) -> Result<(), ConformanceError> {
    for initial in &fixture.initial_state {
        validate_project_value(
            fixture,
            project,
            "initial-state",
            &initial.channel,
            &initial.value,
        )?;
    }
    for (index, step) in fixture.steps.iter().enumerate() {
        for input in &step.inputs {
            validate_project_value(
                fixture,
                project,
                &format!("step {index} input"),
                &input.channel,
                &input.value,
            )?;
        }
        for expected in &step.expected {
            validate_project_value(
                fixture,
                project,
                &format!("step {index} expected output"),
                &expected.channel,
                &expected.value,
            )?;
        }
    }
    Ok(())
}

fn validate_project_value(
    fixture: &ConformanceFixture,
    project: &Project,
    role: &str,
    channel: &str,
    wire: &WireValue,
) -> Result<(), ConformanceError> {
    let Some(symbol) = project.symbols().get(channel) else {
        return Err(fixture.invalid(format!(
            "{role} channel {channel:?} is not a project symbol; use its canonical project path"
        )));
    };
    if symbol.kind != SymbolKind::Channel {
        return Err(fixture.invalid(format!(
            "{role} path {channel:?} resolves to {:?}, not a project Channel",
            symbol.kind
        )));
    }
    let value = wire.to_value(fixture, project)?;
    let declared = runtime_family(&value);
    if let Some(stored) = project_runtime_family(symbol.value_type, symbol.declared_type.as_deref())
        && declared != stored
    {
        return Err(fixture.invalid(format!(
            "{role} channel {channel:?} declares {}, but the project stores {}",
            declared.name(),
            stored.name()
        )));
    }
    let coerced =
        crate::expr::coerce_for_channel(channel, value.clone(), project).map_err(|source| {
            fixture.invalid(format!(
                "{role} channel {channel:?} has an incompatible typed value: {source}"
            ))
        })?;
    if declared != runtime_family(&coerced) {
        return Err(fixture.invalid(format!(
            "{role} channel {channel:?} declares {}, but the project stores {}",
            declared.name(),
            runtime_family(&coerced).name()
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeFamily {
    Boolean,
    FloatingPoint,
    Integer,
    UnsignedInteger,
    FixedPoint7dps,
    Enum(usize),
    String,
}

impl RuntimeFamily {
    fn name(self) -> &'static str {
        match self {
            Self::Boolean => "Boolean",
            Self::FloatingPoint => "FloatingPoint",
            Self::Integer => "Integer",
            Self::UnsignedInteger => "UnsignedInteger",
            Self::FixedPoint7dps => "FixedPoint7dps",
            Self::Enum(_) => "Enumeration",
            Self::String => "String",
        }
    }
}

fn runtime_family(value: &Value) -> RuntimeFamily {
    match value {
        Value::Bool(_) => RuntimeFamily::Boolean,
        Value::M1(scalar) => match scalar.kind() {
            crate::value::M1ScalarKind::FloatingPoint => RuntimeFamily::FloatingPoint,
            crate::value::M1ScalarKind::Integer => RuntimeFamily::Integer,
            crate::value::M1ScalarKind::UnsignedInteger => RuntimeFamily::UnsignedInteger,
            crate::value::M1ScalarKind::FixedPoint7dps => RuntimeFamily::FixedPoint7dps,
        },
        Value::Enum { id, .. } => RuntimeFamily::Enum(*id),
        Value::Str(_) => RuntimeFamily::String,
    }
}

fn project_runtime_family(
    value_type: ValueType,
    declared_type: Option<&str>,
) -> Option<RuntimeFamily> {
    if let Some(declared) = declared_type {
        let normalized = declared.to_ascii_lowercase().replace([' ', '_', '-'], "");
        let family = match normalized.as_str() {
            "bool" | "boolean" => Some(RuntimeFamily::Boolean),
            "f32" | "f64" | "float" | "floatingpoint" => Some(RuntimeFamily::FloatingPoint),
            "s8" | "s16" | "s32" | "s64" | "integer" => Some(RuntimeFamily::Integer),
            "u8" | "u16" | "u32" | "u64" | "unsigned" | "unsignedinteger" => {
                Some(RuntimeFamily::UnsignedInteger)
            }
            "fixedpoint7dps" | "fixed7dps" => Some(RuntimeFamily::FixedPoint7dps),
            "str" | "string" => Some(RuntimeFamily::String),
            _ => None,
        };
        if family.is_some() {
            return family;
        }
    }
    match value_type {
        ValueType::Boolean => Some(RuntimeFamily::Boolean),
        ValueType::Integer => Some(RuntimeFamily::Integer),
        ValueType::Unsigned => Some(RuntimeFamily::UnsignedInteger),
        ValueType::Float => Some(RuntimeFamily::FloatingPoint),
        ValueType::String => Some(RuntimeFamily::String),
        ValueType::Enum(id) => Some(RuntimeFamily::Enum(id)),
        ValueType::Unknown => None,
    }
}

struct ResolvedBundle {
    project: PathBuf,
    config: Option<PathBuf>,
}

fn resolve_and_verify_bundle(
    fixture: &ConformanceFixture,
) -> Result<ResolvedBundle, ConformanceError> {
    let fixture_dir = fixture
        .source_path
        .parent()
        .expect("a canonical fixture path has a parent");
    let root_path = if fixture.project.root.is_absolute() {
        fixture.project.root.clone()
    } else {
        fixture_dir.join(&fixture.project.root)
    };
    let root = fs::canonicalize(&root_path).map_err(|source| ConformanceError::Io {
        path: root_path,
        source,
    })?;
    if !root.is_dir() {
        return Err(fixture.invalid(format!(
            "project bundle root {} is not a directory",
            root.display()
        )));
    }

    validate_relative_path(fixture, &fixture.project.project, "project")?;
    if let Some(config) = &fixture.project.config {
        validate_relative_path(fixture, config, "config")?;
    }
    let project = resolve_bundle_file(fixture, &root, &fixture.project.project)?;
    let config = fixture
        .project
        .config
        .as_ref()
        .map(|path| resolve_bundle_file(fixture, &root, path))
        .transpose()?;

    let mut expected_paths = BTreeSet::new();
    expected_paths.insert(fixture.project.project.clone());
    if let Some(config) = &fixture.project.config {
        expected_paths.insert(config.clone());
    }
    let project_dir = fixture
        .project
        .project
        .parent()
        .unwrap_or_else(|| Path::new(""));
    collect_script_paths(fixture, &root, project_dir, &mut expected_paths)?;
    let mut script_basenames = BTreeMap::new();
    for path in expected_paths
        .iter()
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("m1scr"))
    {
        let basename = path
            .file_name()
            .expect("a normalized script path has a basename")
            .to_os_string();
        if let Some(first) = script_basenames.insert(basename, path) {
            return Err(fixture.invalid(format!(
                "project bundle contains duplicate script basename {:?}: {} and {}; evaluator loading would be filesystem-order dependent",
                path.file_name().expect("script path has a basename"),
                first.display(),
                path.display()
            )));
        }
    }

    let mut manifest = BTreeMap::new();
    for entry in &fixture.project.files {
        validate_relative_path(fixture, &entry.path, "manifest")?;
        if entry.sha256.len() != 64
            || !entry
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(fixture.invalid(format!(
                "manifest entry {} has an invalid SHA-256 digest",
                entry.path.display()
            )));
        }
        if manifest
            .insert(entry.path.clone(), entry.sha256.clone())
            .is_some()
        {
            return Err(fixture.invalid(format!(
                "manifest declares {} more than once",
                entry.path.display()
            )));
        }
    }
    let manifest_paths: BTreeSet<PathBuf> = manifest.keys().cloned().collect();
    if manifest_paths != expected_paths {
        let missing = expected_paths
            .difference(&manifest_paths)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        let extra = manifest_paths
            .difference(&expected_paths)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();
        return Err(fixture.invalid(format!(
            "project manifest does not match evaluator inputs; missing [{}], extra [{}]",
            missing.join(", "),
            extra.join(", ")
        )));
    }

    for (relative, declared_hash) in manifest {
        let path = resolve_bundle_file(fixture, &root, &relative)?;
        let bytes = fs::read(&path).map_err(|source| ConformanceError::Io {
            path: path.clone(),
            source,
        })?;
        let actual = format!("{:x}", Sha256::digest(bytes));
        if !actual.eq_ignore_ascii_case(&declared_hash) {
            return Err(ConformanceError::HashMismatch {
                path,
                expected: declared_hash,
                actual,
            });
        }
    }

    Ok(ResolvedBundle { project, config })
}

fn validate_relative_path(
    fixture: &ConformanceFixture,
    path: &Path,
    label: &str,
) -> Result<(), ConformanceError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(fixture.invalid(format!(
            "{label} path {:?} must be a normalized path relative to the project bundle root",
            path.display().to_string()
        )));
    }
    Ok(())
}

fn resolve_bundle_file(
    fixture: &ConformanceFixture,
    root: &Path,
    relative: &Path,
) -> Result<PathBuf, ConformanceError> {
    let mut joined = root.to_path_buf();
    let components = relative.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            unreachable!("bundle paths were validated before resolution");
        };
        joined.push(name);
        let metadata = fs::symlink_metadata(&joined).map_err(|source| ConformanceError::Io {
            path: joined.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(fixture.invalid(format!(
                "project bundle path {} must not contain a symlink",
                joined.display()
            )));
        }
        let is_last = index + 1 == components.len();
        if (is_last && !metadata.is_file()) || (!is_last && !metadata.is_dir()) {
            return Err(fixture.invalid(format!(
                "project bundle path {} is not a {}",
                joined.display(),
                if is_last { "regular file" } else { "directory" }
            )));
        }
    }
    Ok(joined)
}

fn collect_script_paths(
    fixture: &ConformanceFixture,
    root: &Path,
    relative_dir: &Path,
    out: &mut BTreeSet<PathBuf>,
) -> Result<(), ConformanceError> {
    let dir = root.join(relative_dir);
    let dir_metadata = fs::symlink_metadata(&dir).map_err(|source| ConformanceError::Io {
        path: dir.clone(),
        source,
    })?;
    if dir_metadata.file_type().is_symlink() {
        return Err(fixture.invalid(format!(
            "project bundle contains symlink {}, which cannot be hashed portably",
            relative_dir.display()
        )));
    }
    if !dir_metadata.is_dir() {
        return Err(fixture.invalid(format!(
            "project script root {} is not a directory",
            relative_dir.display()
        )));
    }
    let mut entries = fs::read_dir(&dir)
        .map_err(|source| ConformanceError::Io {
            path: dir.clone(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| ConformanceError::Io {
            path: dir.clone(),
            source,
        })?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let file_type = entry.file_type().map_err(|source| ConformanceError::Io {
            path: entry.path(),
            source,
        })?;
        let relative = relative_dir.join(entry.file_name());
        if file_type.is_symlink() {
            return Err(fixture.invalid(format!(
                "project bundle contains symlink {}, which cannot be hashed portably",
                relative.display()
            )));
        }
        if file_type.is_dir() {
            collect_script_paths(fixture, root, &relative, out)?;
        } else if file_type.is_file()
            && entry.path().extension().and_then(|value| value.to_str()) == Some("m1scr")
        {
            out.insert(relative);
        }
    }
    Ok(())
}

fn fixture_scenario(
    fixture: &ConformanceFixture,
    loaded: &Loaded,
) -> Result<Scenario, ConformanceError> {
    let mut initial_by_channel = BTreeMap::new();
    let mut initial_state = Vec::with_capacity(fixture.initial_state.len());
    for initial in &fixture.initial_state {
        let value = initial.value.to_value(fixture, &loaded.project)?;
        initial_by_channel.insert(initial.channel.clone(), value.clone());
        initial_state.push(InitialValue {
            channel: initial.channel.clone(),
            value,
        });
    }

    let mut updates: BTreeMap<String, Vec<(f64, Value)>> = BTreeMap::new();
    for (index, step) in fixture.steps.iter().enumerate() {
        let grid_time = index as f64 / fixture.calculation_rate_hz;
        for input in &step.inputs {
            updates
                .entry(input.channel.clone())
                .or_default()
                .push((grid_time, input.value.to_value(fixture, &loaded.project)?));
        }
    }
    for (channel, points) in &mut updates {
        if points.first().is_some_and(|(time, _)| *time > 0.0) {
            let initial = initial_by_channel
                .get(channel)
                .expect("late input was required to have initial state");
            points.insert(0, (0.0, initial.clone()));
        }
    }
    let inputs = updates
        .into_iter()
        .map(|(channel, points)| InputSeries {
            channel,
            kind: InputKind::Series(points),
        })
        .collect();
    let duration_s = fixture.steps.len() as f64 / fixture.calculation_rate_hz;
    if !duration_s.is_finite() || duration_s <= 0.0 {
        return Err(fixture.invalid("fixture tick grid produces an invalid duration"));
    }
    Ok(Scenario {
        mode: fixture.resolved_mode(),
        initial_state,
        inputs,
        duration_s,
        base_rate_hz: fixture.calculation_rate_hz,
        overrides: Vec::new(),
        io: Vec::new(),
        serial: Default::default(),
        allow_default_inputs: false,
    })
}

fn compare_trace(
    fixture: &ConformanceFixture,
    project: &Project,
    trace: &Trace,
) -> Result<(), ConformanceError> {
    if trace.time.len() != fixture.steps.len() {
        return Err(fixture.invalid(format!(
            "runner produced {} ticks for {} fixture steps",
            trace.time.len(),
            fixture.steps.len()
        )));
    }
    let declared_sources: BTreeSet<&str> = fixture
        .initial_state
        .iter()
        .map(|value| value.channel.as_str())
        .chain(
            fixture
                .steps
                .iter()
                .flat_map(|step| step.inputs.iter().map(|input| input.channel.as_str())),
        )
        .collect();
    if let Some(source) = trace
        .external
        .iter()
        .find(|source| !declared_sources.contains(source.as_str()))
    {
        return Err(fixture.invalid(format!(
            "evaluation used undeclared external source {source:?}; every external source must be an explicit fixture input or initial-state channel"
        )));
    }
    for (index, step) in fixture.steps.iter().enumerate() {
        if (trace.time[index] - step.time_s).abs() > 1e-12_f64.max(step.time_s.abs() * 1e-12) {
            return Err(fixture.invalid(format!(
                "runner tick {index} was at {} s, fixture records {} s",
                trace.time[index], step.time_s
            )));
        }
        for expected in &step.expected {
            if trace.channel_is_external_at_tick(&expected.channel, index) {
                return Err(fixture.invalid(format!(
                    "step {index} expected channel {:?} is externally supplied rather than evaluator-computed at that tick",
                    expected.channel
                )));
            }
            let actual = trace
                .channel_value_at_tick(&expected.channel, index)
                .cloned();
            let alignment_error = actual.is_none().then(|| {
                if trace.channels.contains_key(&expected.channel) {
                    "trace channel has no value aligned to this tick".to_string()
                } else {
                    "trace has no channel with this path".to_string()
                }
            });
            let expected_value = expected.value.to_value(fixture, project)?;
            let result = match (actual.as_ref(), alignment_error) {
                (Some(actual), None) => {
                    compare_value(&expected_value, actual, expected.tolerance.as_ref())
                }
                (_, Some(detail)) => Err(detail),
                (None, None) => Err("trace has no aligned value for this channel".to_string()),
            };
            if let Err(detail) = result {
                return Err(ConformanceError::Mismatch(Box::new(ConformanceMismatch {
                    fixture: fixture.name.clone(),
                    step: index,
                    time_s: step.time_s,
                    channel: expected.channel.clone(),
                    expected: expected_text(expected),
                    actual,
                    detail,
                })));
            }
        }
    }
    Ok(())
}

fn validate_channel_name(
    fixture: &ConformanceFixture,
    channel: &str,
    label: &str,
) -> Result<(), ConformanceError> {
    if channel.trim().is_empty() {
        Err(fixture.invalid(format!("{label} channel must not be empty")))
    } else {
        Ok(())
    }
}

fn validate_tolerance(
    fixture: &ConformanceFixture,
    step: usize,
    expected: &ExpectedChannelValue,
) -> Result<(), ConformanceError> {
    match (&expected.value, &expected.tolerance) {
        (
            WireValue::FloatingPoint { .. },
            Some(ValueTolerance::FloatingPoint { absolute, relative }),
        ) => {
            if absolute.is_none() && relative.is_none() {
                return Err(fixture.invalid(format!(
                    "step {step} channel {:?} needs an absolute or relative floating-point tolerance",
                    expected.channel
                )));
            }
            for (name, bound) in [("absolute", absolute), ("relative", relative)] {
                if let Some(bound) = bound {
                    parse_tolerance(bound).map_err(|detail| {
                        fixture.invalid(format!(
                            "step {step} channel {:?} has invalid {name} tolerance: {detail}",
                            expected.channel
                        ))
                    })?;
                }
            }
            Ok(())
        }
        (WireValue::FloatingPoint { .. }, _) => Err(fixture.invalid(format!(
            "step {step} channel {:?} needs a floating-point tolerance",
            expected.channel
        ))),
        (WireValue::FixedPoint7dps { .. }, Some(ValueTolerance::FixedPoint7dps { .. })) => Ok(()),
        (WireValue::FixedPoint7dps { .. }, _) => Err(fixture.invalid(format!(
            "step {step} channel {:?} needs a fixed-point raw tolerance",
            expected.channel
        ))),
        (_, None) => Ok(()),
        (_, Some(_)) => Err(fixture.invalid(format!(
            "step {step} channel {:?} has a tolerance, but its type compares exactly",
            expected.channel
        ))),
    }
}

fn compare_value(
    expected: &Value,
    actual: &Value,
    tolerance: Option<&ValueTolerance>,
) -> Result<(), String> {
    match (expected, actual) {
        (Value::Bool(expected), Value::Bool(actual)) => exact(*expected == *actual),
        (Value::M1(M1Scalar::Integer(expected)), Value::M1(M1Scalar::Integer(actual))) => {
            exact(expected == actual)
        }
        (
            Value::M1(M1Scalar::UnsignedInteger(expected)),
            Value::M1(M1Scalar::UnsignedInteger(actual)),
        ) => exact(expected == actual),
        (
            Value::M1(M1Scalar::FixedPoint7dps(expected)),
            Value::M1(M1Scalar::FixedPoint7dps(actual)),
        ) => {
            let Some(ValueTolerance::FixedPoint7dps { raw }) = tolerance else {
                return Err("missing fixed-point tolerance".to_string());
            };
            let delta = (i64::from(expected.raw()) - i64::from(actual.raw())).unsigned_abs();
            if delta <= u64::from(*raw) {
                Ok(())
            } else {
                Err(format!("raw delta {delta} exceeds tolerance {raw}"))
            }
        }
        (
            Value::M1(M1Scalar::FloatingPoint(expected)),
            Value::M1(M1Scalar::FloatingPoint(actual)),
        ) => compare_float(*expected, *actual, tolerance),
        (
            Value::Enum {
                id: expected_id,
                member: expected_member,
            },
            Value::Enum {
                id: actual_id,
                member: actual_member,
            },
        ) => exact(expected_id == actual_id && expected_member == actual_member),
        (Value::Str(expected), Value::Str(actual)) => exact(expected == actual),
        _ => Err(format!(
            "runtime type differs, expected {}, got {}",
            runtime_value_text(expected),
            runtime_value_text(actual)
        )),
    }
}

fn exact(matches: bool) -> Result<(), String> {
    if matches {
        Ok(())
    } else {
        Err("values differ under exact comparison".to_string())
    }
}

fn compare_float(
    expected: f32,
    actual: f32,
    tolerance: Option<&ValueTolerance>,
) -> Result<(), String> {
    if expected.is_nan() {
        return exact(actual.is_nan())
            .map_err(|_| "expected NaN, but the actual value is not NaN".to_string());
    }
    if expected.is_infinite() {
        return exact(actual == expected).map_err(|_| {
            format!(
                "expected {}, got {}",
                float_text(expected),
                float_text(actual)
            )
        });
    }
    if !actual.is_finite() {
        return Err(format!(
            "expected a finite value, got {}",
            float_text(actual)
        ));
    }
    let Some(ValueTolerance::FloatingPoint { absolute, relative }) = tolerance else {
        return Err("missing floating-point tolerance".to_string());
    };
    let absolute = absolute
        .as_deref()
        .map(parse_tolerance)
        .transpose()
        .map_err(|detail| format!("invalid absolute tolerance: {detail}"))?
        .unwrap_or(0.0);
    let relative = relative
        .as_deref()
        .map(parse_tolerance)
        .transpose()
        .map_err(|detail| format!("invalid relative tolerance: {detail}"))?
        .unwrap_or(0.0);
    let expected = f64::from(expected);
    let actual = f64::from(actual);
    let delta = (actual - expected).abs();
    let allowed = absolute.max(relative * expected.abs().max(actual.abs()));
    if delta <= allowed {
        Ok(())
    } else {
        Err(format!(
            "absolute delta {delta} exceeds allowed error {allowed}"
        ))
    }
}

fn parse_wire_float(text: &str) -> Result<f32, String> {
    let trimmed = text.trim();
    let explicit = match trimmed.to_ascii_lowercase().as_str() {
        "nan" | "+nan" | "-nan" => Some(f32::NAN),
        "inf" | "+inf" | "infinity" | "+infinity" => Some(f32::INFINITY),
        "-inf" | "-infinity" => Some(f32::NEG_INFINITY),
        _ => None,
    };
    if let Some(value) = explicit {
        return Ok(value);
    }
    let value = trimmed
        .parse::<f32>()
        .map_err(|_| format!("floating-point value {text:?} is not a binary32 decimal"))?;
    if value.is_infinite() {
        Err(format!(
            "finite decimal {text:?} is outside the binary32 range"
        ))
    } else {
        Ok(value)
    }
}

fn parse_tolerance(text: &str) -> Result<f64, String> {
    let value = text
        .trim()
        .parse::<f64>()
        .map_err(|_| format!("{text:?} is not a decimal number"))?;
    if !value.is_finite() || value < 0.0 {
        Err(format!("{text:?} must be finite and non-negative"))
    } else {
        Ok(value)
    }
}

fn expected_text(expected: &ExpectedChannelValue) -> String {
    let value = expected.value.describe();
    match &expected.tolerance {
        None => format!("{value} exactly"),
        Some(ValueTolerance::FixedPoint7dps { raw }) => {
            format!("{value} within {raw} raw unit(s)")
        }
        Some(ValueTolerance::FloatingPoint { absolute, relative }) => format!(
            "{value} with absolute={} relative={}",
            absolute.as_deref().unwrap_or("unset"),
            relative.as_deref().unwrap_or("unset")
        ),
    }
}

fn runtime_value_text(value: &Value) -> String {
    match value {
        Value::Bool(value) => format!("boolean({value})"),
        Value::M1(M1Scalar::FloatingPoint(value)) => {
            format!("floating-point({})", float_text(*value))
        }
        Value::M1(M1Scalar::Integer(value)) => format!("integer({value})"),
        Value::M1(M1Scalar::UnsignedInteger(value)) => {
            format!("unsigned-integer({value})")
        }
        Value::M1(M1Scalar::FixedPoint7dps(value)) => {
            format!("fixed-point-7dps(raw={})", value.raw())
        }
        Value::Enum { id, member } => format!("enum(id={id}, member={member:?})"),
        Value::Str(value) => format!("string({value:?})"),
    }
}

fn float_text(value: f32) -> String {
    if value.is_nan() {
        "NaN".to_string()
    } else if value == f32::INFINITY {
        "Infinity".to_string()
    } else if value == f32::NEG_INFINITY {
        "-Infinity".to_string()
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparison_rules_are_family_specific() {
        let float_tolerance = ValueTolerance::FloatingPoint {
            absolute: Some("0.01".to_string()),
            relative: None,
        };
        assert!(
            compare_value(
                &Value::m1_float(1.0),
                &Value::m1_float(1.005),
                Some(&float_tolerance),
            )
            .is_ok()
        );
        assert!(
            compare_value(
                &Value::m1_float(1.0),
                &Value::m1_float(1.02),
                Some(&float_tolerance),
            )
            .is_err()
        );
        assert!(compare_value(&Value::Bool(true), &Value::Bool(false), None).is_err());
        assert!(compare_value(&Value::m1_integer(1), &Value::m1_integer(2), None).is_err());
        assert!(compare_value(&Value::m1_unsigned(1), &Value::m1_integer(1), None,).is_err());

        let fixed_tolerance = ValueTolerance::FixedPoint7dps { raw: 2 };
        let fixed = |raw| Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(raw)));
        assert!(compare_value(&fixed(100), &fixed(102), Some(&fixed_tolerance)).is_ok());
        assert!(compare_value(&fixed(100), &fixed(103), Some(&fixed_tolerance)).is_err());

        let relative_tolerance = ValueTolerance::FloatingPoint {
            absolute: None,
            relative: Some("0.01".to_string()),
        };
        assert!(
            compare_value(
                &Value::m1_float(100.0),
                &Value::m1_float(101.0),
                Some(&relative_tolerance),
            )
            .is_ok()
        );
        assert!(
            compare_value(
                &Value::m1_float(100.0),
                &Value::m1_float(102.0),
                Some(&relative_tolerance),
            )
            .is_err()
        );
    }

    #[test]
    fn non_finite_floats_follow_explicit_rules() {
        let tolerance = ValueTolerance::FloatingPoint {
            absolute: Some("1000".to_string()),
            relative: Some("1000".to_string()),
        };
        assert!(
            compare_value(
                &Value::m1_float(f32::NAN),
                &Value::m1_float(f32::NAN),
                Some(&tolerance),
            )
            .is_ok()
        );
        assert!(
            compare_value(
                &Value::m1_float(f32::INFINITY),
                &Value::m1_float(f32::NEG_INFINITY),
                Some(&tolerance),
            )
            .is_err()
        );
        assert!(
            compare_value(
                &Value::m1_float(1.0),
                &Value::m1_float(f32::NAN),
                Some(&tolerance),
            )
            .is_err()
        );
    }

    #[test]
    fn finite_overflow_is_not_an_infinity_spelling() {
        assert!(parse_wire_float("Infinity").unwrap().is_infinite());
        assert!(parse_wire_float("1e9999").is_err());
        assert_eq!(
            parse_wire_float("-0").unwrap().to_bits(),
            (-0.0_f32).to_bits()
        );
    }

    #[test]
    fn m1_sim_provenance_spelling_is_stable() {
        let provenance: FixtureProvenance = toml::from_str(
            r#"
kind = "m1-sim"
source = "capture session"
procedure = "documented procedure"
tool_version = "1.2.3"
captured_at_utc = "2026-08-30T12:00:00Z"
"#,
        )
        .expect("m1-sim provenance parses");
        assert_eq!(provenance.kind, ProvenanceKind::M1Sim);
    }

    #[test]
    fn sparse_trace_column_is_not_shifted_onto_the_first_tick() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/conformance/synthetic-mini.toml");
        let fixture = ConformanceFixture::from_path(&path).expect("fixture parses");
        let bundle = resolve_and_verify_bundle(&fixture).expect("bundle verifies");
        let loaded = load(&bundle.project, bundle.config.as_deref()).expect("project loads");
        let mut trace = Trace::new();
        trace.push_tick(fixture.steps[0].time_s);
        trace.push_tick(fixture.steps[1].time_s);
        trace.record_channel("Root.Demo.Output", Value::m1_float(50.0));
        trace.record_channel("Root.Demo.Output", Value::m1_float(50.0));
        trace.push_tick(fixture.steps[2].time_s);
        trace.record_channel("Root.Demo.Output", Value::m1_float(50.0));

        let error = compare_trace(&fixture, &loaded.project, &trace)
            .expect_err("same-tick writes must not fill an earlier sparse tick");
        let ConformanceError::Mismatch(mismatch) = error else {
            panic!("expected mismatch, got {error}");
        };
        assert_eq!(mismatch.step, 0);
        assert_eq!(mismatch.actual, None);
        assert!(mismatch.detail.contains("no value aligned to this tick"));
    }

    #[test]
    fn comparison_uses_the_final_channel_assignment_on_each_tick() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/conformance/synthetic-mini.toml");
        let fixture = ConformanceFixture::from_path(&path).expect("fixture parses");
        let bundle = resolve_and_verify_bundle(&fixture).expect("bundle verifies");
        let loaded = load(&bundle.project, bundle.config.as_deref()).expect("project loads");
        let mut trace = Trace::new();
        for step in &fixture.steps {
            trace.push_tick(step.time_s);
            trace.record_channel("Root.Demo.Output", Value::m1_float(-1.0));
            trace.record_channel("Root.Demo.Output", Value::m1_float(50.0));
        }

        compare_trace(&fixture, &loaded.project, &trace)
            .expect("the final assignment on every tick matches");
    }
}
