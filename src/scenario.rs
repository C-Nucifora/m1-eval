// SPDX-License-Identifier: GPL-3.0-or-later
//! The [`Scenario`]: the user-authored description of *how to drive a run*.
//!
//! A scenario chooses the run mode (which runner, against which function or
//! target channel), the time grid (`duration_s` + `base_rate_hz`), the input
//! sources for the channels the engine does not itself compute (constants or
//! piecewise time series), any channel overrides that pin a value over the
//! top of everything else, and any hardware-call overrides (`[[io]]`) that
//! drive a hardware-backed call directly.
//!
//! ## Wire formats
//!
//! The primary format is TOML; JSON is accepted too (the same shape via `serde`).
//! A scenario is *declarative data* — no wall-clock, no RNG — so a given file
//! always produces the same seeded inputs for a given tick grid.
//!
//! ```toml
//! mode = "function"          # or "cone"
//! target = "Root.Demo.Update"  # function name (function mode) or channel (cone)
//! duration_s = 1.0
//! base_rate_hz = 100.0
//!
//! [[inputs]]
//! channel = "Root.Demo.Gain"
//! const = 2.5
//!
//! [[inputs]]
//! channel = "Root.Demo.Speed"
//! series = [[0.0, 0.0], [0.5, 50.0]]   # (t_seconds, value) keyframes
//!
//! [[overrides]]
//! channel = "Root.Demo.Output"
//! const = 0.0
//!
//! [[io]]
//! call = "CanComms.GetFloat"          # wildcard for every matching call site
//! series = [[0.0, 12.5], [0.5, 99.0]]
//!
//! [[io]]
//! call = "CanComms.GetFloat"          # this occurrence only
//! script = "Demo.Update.m1scr"
//! offset = 418
//! const = 7.5
//! ```
//!
//! ## Time-series resampling
//!
//! A `series` is a list of `(t, value)` keyframes. At a tick instant `t` the
//! engine samples the series by *holding* the most recent keyframe at or before
//! `t` (zero-order hold / step), which is deterministic and avoids inventing
//! values between samples. Before the first keyframe the first value is held.
//! Numeric keyframes are stored as M1-width [`Value::M1`] scalars. Existing bare
//! numbers remain compatible and narrow to `Integer` or binary32. Callers that
//! need an exact family can use a typed object such as
//! `const = { unsigned = 4294967295 }` or
//! `series = [[0.0, { fixed_raw = 12345678 }]]`. An [`InputSeries`] of kind
//! [`InputKind::Const`] holds a single value for every tick.
//!
//! Identifiers may contain spaces (`Cooling Fan.Output`); channel names are used
//! verbatim as canonical-ish paths and never split on whitespace.

use crate::env::CallSite;
use crate::error::EvalError;
use crate::value::{FixedPoint7dps, M1Scalar, Value};
use serde::Deserialize;
use std::collections::BTreeSet;

/// Which runner a scenario drives, and the thing it targets.
#[derive(Debug, Clone, PartialEq)]
pub enum RunMode {
    /// Run a single function each tick. The string is the function's name — the
    /// runner resolves it to a script/symbol. Accepts the script basename, the
    /// `Foo.Update` stem, or the canonical `Root.Foo.Update` path.
    Function(String),
    /// Run a target channel plus its upstream dependency cone. The string is the
    /// canonical channel path the user wants computed.
    Cone(String),
    /// Run *every* periodically-scheduled function whose effective project
    /// trigger resolves to a rate. Each runs at its own rate in
    /// dependency-then-rate order. The offline schedule model has no single
    /// target: the runner schedules the whole project.
    WholeProject,
}

/// One input source the engine is *given* rather than computes.
#[derive(Debug, Clone, PartialEq)]
pub struct InputSeries {
    /// The channel/parameter path this drives (verbatim; spaces preserved).
    pub channel: String,
    /// Whether it is a constant or a time series.
    pub kind: InputKind,
}

/// One value present in the evaluator's channel store before startup code or
/// the first periodic tick runs.
///
/// Unlike an [`InputSeries`], this value is seeded once. It can initialise a
/// channel that the evaluated scripts later update without pinning that channel
/// back to the captured value on every tick.
#[derive(Debug, Clone, PartialEq)]
pub struct InitialValue {
    /// The channel path to seed.
    pub channel: String,
    /// Its typed initial value.
    pub value: Value,
}

/// A constant value or a `(t, value)` time series.
#[derive(Debug, Clone, PartialEq)]
pub enum InputKind {
    /// One value held for the whole run.
    Const(Value),
    /// `(t_seconds, value)` keyframes, ascending in `t`. Sampled by zero-order
    /// hold at each tick.
    Series(Vec<(f64, Value)>),
}

impl InputKind {
    /// Sample this source at tick time `t` (seconds). A constant returns its
    /// value at every `t`; a series returns the most recent keyframe value at or
    /// before `t` (zero-order hold), or the first keyframe before the series
    /// begins.
    pub fn sample(&self, t: f64) -> Value {
        match self {
            InputKind::Const(v) => v.clone(),
            InputKind::Series(points) => sample_series(points, t),
        }
    }
}

impl InputSeries {
    /// Sample this input at tick time `t` (seconds) — see [`InputKind::sample`].
    pub fn sample(&self, t: f64) -> Value {
        self.kind.sample(t)
    }
}

/// One scenario-driven hardware-call override: the call name plus the value the
/// call returns over time. The evaluator samples it per tick like an input and
/// consults it before an external adapter or built-in fallback.
#[derive(Debug, Clone, PartialEq)]
pub struct IoSeries {
    /// The hardware call this drives, spelled `"Object.Method"` (e.g.
    /// `"CanComms.GetFloat"`, `"System.FlashSize"`, `"DBC PC.Dash
    /// Switches.Receive"`). Dispatch tries the resolved canonical name and the
    /// source spelling, with spaces preserved verbatim.
    pub call: String,
    /// Exact call occurrence to drive. `None` is the backwards-compatible
    /// wildcard which applies to every site of `call`. An exact selector wins
    /// over the wildcard for the same call.
    pub site: Option<CallSite>,
    /// Whether it is a constant or a time series.
    pub kind: InputKind,
}

impl IoSeries {
    /// Sample this override at tick time `t` (seconds) — see
    /// [`InputKind::sample`].
    pub fn sample(&self, t: f64) -> Value {
        self.kind.sample(t)
    }
}

