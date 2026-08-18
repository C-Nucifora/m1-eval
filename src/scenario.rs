// SPDX-License-Identifier: GPL-3.0-or-later
//! The [`Scenario`]: the user-authored description of *how to drive a run*.
//!
//! A scenario chooses the run mode (which runner, against which function or
//! target channel), the time grid (`duration_s` + `base_rate_hz`), the input
//! sources for the channels the engine does not itself compute (constants or
//! piecewise time series), and any channel overrides that pin a value over the
//! top of everything else.
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
//! ```
//!
//! ## Time-series resampling
//!
//! A `series` is a list of `(t, value)` keyframes. At a tick instant `t` the
//! engine samples the series by *holding* the most recent keyframe at or before
//! `t` (zero-order hold / step), which is deterministic and avoids inventing
//! values between samples. Before the first keyframe the first value is held.
//! Numeric keyframes are stored as [`Value::Float`]; an [`InputSeries`] of kind
//! [`InputKind::Const`] holds a single value for every tick.
//!
//! Identifiers may contain spaces (`Cooling Fan.Output`); channel names are used
//! verbatim as canonical-ish paths and never split on whitespace.

use crate::error::EvalError;
use crate::value::Value;
use serde::Deserialize;

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
    /// Run *every* periodically-scheduled function (those whose trigger resolves
    /// to a `call_rate_hz`) each base tick at its own rate, in dependency-then-rate
    /// order. The faithful mini-ECU: no single target — the runner schedules the
    /// whole project.
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

/// A constant value or a `(t, value)` time series.
#[derive(Debug, Clone, PartialEq)]
pub enum InputKind {
    /// One value held for the whole run.
    Const(Value),
    /// `(t_seconds, value)` keyframes, ascending in `t`. Sampled by zero-order
    /// hold at each tick.
    Series(Vec<(f64, Value)>),
}

impl InputSeries {
    /// Sample this input at tick time `t` (seconds). A constant returns its value
    /// at every `t`; a series returns the most recent keyframe value at or before
    /// `t` (zero-order hold), or the first keyframe before the series begins.
    pub fn sample(&self, t: f64) -> Value {
        match &self.kind {
            InputKind::Const(v) => v.clone(),
            InputKind::Series(points) => sample_series(points, t),
        }
    }
}

/// Zero-order-hold sample of an ascending `(t, value)` keyframe series at `t`.
/// Holds the first value before the series starts and the last value after it
/// ends. An empty series is a programming error upstream; we return `Float(0.0)`
/// only as a last resort, but the parser rejects empty series so this is unreached
/// in practice.
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
            .unwrap_or(Value::Float(0.0)),
    }
}

/// The fully-parsed scenario: run mode, time grid, inputs, and overrides.
#[derive(Debug, Clone, PartialEq)]
pub struct Scenario {
    /// Which runner and target.
    pub mode: RunMode,
    /// Externally-driven input sources (constants + series).
    pub inputs: Vec<InputSeries>,
    /// Total run duration in seconds. Ticks span `[0, duration_s)`.
    pub duration_s: f64,
    /// Base tick rate in Hz; the tick step is `dt = 1 / base_rate_hz`.
    pub base_rate_hz: f64,
    /// Channels pinned to a constant or series, layered *over* the inputs and
    /// any computed value. Same shape as [`Scenario::inputs`].
    pub overrides: Vec<InputSeries>,
    /// Whole-project mode only: substitute type-correct startup defaults for
    /// unseeded channel reads (each substitution is reported on the trace)
    /// instead of failing loud. **Off by default** — strict fail-loud is the
    /// baseline; defaulting is an explicit, visible opt-in.
    pub allow_default_inputs: bool,
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
/// [`Value::Float`], empty cells add no keyframe (zero-order hold keeps the prior
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
            let v = trimmed
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| EvalError::TypeError {
                    detail: format!(
                        "CSV row {} column {:?} value {trimmed:?} is not numeric",
                        row_idx + 2,
                        acc.0
                    ),
                })?;
            acc.1.push((t, v));
        }
    }

    Ok(ParsedTimeSeriesCsv { columns, units })
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
    inputs: Vec<RawInput>,
    #[serde(default)]
    overrides: Vec<RawInput>,
    /// Opt-in unseeded-channel defaulting for whole-project mode (strict
    /// fail-loud when absent/false).
    #[serde(default)]
    allow_default_inputs: bool,
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