/// Zero-order-hold sample of an ascending `(t, value)` keyframe series at `t`.
/// Holds the first value before the series starts and the last value after it
/// ends. An empty series is a programming error upstream; the M1 binary32 zero
/// fallback is unreachable because the parser rejects empty series.
fn sample_series(points: &[(f64, Value)], t: f64) -> Value {
    let mut held: Option<&Value> = None;
    for (kt, v) in points {
        if *kt <= t {
            held = Some(v);
        } else {
            break;
        }
    }
    match held {
        Some(v) => v.clone(),
        // Before the first keyframe: hold the first value.
        None => points
            .first()
            .map(|(_, v)| v.clone())
            .unwrap_or(Value::m1_float(0.0)),
    }
}

/// The fully-parsed scenario: run mode, time grid, inputs, and overrides.
#[derive(Debug, Clone, PartialEq)]
pub struct Scenario {
    /// Which runner and target.
    pub mode: RunMode,
    /// Values seeded once before startup code and the first tick. These model a
    /// captured evaluator state, while [`Scenario::inputs`] model values driven
    /// throughout the run.
    pub initial_state: Vec<InitialValue>,
    /// Externally-driven input sources (constants + series).
    pub inputs: Vec<InputSeries>,
    /// Total run duration in seconds. Ticks span `[0, duration_s)`.
    pub duration_s: f64,
    /// Base tick rate in Hz; the tick step is `dt = 1 / base_rate_hz`.
    pub base_rate_hz: f64,
    /// Channels pinned to a constant or series, layered *over* the inputs and
    /// any computed value. Same shape as [`Scenario::inputs`].
    pub overrides: Vec<InputSeries>,
    /// Scenario-driven hardware-call overrides (`[[io]]`), keyed by
    /// `"Object.Method"` and resampled every tick. See [`IoSeries`].
    pub io: Vec<IoSeries>,
    /// Deterministic virtual-serial inputs. Each declaration becomes visible to
    /// `Serial.Receive` when evaluator time reaches its timestamp.
    pub serial: SerialScenario,
    /// Whole-project mode only: substitute type-correct startup defaults for
    /// unseeded channel reads (each substitution is reported on the trace)
    /// instead of failing loud. **Off by default** — strict fail-loud is the
    /// baseline; defaulting is an explicit, visible opt-in.
    pub allow_default_inputs: bool,
}

/// Scenario-owned inputs for the deterministic virtual serial adapter.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SerialScenario {
    /// Received byte chunks, ordered by timestamp and then declaration order.
    pub rx: Vec<SerialRx>,
}

/// One byte chunk made available on a virtual RS232 port at evaluator time.
#[derive(Debug, Clone, PartialEq)]
pub struct SerialRx {
    /// Seconds from the beginning of the run. Startup and periodic tick zero
    /// both observe declarations at `0.0`.
    pub time_s: f64,
    /// Non-negative M1 serial port number.
    pub port: i32,
    /// Bytes appended to the port's receive stream at [`SerialRx::time_s`].
    pub bytes: Vec<u8>,
}

impl Scenario {
    /// Parse a scenario from a TOML document.
    pub fn from_toml_str(s: &str) -> Result<Scenario, EvalError> {
        let raw: RawScenario = toml::from_str(s).map_err(|e| EvalError::UnsupportedConstruct {
            kind: format!("scenario TOML parse error: {e}"),
            at: 0,
        })?;
        raw.into_scenario()
    }

    /// Parse a scenario from a JSON document (the same shape as the TOML).
    pub fn from_json_str(s: &str) -> Result<Scenario, EvalError> {
        let raw: RawScenario =
            serde_json::from_str(s).map_err(|e| EvalError::UnsupportedConstruct {
                kind: format!("scenario JSON parse error: {e}"),
                at: 0,
            })?;
        raw.into_scenario()
    }

    /// Fill `Series` inputs from a CSV time-series sidecar. The CSV's first column
    /// is `time` (seconds); every other column header is a channel name. Each
    /// matching channel gets a `Series` of `(time, cell)` rows, *replacing* any
    /// previously-declared input for that channel. Columns whose header names no
    /// declared-or-new input are added as new `Series` inputs (so a CSV can drive
    /// channels the TOML did not mention).
    ///
    /// Determinism: rows are taken in file order; the `time` column must be
    /// ascending for the zero-order-hold sampler to behave, but we do not sort
    /// (a non-monotonic log is the caller's problem and would be surfaced by the
    /// sampler holding the last in-order keyframe).
    pub fn load_csv(&mut self, csv: &str) -> Result<(), EvalError> {
        // `load_csv` predates the i2 units row, so it does not detect one: a
        // non-numeric first cell remains a hard error here (its long-standing
        // behaviour). The shared parser carries the optional units-row handling
        // for [`crate::log::Log::from_csv`].
        let parsed = parse_time_series_csv(csv, false)?;
        for (channel, points) in parsed.columns {
            if points.is_empty() {
                continue;
            }
            let input = InputSeries {
                channel: channel.clone(),
                kind: InputKind::Series(points),
            };
            // Replace any existing same-channel input; else append.
            match self.inputs.iter_mut().find(|i| i.channel == channel) {
                Some(existing) => *existing = input,
                None => self.inputs.push(input),
            }
        }
        Ok(())
    }
}

/// One parsed `time`-first CSV: the per-channel keyframes plus an optional
/// captured units row. Shared by [`Scenario::load_csv`] and
/// [`crate::log::Log::from_csv`] so there is a single CSV parser, not two.
pub(crate) struct ParsedTimeSeriesCsv {
    /// One `(channel name, ascending (t, value) keyframes)` per non-time column,
    /// in header order. Channels with no non-empty cell carry an empty `Vec`.
    pub columns: Vec<(String, Vec<(f64, Value)>)>,
    /// If `detect_units` was set and the first data row's first cell was
    /// non-numeric, the units row mapped `channel name -> unit string` (empty
    /// units cells are skipped). Otherwise empty.
    pub units: std::collections::BTreeMap<String, String>,
}

/// Parse a `time`-first CSV into per-channel `(t, value)` keyframes.
///
/// The first column header must be `time` (case-insensitive); every other column
/// header is a channel name (verbatim, spaces allowed). Each data row is
/// `t_seconds, value, value, …`: `t` parses to `f64`, numeric cells to
/// M1 binary32 values, empty cells add no keyframe (zero-order hold keeps the prior
/// value), and a non-numeric value cell fails loud as an [`EvalError::TypeError`].
///
/// When `detect_units` is set, a *first data row whose first cell is non-numeric*
/// is treated as an i2-style units row (e.g. `s,rpm,km/h`): its cells are diverted
/// into `units` and it contributes no keyframe. With `detect_units` clear, a
/// non-numeric first cell is the long-standing "non-numeric time" hard error.
pub(crate) fn parse_time_series_csv(
    csv: &str,
    detect_units: bool,
) -> Result<ParsedTimeSeriesCsv, EvalError> {
    let mut lines = csv.lines();
    let header = lines
        .next()
        .ok_or_else(|| EvalError::UnsupportedConstruct {
            kind: "empty CSV: no header row".to_string(),
            at: 0,
        })?;
    let cols: Vec<String> = split_csv_row(header);
    if cols.is_empty() || !cols[0].eq_ignore_ascii_case("time") {
        return Err(EvalError::UnsupportedConstruct {
            kind: "CSV first column must be `time`".to_string(),
            at: 0,
        });
    }
    // Duplicate headers: whichever column the sampler later picked would be
    // arbitrary — fail loud naming the duplicate.
    {
        let mut seen = std::collections::BTreeSet::new();
        for c in &cols[1..] {
            if !seen.insert(c.as_str()) {
                return Err(EvalError::UnsupportedConstruct {
                    kind: format!("CSV declares the column {c:?} more than once"),
                    at: 0,
                });
            }
        }
    }
    // One accumulator per non-time column, in header order.
    let mut columns: Vec<(String, Vec<(f64, Value)>)> =
        cols[1..].iter().map(|c| (c.clone(), Vec::new())).collect();
    let mut units: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut seen_data_row = false;
    let mut prev_time: Option<f64> = None;

    for (row_idx, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let cells = split_csv_row(line);
        let first = cells.first().map(|c| c.trim()).unwrap_or("");

        // The optional units row is only ever the *first* non-empty data line and
        // only when its first cell is non-numeric. After that, a non-numeric first
        // cell is a hard "non-numeric time" error.
        let first_is_numeric = first.parse::<f64>().is_ok();
        if detect_units && !seen_data_row && !first_is_numeric {
            for (i, (channel, _)) in columns.iter().enumerate() {
                let Some(cell) = cells.get(i + 1) else {
                    continue;
                };
                let unit = cell.trim();
                if !unit.is_empty() {
                    units.insert(channel.clone(), unit.to_string());
                }
            }
            seen_data_row = true;
            continue;
        }
        seen_data_row = true;

        // A row wider than the header is a shifted or corrupt export. (Fewer
        // cells stays legal: trailing empty cells are the documented
        // no-keyframe hold.)
        if cells.len() > cols.len() {
            return Err(EvalError::UnsupportedConstruct {
                kind: format!(
                    "CSV row {} has {} cells but the header declares {} columns",
                    row_idx + 2,
                    cells.len(),
                    cols.len()
                ),
                at: 0,
            });
        }
        let t = first
            .parse::<f64>()
            .map_err(|_| EvalError::UnsupportedConstruct {
                kind: format!("CSV row {} has a non-numeric time", row_idx + 2),
                at: 0,
            })?;
        // The zero-order-hold sampler assumes strictly ascending finite
        // keyframes; a NaN/infinite or out-of-order/duplicate timestamp would
        // silently mis-sample every later lookup.
        if !t.is_finite() {
            return Err(EvalError::UnsupportedConstruct {
                kind: format!("CSV row {} has a non-finite time {t}", row_idx + 2),
                at: 0,
            });
        }
        if let Some(prev) = prev_time
            && t <= prev
        {
            return Err(EvalError::UnsupportedConstruct {
                kind: format!(
                    "CSV row {} time {t} is not strictly increasing (previous {prev})",
                    row_idx + 2
                ),
                at: 0,
            });
        }
        prev_time = Some(t);
        for (i, acc) in columns.iter_mut().enumerate() {
            let Some(cell) = cells.get(i + 1) else {
                continue;
            };
            let trimmed = cell.trim();
            if trimmed.is_empty() {
                continue;
            }
            let explicit_non_finite = explicit_non_finite_float(trimmed);
            let narrowed = match explicit_non_finite {
                Some(value) => value,
                None => trimmed.parse::<f32>().map_err(|_| EvalError::TypeError {
                    detail: format!(
                        "CSV row {} column {:?} value {trimmed:?} is not numeric",
                        row_idx + 2,
                        acc.0
                    ),
                })?,
            };
            if explicit_non_finite.is_none() && !narrowed.is_finite() {
                return Err(EvalError::TypeError {
                    detail: format!(
                        "CSV row {} column {:?} value {trimmed:?} is outside M1 binary32 range",
                        row_idx + 2,
                        acc.0
                    ),
                });
            }
            let v = Value::m1_float(narrowed);
            acc.1.push((t, v));
        }
    }

    Ok(ParsedTimeSeriesCsv { columns, units })
}

/// Parse the non-finite spellings emitted by [`crate::trace::Trace::to_csv`].
/// Keeping this lexical check separate lets CSV preserve real binary32
/// sentinels while still rejecting a finite decimal such as `1e9999` that
/// overflowed during host parsing.
fn explicit_non_finite_float(text: &str) -> Option<f32> {
    match text.to_ascii_lowercase().as_str() {
        "nan" | "+nan" | "-nan" => Some(f32::NAN),
        "inf" | "+inf" | "infinity" | "+infinity" => Some(f32::INFINITY),
        "-inf" | "-infinity" => Some(f32::NEG_INFINITY),
        _ => None,
    }
}

/// Split a CSV row into unquoted fields. Handles the minimal RFC-4180 quoting
/// the trace writer emits (double-quoted fields with `""` escapes). Shared with
/// [`crate::log`] so both CSV readers use one tokenizer.
pub(crate) fn split_csv_row(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut field = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes => {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            }
            '"' => in_quotes = true,
            ',' if !in_quotes => {
                out.push(std::mem::take(&mut field));
            }
            _ => field.push(c),
        }
    }
    out.push(field);
    out
}

// ---- serde wire model ----