/// A raw scalar value from the wire: a number, boolean, or string. TOML/JSON
/// numbers come through as either integer or float; we normalise to a [`Value`].
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

impl RawValue {
    fn into_value(self) -> Value {
        match self {
            RawValue::Bool(b) => Value::Bool(b),
            RawValue::Int(i) => Value::Int(i),
            RawValue::Float(f) => Value::Float(f),
            RawValue::Str(s) => Value::Str(s),
        }
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
        let overrides = self
            .overrides
            .into_iter()
            .map(RawInput::into_input)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Scenario {
            mode,
            inputs,
            duration_s: self.duration_s,
            base_rate_hz: self.base_rate_hz,
            overrides,
            allow_default_inputs: self.allow_default_inputs,
        })
    }
}

impl RawInput {
    fn into_input(self) -> Result<InputSeries, EvalError> {
        let kind = match (self.constant, self.series) {
            (Some(c), None) => InputKind::Const(c.into_value()),
            (None, Some(points)) => {
                if points.is_empty() {
                    return Err(EvalError::UnsupportedConstruct {
                        kind: format!("input {:?} has an empty series", self.channel),
                        at: 0,
                    });
                }
                InputKind::Series(
                    points
                        .into_iter()
                        .map(|(t, v)| (t, v.into_value()))
                        .collect(),
                )
            }
            (Some(_), Some(_)) => {
                return Err(EvalError::UnsupportedConstruct {
                    kind: format!(
                        "input {:?} sets both `const` and `series` (choose one)",
                        self.channel
                    ),
                    at: 0,
                });
            }
            (None, None) => {
                return Err(EvalError::UnsupportedConstruct {
                    kind: format!("input {:?} sets neither `const` nor `series`", self.channel),
                    at: 0,
                });
            }
        };
        Ok(InputSeries {
            channel: self.channel,
            kind,
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
        assert_eq!(gain.kind, InputKind::Const(Value::Float(2.5)));

        // The series input, sampled by zero-order hold.
        let speed = sc
            .inputs
            .iter()
            .find(|i| i.channel == "Root.Demo.Speed")
            .unwrap();
        // Before/at first keyframe -> 0.0.
        assert_eq!(speed.sample(0.0), Value::Float(0.0));
        assert_eq!(speed.sample(0.4), Value::Float(0.0));
        // At/after the second keyframe -> 50.0.
        assert_eq!(speed.sample(0.5), Value::Float(50.0));
        assert_eq!(speed.sample(0.99), Value::Float(50.0));
    }

    #[test]
    fn const_samples_constant_at_every_tick() {
        let i = InputSeries {
            channel: "X".to_string(),
            kind: InputKind::Const(Value::Int(7)),
        };
        assert_eq!(i.sample(0.0), Value::Int(7));
        assert_eq!(i.sample(123.4), Value::Int(7));
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
        assert_eq!(sc.inputs[0].kind, InputKind::Const(Value::Int(10)));
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
        assert_eq!(speed.sample(0.6), Value::Float(80.0));

        // The new channel was added.
        let brake = sc
            .inputs
            .iter()
            .find(|i| i.channel == "Root.Demo.Brake")
            .expect("brake added from CSV");
        assert_eq!(brake.sample(0.0), Value::Float(1.0));
        assert_eq!(brake.sample(0.5), Value::Float(0.0));
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
}