/// The raw `serde`-deserialised scenario, before validation/normalisation into a
/// [`Scenario`]. Kept separate so the public type stays free of `serde` derives
/// and parse-time looseness (e.g. a `mode` string, an untyped `const`).
#[derive(Debug, Deserialize)]
struct RawScenario {
    mode: String,
    /// The target: a function name (function mode) or channel (cone mode).
    /// Optional on the wire: `whole-project` mode carries no single target, and
    /// `function`/`cone` modes get a fail-loud error below when it is missing.
    #[serde(default)]
    target: Option<String>,
    duration_s: f64,
    /// Base tick rate in Hz. Optional: when omitted (or `0`) in `whole-project`
    /// mode the runner derives the least common multiple of the scheduled rates
    /// as the base tick (so every rate has an exact integer period). The
    /// `function`/`cone` modes still require a positive value (they have no
    /// schedule to derive a default from).
    #[serde(default)]
    base_rate_hz: f64,
    #[serde(default)]
    initial_state: Vec<RawInitialValue>,
    #[serde(default)]
    inputs: Vec<RawInput>,
    #[serde(default)]
    overrides: Vec<RawInput>,
    #[serde(default)]
    io: Vec<RawIo>,
    #[serde(default)]
    serial: RawSerial,
    /// Opt-in unseeded-channel defaulting for whole-project mode (strict
    /// fail-loud when absent/false).
    #[serde(default)]
    allow_default_inputs: bool,
}

/// A raw one-time channel seed.
#[derive(Debug, Deserialize)]
struct RawInitialValue {
    channel: String,
    value: RawValue,
}

/// Wire shape for `[serial]` and its ergonomic `[[serial.rx]]` entries.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSerial {
    #[serde(default)]
    rx: Vec<RawSerialRx>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSerialRx {
    time_s: f64,
    port: i64,
    bytes: Vec<i64>,
}

/// A raw input/override entry: a channel plus exactly one of `const`/`series`.
#[derive(Debug, Deserialize)]
struct RawInput {
    channel: String,
    #[serde(default)]
    #[serde(rename = "const")]
    constant: Option<RawValue>,
    #[serde(default)]
    series: Option<Vec<(f64, RawValue)>>,
}

/// A raw `[[io]]` entry: a hardware call name plus exactly one of
/// `const`/`series`, with an optional exact call-site selector.
#[derive(Debug, Deserialize)]
struct RawIo {
    call: String,
    /// Both fields select one exact call occurrence. Omitting both creates a
    /// wildcard. Supplying only one is rejected.
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    #[serde(rename = "const")]
    constant: Option<RawValue>,
    #[serde(default)]
    series: Option<Vec<(f64, RawValue)>>,
}

/// A raw scalar value from the wire: a typed M1 object, number, boolean, or
/// string. TOML/JSON numbers come through as either integer or float and use the
/// M1-width narrowing rule at the wire boundary.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawValue {
    Typed(RawM1Value),
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Str(String),
}

/// Explicit typed scenario syntax for callers that must distinguish all four
/// M1 numeric families. Bare wire numbers narrow according to their JSON/TOML
/// number family.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawM1Value {
    #[serde(default)]
    integer: Option<i64>,
    #[serde(default)]
    unsigned: Option<u64>,
    #[serde(default)]
    floating_point: Option<f64>,
    #[serde(default)]
    fixed_raw: Option<i64>,
}

impl RawValue {
    fn into_value(self) -> Result<Value, EvalError> {
        match self {
            RawValue::Typed(value) => value.into_value(),
            RawValue::Bool(b) => Ok(Value::Bool(b)),
            RawValue::Int(i) => i32::try_from(i)
                .map(Value::m1_integer)
                .map_err(|_| scenario_width_error("integer", i)),
            RawValue::Uint(i) => i32::try_from(i)
                .map(Value::m1_integer)
                .map_err(|_| scenario_width_error("integer", i)),
            RawValue::Float(f) => scenario_float(f),
            RawValue::Str(s) => Ok(Value::Str(s)),
        }
    }
}

impl RawM1Value {
    fn into_value(self) -> Result<Value, EvalError> {
        let present = [
            self.integer.is_some(),
            self.unsigned.is_some(),
            self.floating_point.is_some(),
            self.fixed_raw.is_some(),
        ];
        if present.into_iter().filter(|field| *field).count() != 1 {
            return Err(EvalError::TypeError {
                detail: "typed scenario value must set exactly one of `integer`, `unsigned`, `floating_point`, or `fixed_raw`".to_string(),
            });
        }
        if let Some(value) = self.integer {
            return i32::try_from(value)
                .map(Value::m1_integer)
                .map_err(|_| scenario_width_error("integer", value));
        }
        if let Some(value) = self.unsigned {
            return u32::try_from(value)
                .map(Value::m1_unsigned)
                .map_err(|_| scenario_width_error("unsigned", value));
        }
        if let Some(value) = self.floating_point {
            return scenario_float(value);
        }
        let raw = self.fixed_raw.expect("exactly one typed field is present");
        i32::try_from(raw)
            .map(|raw| Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(raw))))
            .map_err(|_| scenario_width_error("fixed-point raw", raw))
    }
}

fn scenario_float(value: f64) -> Result<Value, EvalError> {
    let narrowed = value as f32;
    if value.is_finite() && narrowed.is_infinite() {
        Err(scenario_width_error("floating-point", value))
    } else {
        Ok(Value::m1_float(narrowed))
    }
}

fn scenario_width_error(kind: &str, value: impl std::fmt::Display) -> EvalError {
    EvalError::TypeError {
        detail: format!("scenario {kind} value {value} is outside its M1-width representation"),
    }
}

impl RawScenario {
    fn into_scenario(self) -> Result<Scenario, EvalError> {
        // `function`/`cone` require a target; `whole-project` ignores it (and so
        // it is optional only in that mode). Resolving the target here keeps the
        // fail-loud error attached to the mode that needs it.
        let require_target = |mode: &str| -> Result<String, EvalError> {
            self.target
                .clone()
                .ok_or_else(|| EvalError::UnsupportedConstruct {
                    kind: format!("scenario mode {mode:?} requires a `target`"),
                    at: 0,
                })
        };
        let mode = match self.mode.as_str() {
            "function" => RunMode::Function(require_target("function")?),
            "cone" => RunMode::Cone(require_target("cone")?),
            "whole-project" => RunMode::WholeProject,
            other => {
                return Err(EvalError::UnsupportedConstruct {
                    kind: format!(
                        "unknown scenario mode {other:?} (expected `function`, `cone`, or `whole-project`)"
                    ),
                    at: 0,
                });
            }
        };
        // `whole-project` accepts a missing/zero base rate (the runner derives
        // the lcm of the scheduled rates). Every other mode needs an explicit
        // positive base tick — there is no schedule to derive one from. A
        // negative rate is always invalid.
        if self.base_rate_hz < 0.0 {
            return Err(EvalError::UnsupportedConstruct {
                kind: format!(
                    "base_rate_hz must be non-negative, got {}",
                    self.base_rate_hz
                ),
                at: 0,
            });
        }
        if self.base_rate_hz == 0.0 && !matches!(mode, RunMode::WholeProject) {
            return Err(EvalError::UnsupportedConstruct {
                kind: format!(
                    "base_rate_hz must be positive for {:?} mode (only whole-project may omit it)",
                    self.mode
                ),
                at: 0,
            });
        }
        if self.duration_s < 0.0 {
            return Err(EvalError::UnsupportedConstruct {
                kind: format!("duration_s must be non-negative, got {}", self.duration_s),
                at: 0,
            });
        }
        let inputs = self
            .inputs
            .into_iter()
            .map(RawInput::into_input)
            .collect::<Result<Vec<_>, _>>()?;
        let initial_state = self
            .initial_state
            .into_iter()
            .map(|initial| {
                initial.value.into_value().map(|value| InitialValue {
                    channel: initial.channel,
                    value,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let overrides = self
            .overrides
            .into_iter()
            .map(RawInput::into_input)
            .collect::<Result<Vec<_>, _>>()?;
        let io = self
            .io
            .into_iter()
            .map(RawIo::into_io)
            .collect::<Result<Vec<_>, _>>()?;
        let serial = self.serial.into_serial()?;
        let mut io_selectors = BTreeSet::new();
        for entry in &io {
            if !io_selectors.insert((entry.call.clone(), entry.site.clone())) {
                let selector = match &entry.site {
                    Some(site) => format!(
                        "{} in {} at byte {}",
                        entry.call,
                        site.script(),
                        site.offset()
                    ),
                    None => format!("{} at every call site", entry.call),
                };
                return Err(EvalError::UnsupportedConstruct {
                    kind: format!("duplicate io selector {selector:?}"),
                    at: 0,
                });
            }
        }
        Ok(Scenario {
            mode,
            initial_state,
            inputs,
            duration_s: self.duration_s,
            base_rate_hz: self.base_rate_hz,
            overrides,
            io,
            serial,
            allow_default_inputs: self.allow_default_inputs,
        })
    }
}

impl RawSerial {
    fn into_serial(self) -> Result<SerialScenario, EvalError> {
        let mut rx = self
            .rx
            .into_iter()
            .enumerate()
            .map(|(declaration, entry)| {
                if !entry.time_s.is_finite() || entry.time_s < 0.0 {
                    return Err(EvalError::UnsupportedConstruct {
                        kind: format!(
                            "serial rx declaration {} has invalid time_s {} (expected a finite, non-negative time)",
                            declaration + 1,
                            entry.time_s
                        ),
                        at: 0,
                    });
                }
                let port = i32::try_from(entry.port).map_err(|_| {
                    EvalError::UnsupportedConstruct {
                        kind: format!(
                            "serial rx declaration {} has port {} outside the M1 Integer range",
                            declaration + 1,
                            entry.port
                        ),
                        at: 0,
                    }
                })?;
                if port < 0 {
                    return Err(EvalError::UnsupportedConstruct {
                        kind: format!(
                            "serial rx declaration {} has negative port {port}",
                            declaration + 1
                        ),
                        at: 0,
                    });
                }
                let bytes = entry
                    .bytes
                    .into_iter()
                    .enumerate()
                    .map(|(index, byte)| {
                        u8::try_from(byte).map_err(|_| EvalError::UnsupportedConstruct {
                            kind: format!(
                                "serial rx declaration {} byte {} is {byte}, outside 0..=255",
                                declaration + 1,
                                index
                            ),
                            at: 0,
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if bytes.len() > 256 {
                    return Err(EvalError::UnsupportedConstruct {
                        kind: format!(
                            "serial rx declaration {} has {} bytes, exceeding the 256-byte receive buffer",
                            declaration + 1,
                            bytes.len()
                        ),
                        at: 0,
                    });
                }
                Ok((
                    declaration,
                    SerialRx {
                        time_s: entry.time_s,
                        port,
                        bytes,
                    },
                ))
            })
            .collect::<Result<Vec<_>, EvalError>>()?;

        // Stable ordering makes out-of-order source declarations ergonomic while
        // preserving source order for chunks with the same timestamp.
        rx.sort_by(|(left_index, left), (right_index, right)| {
            left.time_s
                .total_cmp(&right.time_s)
                .then_with(|| left_index.cmp(right_index))
        });
        Ok(SerialScenario {
            rx: rx.into_iter().map(|(_, entry)| entry).collect(),
        })
    }
}

/// Validate a raw `const`/`series` pair into an [`InputKind`]: exactly one of
/// the two must be set, and a series must be non-empty. `what`/`name` label the
/// entry in the fail-loud message (`input "Root.Demo.Gain"`, `io
/// "CanComms.GetFloat"`).
fn raw_kind(
    what: &str,
    name: &str,
    constant: Option<RawValue>,
    series: Option<Vec<(f64, RawValue)>>,
) -> Result<InputKind, EvalError> {
    match (constant, series) {
        (Some(c), None) => Ok(InputKind::Const(c.into_value()?)),
        (None, Some(points)) => {
            if points.is_empty() {
                return Err(EvalError::UnsupportedConstruct {
                    kind: format!("{what} {name:?} has an empty series"),
                    at: 0,
                });
            }
            Ok(InputKind::Series(
                points
                    .into_iter()
                    .map(|(t, v)| v.into_value().map(|value| (t, value)))
                    .collect::<Result<Vec<_>, _>>()?,
            ))
        }
        (Some(_), Some(_)) => Err(EvalError::UnsupportedConstruct {
            kind: format!("{what} {name:?} sets both `const` and `series` (choose one)"),
            at: 0,
        }),
        (None, None) => Err(EvalError::UnsupportedConstruct {
            kind: format!("{what} {name:?} sets neither `const` nor `series`"),
            at: 0,
        }),
    }
}

impl RawInput {
    fn into_input(self) -> Result<InputSeries, EvalError> {
        let kind = raw_kind("input", &self.channel, self.constant, self.series)?;
        Ok(InputSeries {
            channel: self.channel,
            kind,
        })
    }
}

impl RawIo {
    fn into_io(self) -> Result<IoSeries, EvalError> {
        let kind = raw_kind("io", &self.call, self.constant, self.series)?;
        let site = match (self.script, self.offset) {
            (None, None) => None,
            (Some(script), Some(offset)) if !script.trim().is_empty() => {
                Some(CallSite::new(script, offset))
            }
            (Some(_), Some(_)) => {
                return Err(EvalError::UnsupportedConstruct {
                    kind: format!("io {:?} has an empty `script` selector", self.call),
                    at: 0,
                });
            }
            _ => {
                return Err(EvalError::UnsupportedConstruct {
                    kind: format!(
                        "io {:?} must set both `script` and `offset`, or omit both for a wildcard",
                        self.call
                    ),
                    at: 0,
                });
            }
        };
        Ok(IoSeries {
            call: self.call,
            kind,
            site,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOML: &str = r#"
mode = "function"
target = "Root.Demo.Update"
duration_s = 1.0
base_rate_hz = 100.0

[[inputs]]
channel = "Root.Demo.Gain"
const = 2.5

[[inputs]]
channel = "Root.Demo.Speed"
series = [[0.0, 0.0], [0.5, 50.0]]
"#;

    #[test]
    fn parses_toml_scenario() {
        let sc = Scenario::from_toml_str(TOML).expect("valid scenario");
        assert_eq!(sc.mode, RunMode::Function("Root.Demo.Update".to_string()));
        assert_eq!(sc.duration_s, 1.0);
        assert_eq!(sc.base_rate_hz, 100.0);
        assert_eq!(sc.inputs.len(), 2);

        // The constant input.
        let gain = sc
            .inputs
            .iter()
            .find(|i| i.channel == "Root.Demo.Gain")
            .unwrap();
        assert_eq!(gain.kind, InputKind::Const(Value::m1_float(2.5)));

        // The series input, sampled by zero-order hold.
        let speed = sc
            .inputs
            .iter()
            .find(|i| i.channel == "Root.Demo.Speed")
            .unwrap();
        // Before/at first keyframe -> 0.0.
        assert_eq!(speed.sample(0.0), Value::m1_float(0.0));
        assert_eq!(speed.sample(0.4), Value::m1_float(0.0));
        // At/after the second keyframe -> 50.0.
        assert_eq!(speed.sample(0.5), Value::m1_float(50.0));
        assert_eq!(speed.sample(0.99), Value::m1_float(50.0));
    }

    #[test]
    fn parses_and_stably_orders_virtual_serial_rx_declarations() {
        let scenario = Scenario::from_toml_str(
            r#"
mode = "function"
target = "Root.Demo.Update"
duration_s = 0.2
base_rate_hz = 100.0

[[serial.rx]]
time_s = 0.1
port = 2
bytes = [0x43]

[[serial.rx]]
time_s = 0.0
port = 2
bytes = [0x41]

[[serial.rx]]
time_s = 0.1
port = 2
bytes = [0x42]
"#,
        )
        .expect("serial scenario parses");

        assert_eq!(
            scenario.serial.rx,
            vec![
                SerialRx {
                    time_s: 0.0,
                    port: 2,
                    bytes: vec![0x41],
                },
                SerialRx {
                    time_s: 0.1,
                    port: 2,
                    bytes: vec![0x43],
                },
                SerialRx {
                    time_s: 0.1,
                    port: 2,
                    bytes: vec![0x42],
                },
            ]
        );
    }

    #[test]
    fn virtual_serial_wire_values_are_range_checked() {
        for (entry, expected) in [
            (
                "time_s = -0.1\nport = 0\nbytes = [1]",
                "finite, non-negative time",
            ),
            ("time_s = 0.0\nport = -1\nbytes = [1]", "negative port -1"),
            ("time_s = 0.0\nport = 0\nbytes = [256]", "outside 0..=255"),
            (
                &format!(
                    "time_s = 0.0\nport = 0\nbytes = [{}]",
                    std::iter::repeat_n("0", 257).collect::<Vec<_>>().join(", ")
                ),
                "257 bytes",
            ),
        ] {
            let source = format!(
                "mode = \"function\"\ntarget = \"Root.Demo.Update\"\nduration_s = 0.1\nbase_rate_hz = 100.0\n\n[[serial.rx]]\n{entry}\n"
            );
            let error = Scenario::from_toml_str(&source).expect_err("invalid serial input fails");
            assert!(
                error.to_string().contains(expected),
                "{error} should contain {expected:?}"
            );
        }
    }

    #[test]
    fn const_samples_constant_at_every_tick() {
        let i = InputSeries {
            channel: "X".to_string(),
            kind: InputKind::Const(Value::m1_integer(7)),
        };
        assert_eq!(i.sample(0.0), Value::m1_integer(7));
        assert_eq!(i.sample(123.4), Value::m1_integer(7));
    }

    #[test]
    fn json_parses_the_same_shape() {
        let json = r#"{
            "mode": "cone",
            "target": "Root.Demo.Output",
            "duration_s": 0.5,
            "base_rate_hz": 50.0,
            "inputs": [{ "channel": "Root.Demo.Speed", "const": 10 }]
        }"#;
        let sc = Scenario::from_json_str(json).expect("valid JSON scenario");
        assert_eq!(sc.mode, RunMode::Cone("Root.Demo.Output".to_string()));
        assert_eq!(sc.base_rate_hz, 50.0);
        assert_eq!(sc.inputs[0].kind, InputKind::Const(Value::m1_integer(10)));
    }

    #[test]
    fn csv_fills_series_inputs() {
        let mut sc = Scenario::from_toml_str(TOML).expect("valid scenario");
        // The CSV drives Speed (replacing its TOML series) and a new channel.
        let csv = "time,Root.Demo.Speed,Root.Demo.Brake\n0.0,0,1\n0.5,80,0\n";
        sc.load_csv(csv).expect("csv loads");

        let speed = sc
            .inputs
            .iter()
            .find(|i| i.channel == "Root.Demo.Speed")
            .unwrap();
        // The CSV series replaced the TOML one: at t=0.6 it holds 80.
        assert_eq!(speed.sample(0.6), Value::m1_float(80.0));

        // The new channel was added.
        let brake = sc
            .inputs
            .iter()
            .find(|i| i.channel == "Root.Demo.Brake")
            .expect("brake added from CSV");
        assert_eq!(brake.sample(0.0), Value::m1_float(1.0));
        assert_eq!(brake.sample(0.5), Value::m1_float(0.0));
    }

    #[test]
    fn csv_rejects_decimal_overflow_before_binary32_narrowing() {
        for value in ["1e39", "1e9999"] {
            let mut scenario =
                Scenario::from_toml_str("mode = \"whole-project\"\nduration_s = 0.1\n").unwrap();
            let csv = format!("time,Value\n0,{value}\n");
            let error = scenario.load_csv(&csv).unwrap_err();
            assert!(
                format!("{error}").contains("outside M1 binary32 range"),
                "{value}: {error}"
            );
        }
    }

    #[test]
    fn trace_csv_preserves_explicit_non_finite_binary32_values() {
        let mut trace = crate::trace::Trace::new();
        trace.push_tick(0.0);
        trace.record_channel("NaN", Value::m1_float(f32::NAN));
        trace.record_channel("Positive", Value::m1_float(f32::INFINITY));
        trace.record_channel("Negative", Value::m1_float(f32::NEG_INFINITY));

        let mut scenario =
            Scenario::from_toml_str("mode = \"whole-project\"\nduration_s = 0.1\n").unwrap();
        scenario.load_csv(&trace.to_csv()).unwrap();

        let sample = |name: &str| {
            scenario
                .inputs
                .iter()
                .find(|input| input.channel == name)
                .unwrap()
                .sample(0.0)
                .m1_scalar()
                .unwrap()
                .as_f32()
        };
        assert!(sample("NaN").is_nan());
        assert_eq!(sample("Positive"), f32::INFINITY);
        assert_eq!(sample("Negative"), f32::NEG_INFINITY);
    }

    #[test]
    fn typed_scenario_accepts_explicit_non_finite_binary32_values() {
        let scenario = Scenario::from_toml_str(
            r#"
mode = "whole-project"
duration_s = 0.1

[[inputs]]
channel = "NaN"
const = { floating_point = nan }

[[inputs]]
channel = "Positive"
const = { floating_point = inf }

[[inputs]]
channel = "Negative"
const = { floating_point = -inf }
"#,
        )
        .unwrap();
        let sample = |index: usize| {
            scenario.inputs[index]
                .sample(0.0)
                .m1_scalar()
                .unwrap()
                .as_f32()
        };
        assert!(sample(0).is_nan());
        assert_eq!(sample(1), f32::INFINITY);
        assert_eq!(sample(2), f32::NEG_INFINITY);
    }

    #[test]
    fn unknown_mode_fails_loud() {
        // `whole-project` is now a valid mode (Phase 2), so the negative case is
        // a genuinely unknown mode instead.
        let toml = r#"
mode = "galaxy"
target = "X"
duration_s = 1.0
base_rate_hz = 100.0
"#;
        match Scenario::from_toml_str(toml) {
            Err(EvalError::UnsupportedConstruct { .. }) => {}
            other => panic!("expected UnsupportedConstruct, got {other:?}"),
        }
    }

    #[test]
    fn whole_project_mode_parses_without_target() {
        // Whole-project mode schedules every periodically-triggered function, so
        // it carries no single `target`; the field is optional in this mode.
        let toml = r#"
mode = "whole-project"
duration_s = 1.0
base_rate_hz = 100.0

[[inputs]]
channel = "Root.Demo.Speed"
const = 20.0
"#;
        let sc = Scenario::from_toml_str(toml).expect("whole-project scenario parses");
        assert_eq!(sc.mode, RunMode::WholeProject);
        assert_eq!(sc.duration_s, 1.0);
        assert_eq!(sc.base_rate_hz, 100.0);
        assert_eq!(sc.inputs.len(), 1);
    }

    #[test]
    fn whole_project_mode_parses_without_base_rate() {
        // Whole-project mode may omit `base_rate_hz` entirely: the runner then
        // derives the base tick (lcm of the scheduled rates). The parsed
        // scenario carries 0.0 as the "auto" sentinel.
        let toml = r#"
mode = "whole-project"
duration_s = 0.5
"#;
        let sc = Scenario::from_toml_str(toml).expect("whole-project parses without base rate");
        assert_eq!(sc.mode, RunMode::WholeProject);
        assert_eq!(sc.base_rate_hz, 0.0, "0.0 is the auto-base sentinel");
    }

    #[test]
    fn function_mode_without_base_rate_fails_loud() {
        // Only whole-project may omit the base rate; function/cone modes have no
        // schedule to derive a default from, so omitting it fails loud.
        let toml = r#"
mode = "function"
target = "F"
duration_s = 0.5
"#;
        assert!(
            Scenario::from_toml_str(toml).is_err(),
            "function mode requires an explicit base_rate_hz"
        );
    }

    #[test]
    fn whole_project_mode_ignores_a_supplied_target() {
        // A stray `target` in whole-project mode is harmless (ignored), not an
        // error — the runner schedules every function regardless.
        let toml = r#"
mode = "whole-project"
target = "Root.Demo.Update"
duration_s = 0.5
base_rate_hz = 50.0
"#;
        let sc = Scenario::from_toml_str(toml).expect("whole-project ignores target");
        assert_eq!(sc.mode, RunMode::WholeProject);
    }

    #[test]
    fn function_mode_without_target_fails_loud() {
        // `function`/`cone` modes still require a target; omitting it fails loud
        // rather than silently scheduling nothing.
        let toml = r#"
mode = "function"
duration_s = 1.0
base_rate_hz = 100.0
"#;
        match Scenario::from_toml_str(toml) {
            Err(EvalError::UnsupportedConstruct { .. }) => {}
            other => panic!("expected UnsupportedConstruct, got {other:?}"),
        }
    }

    #[test]
    fn input_with_both_const_and_series_fails_loud() {
        let toml = r#"
mode = "function"
target = "F"
duration_s = 1.0
base_rate_hz = 100.0

[[inputs]]
channel = "X"
const = 1.0
series = [[0.0, 0.0]]
"#;
        assert!(Scenario::from_toml_str(toml).is_err());
    }

    #[test]
    fn non_positive_rate_fails_loud() {
        let toml = r#"
mode = "function"
target = "F"
duration_s = 1.0
base_rate_hz = 0.0
"#;
        assert!(Scenario::from_toml_str(toml).is_err());
    }

    #[test]
    fn parses_io_overrides() {
        let toml = r#"
mode = "whole-project"
duration_s = 1.0

[[io]]
call = "DBC PC.Dash Switches.Receive"
const = true

[[io]]
call = "System.FlashSize"
script = "Clock.Update.m1scr"
offset = 42
series = [[0.0, 4194304], [0.5, 8388608]]
"#;
        let sc = Scenario::from_toml_str(toml).expect("valid scenario");
        assert_eq!(sc.io.len(), 2);

        // The constant override holds its value at every t.
        let receive = sc
            .io
            .iter()
            .find(|o| o.call == "DBC PC.Dash Switches.Receive")
            .unwrap();
        assert_eq!(receive.sample(0.0), Value::Bool(true));
        assert_eq!(receive.sample(0.9), Value::Bool(true));

        // The series override steps by zero-order hold, like an input.
        let flash = sc.io.iter().find(|o| o.call == "System.FlashSize").unwrap();
        assert_eq!(flash.site, Some(CallSite::new("Clock.Update.m1scr", 42)));
        assert_eq!(flash.sample(0.0), Value::m1_integer(4_194_304));
        assert_eq!(flash.sample(0.49), Value::m1_integer(4_194_304));
        assert_eq!(flash.sample(0.5), Value::m1_integer(8_388_608));
    }

    #[test]
    fn io_override_rejects_both_const_and_series() {
        let toml = r#"
mode = "whole-project"
duration_s = 1.0

[[io]]
call = "CanComms.GetFloat"
const = 1.0
series = [[0.0, 2.0]]
"#;
        assert!(Scenario::from_toml_str(toml).is_err());
    }

    #[test]
    fn io_override_rejects_neither_const_nor_series() {
        let toml = r#"
mode = "whole-project"
duration_s = 1.0

[[io]]
call = "CanComms.GetFloat"
"#;
        assert!(Scenario::from_toml_str(toml).is_err());
    }

    #[test]
    fn io_site_selector_requires_script_and_offset_together() {
        for selector in ["script = \"Demo.Update.m1scr\"", "offset = 12"] {
            let toml = format!(
                "mode = \"whole-project\"\nduration_s = 1.0\n\n[[io]]\ncall = \"System.FlashSize\"\n{selector}\nconst = 1\n"
            );
            let error = Scenario::from_toml_str(&toml).unwrap_err();
            assert!(error.to_string().contains("both `script` and `offset`"));
        }
    }

    #[test]
    fn duplicate_io_selectors_fail_instead_of_overwriting() {
        let toml = r#"
mode = "whole-project"
duration_s = 1.0

[[io]]
call = "System.FlashSize"
script = "Demo.Update.m1scr"
offset = 12
const = 1

[[io]]
call = "System.FlashSize"
script = "Demo.Update.m1scr"
offset = 12
const = 2
"#;
        let error = Scenario::from_toml_str(toml).unwrap_err();
        assert!(error.to_string().contains("duplicate io selector"));
    }

    #[test]
    fn io_override_in_json_parses_the_same_shape() {
        let json = r#"{
            "mode": "whole-project",
            "duration_s": 0.5,
            "io": [{ "call": "CanComms.GetFloat", "const": 12.5 }]
        }"#;
        let sc = Scenario::from_json_str(json).expect("valid JSON scenario");
        assert_eq!(sc.io.len(), 1);
        assert_eq!(sc.io[0].call, "CanComms.GetFloat");
        assert_eq!(sc.io[0].sample(0.0), Value::m1_float(12.5));
    }

    #[test]
    fn typed_scenario_values_preserve_all_m1_scalar_kinds() {
        let toml = r#"
mode = "whole-project"
duration_s = 0.1

[[inputs]]
channel = "Signed"
const = { integer = -2147483648 }

[[inputs]]
channel = "Unsigned"
const = { unsigned = 4294967295 }

[[inputs]]
channel = "Float"
const = { floating_point = 0.1 }

[[inputs]]
channel = "Fixed"
const = { fixed_raw = 12345678 }
"#;
        let scenario = Scenario::from_toml_str(toml).unwrap();
        assert_eq!(scenario.inputs[0].sample(0.0), Value::m1_integer(i32::MIN));
        assert_eq!(scenario.inputs[1].sample(0.0), Value::m1_unsigned(u32::MAX));
        assert_eq!(scenario.inputs[2].sample(0.0), Value::m1_float(0.1));
        assert_eq!(
            scenario.inputs[3].sample(0.0),
            Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(
                12_345_678
            )))
        );
    }

    #[test]
    fn initial_state_is_typed_and_kept_separate_from_inputs() {
        let scenario = Scenario::from_toml_str(
            r#"
mode = "whole-project"
duration_s = 0.1

[[initial_state]]
channel = "Counter"
value = { integer = 10 }

[[inputs]]
channel = "Sensor"
const = { unsigned = 20 }
"#,
        )
        .expect("scenario with initial state parses");
        assert_eq!(scenario.initial_state.len(), 1);
        assert_eq!(scenario.initial_state[0].channel, "Counter");
        assert_eq!(scenario.initial_state[0].value, Value::m1_integer(10));
        assert_eq!(scenario.inputs.len(), 1);
        assert_eq!(scenario.inputs[0].sample(0.0), Value::m1_unsigned(20));
    }

    #[test]
    fn bare_scenario_numbers_narrow_or_return_clear_width_errors() {
        let underflow = r#"{
            "mode": "whole-project",
            "duration_s": 0.1,
            "inputs": [{"channel": "Float", "const": 1e-50}]
        }"#;
        let scenario = Scenario::from_json_str(underflow).unwrap();
        assert_eq!(scenario.inputs[0].sample(0.0), Value::m1_float(0.0));

        for document in [
            r#"{"mode":"whole-project","duration_s":0.1,"inputs":[{"channel":"Integer","const":2147483648}]}"#,
            r#"{"mode":"whole-project","duration_s":0.1,"inputs":[{"channel":"Integer","const":9223372036854775808}]}"#,
            r#"{"mode":"whole-project","duration_s":0.1,"inputs":[{"channel":"Float","const":1e39}]}"#,
        ] {
            let error = Scenario::from_json_str(document).unwrap_err();
            assert!(format!("{error}").contains("M1-width"), "{error}");
        }
    }
}
