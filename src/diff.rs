// SPDX-License-Identifier: GPL-3.0-or-later
//! Counterfactual diff: the per-channel delta between a counterfactual run and the
//! logged ground truth.
//!
//! A counterfactual run ([`crate::runner::run_counterfactual`]) replays a [`Log`]
//! as ground truth, applies an override, and recomputes only the downstream cone —
//! producing a [`Trace`]. To see *what the override actually changed*, [`Diff`]
//! lines that trace up against the log over the trace's own time grid and subtracts.
//!
//! For each numeric channel present in BOTH the trace and the log, the log is
//! resampled onto `trace.time` (zero-order hold, via [`InputSeries::sample`]) and
//! `delta = counterfactual - logged` is computed per tick. A channel is `changed`
//! when its maximum absolute delta exceeds `eps` — so the headline question
//! ("which channels did overriding this sensor move, and by how much?") is answered
//! by [`Diff::changed_channels`].
//!
//! Channels that are not numeric (boolean/enum/string columns), or that the log
//! does not carry, are skipped: a diff is a numeric comparison against recorded
//! truth, and a channel with no logged baseline has nothing to compare against.

use crate::log::Log;
use crate::scenario::InputSeries;
use crate::trace::Trace;
use crate::value::Value;
use std::collections::BTreeMap;

/// The default change threshold: a channel whose maximum absolute delta is at or
/// below this is treated as unchanged. Chosen well below any physically meaningful
/// signal change so the identity (no-op) override reports an empty change set.
pub const DEFAULT_EPS: f64 = 1e-9;

/// One channel's counterfactual-vs-logged comparison over a shared time grid.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelDiff {
    /// The logged (ground-truth) value at each tick, resampled onto the grid.
    pub logged: Vec<f64>,
    /// The counterfactual value at each tick (from the trace).
    pub counterfactual: Vec<f64>,
    /// `counterfactual - logged` at each tick.
    pub delta: Vec<f64>,
    /// The maximum absolute delta over the grid (0.0 for an empty grid).
    pub max_abs_delta: f64,
    /// Whether `max_abs_delta` exceeds the diff's `eps`.
    pub changed: bool,
}

/// The full result of a counterfactual run: the recomputed [`Trace`] plus the
/// per-channel [`Diff`] of that trace against the logged ground truth.
#[derive(Debug, Clone)]
pub struct Counterfactual {
    /// The counterfactual trace (logged channels held, the override cone recomputed).
    pub trace: Trace,
    /// The per-channel delta of the trace against the log.
    pub diff: Diff,
}

/// A counterfactual-vs-log diff: one [`ChannelDiff`] per numeric channel the trace
/// and the log share, over the trace's time grid.
#[derive(Debug, Clone, PartialEq)]
pub struct Diff {
    /// The shared time axis (the counterfactual trace's grid).
    pub time: Vec<f64>,
    /// Per-channel deltas, keyed by the trace's canonical channel path (sorted).
    pub channels: BTreeMap<String, ChannelDiff>,
    /// The change threshold used to set each channel's `changed` flag.
    pub eps: f64,
}

impl Diff {
    /// Compare a counterfactual `trace` against the logged ground truth `log`, with
    /// the [`DEFAULT_EPS`] change threshold. See [`Diff::between_eps`].
    pub fn between(log: &Log, trace: &Trace) -> Diff {
        Diff::between_eps(log, trace, DEFAULT_EPS)
    }

    /// Compare a counterfactual `trace` against the logged ground truth `log` over
    /// `trace.time`, flagging a channel `changed` when its maximum absolute delta
    /// exceeds `eps`.
    ///
    /// A channel is included only when it appears in the trace as a fully-numeric
    /// column AND a matching log series exists (matched by exact path, the
    /// `Root.`-stripped path, or the bare leaf name — mirroring how the runner
    /// canonicalises log inputs). Other channels are skipped.
    pub fn between_eps(log: &Log, trace: &Trace, eps: f64) -> Diff {
        let time = trace.time.clone();
        let mut channels = BTreeMap::new();

        for (path, col) in &trace.channels {
            let Some(series) = match_log_series(log, path) else {
                continue;
            };
            let Some(counterfactual) = column_as_f64(col) else {
                continue;
            };
            let logged: Vec<f64> = time.iter().map(|&t| sample_f64(series, t)).collect();

            let n = logged.len().min(counterfactual.len());
            let delta: Vec<f64> = (0..n).map(|i| counterfactual[i] - logged[i]).collect();
            let max_abs_delta = delta
                .iter()
                .copied()
                .map(f64::abs)
                .filter(|d| d.is_finite())
                .fold(0.0_f64, f64::max);
            let changed = max_abs_delta > eps;

            channels.insert(
                path.clone(),
                ChannelDiff {
                    logged,
                    counterfactual,
                    delta,
                    max_abs_delta,
                    changed,
                },
            );
        }

        Diff {
            time,
            channels,
            eps,
        }
    }

    /// The channel paths whose counterfactual value diverged from the log by more
    /// than `eps` (sorted). Empty for a no-op override — the load-bearing invariant.
    pub fn changed_channels(&self) -> Vec<&str> {
        self.channels
            .iter()
            .filter(|(_, d)| d.changed)
            .map(|(k, _)| k.as_str())
            .collect()
    }

    /// Render the diff as JSON: `{eps, time, channels:{path:{max_abs_delta, changed,
    /// logged, counterfactual, delta}}}`. Deterministic (channels are sorted).
    pub fn to_json(&self) -> String {
        let mut s = String::from("{\"eps\":");
        s.push_str(&num_json(self.eps));
        s.push_str(",\"time\":");
        push_array(&mut s, &self.time);
        s.push_str(",\"channels\":{");
        for (i, (path, d)) in self.channels.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&json_string(path));
            s.push_str(":{\"max_abs_delta\":");
            s.push_str(&num_json(d.max_abs_delta));
            s.push_str(",\"changed\":");
            s.push_str(if d.changed { "true" } else { "false" });
            s.push_str(",\"logged\":");
            push_array(&mut s, &d.logged);
            s.push_str(",\"counterfactual\":");
            push_array(&mut s, &d.counterfactual);
            s.push_str(",\"delta\":");
            push_array(&mut s, &d.delta);
            s.push('}');
        }
        s.push_str("}}");
        s
    }

    /// Render the diff as CSV: a `time` column then, per channel, three columns
    /// `<path> logged`, `<path> cf`, `<path> delta`. Channels are sorted.
    pub fn to_csv(&self) -> String {
        let paths: Vec<&String> = self.channels.keys().collect();
        let mut out = String::from("time");
        for p in &paths {
            out.push(',');
            out.push_str(&csv_field(&format!("{p} logged")));
            out.push(',');
            out.push_str(&csv_field(&format!("{p} cf")));
            out.push(',');
            out.push_str(&csv_field(&format!("{p} delta")));
        }
        out.push('\n');
        for (i, t) in self.time.iter().enumerate() {
            out.push_str(&num_csv(*t));
            for p in &paths {
                let d = &self.channels[*p];
                out.push(',');
                out.push_str(&num_csv(*d.logged.get(i).unwrap_or(&f64::NAN)));
                out.push(',');
                out.push_str(&num_csv(*d.counterfactual.get(i).unwrap_or(&f64::NAN)));
                out.push(',');
                out.push_str(&num_csv(*d.delta.get(i).unwrap_or(&f64::NAN)));
            }
            out.push('\n');
        }
        out
    }
}

/// Find the log series matching a trace channel path: exact, then the `Root.`-
/// stripped path, then the bare leaf name (logs commonly omit the implicit `Root.`
/// group prefix the symbol table uses).
fn match_log_series<'a>(log: &'a Log, path: &str) -> Option<&'a InputSeries> {
    if let Some(s) = log.series_for(path) {
        return Some(s);
    }
    if let Some(stripped) = path.strip_prefix("Root.")
        && let Some(s) = log.series_for(stripped)
    {
        return Some(s);
    }
    let leaf = path.rsplit('.').next().unwrap_or(path);
    log.series_for(leaf)
}

/// A whole column as `f64`, or `None` if any cell is non-numeric (bool/enum/string)
/// — such channels are not part of a numeric diff.
fn column_as_f64(col: &[Value]) -> Option<Vec<f64>> {
    let mut out = Vec::with_capacity(col.len());
    for v in col {
        out.push(v.as_f64().ok()?);
    }
    Some(out)
}

/// Sample a log series at `t` as `f64`; a non-numeric sample becomes `NaN` (it is
/// filtered out of `max_abs_delta` so it never spuriously flags a change).
fn sample_f64(series: &InputSeries, t: f64) -> f64 {
    series.sample(t).as_f64().unwrap_or(f64::NAN)
}

/// Format an `f64` for JSON: a finite number, or `null` for NaN/±inf (JSON has no
/// non-finite numerics).
fn num_json(x: f64) -> String {
    if x.is_finite() {
        let mut s = format!("{x}");
        if !s.contains('.') && !s.contains('e') && !s.contains('E') {
            s.push_str(".0");
        }
        s
    } else {
        "null".to_string()
    }
}

/// Format an `f64` for CSV: a finite number, or an empty field for NaN/±inf.
fn num_csv(x: f64) -> String {
    if x.is_finite() {
        format!("{x}")
    } else {
        String::new()
    }
}

fn push_array(s: &mut String, xs: &[f64]) {
    s.push('[');
    for (i, x) in xs.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&num_json(*x));
    }
    s.push(']');
}

/// Minimal JSON string escaping (quotes, backslashes, control chars).
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Quote a CSV field if it contains a comma, quote, or newline.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        let escaped = s.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::InputKind;

    /// A log with two constant channels (Sensor=10, Mid=25) over [0, 1] s.
    fn two_channel_log() -> Log {
        Log {
            channels: vec![
                InputSeries {
                    channel: "Root.CF.Sensor".to_string(),
                    kind: InputKind::Series(vec![
                        (0.0, Value::Float(10.0)),
                        (1.0, Value::Float(10.0)),
                    ]),
                },
                InputSeries {
                    channel: "Root.CF.Mid".to_string(),
                    kind: InputKind::Series(vec![
                        (0.0, Value::Float(25.0)),
                        (1.0, Value::Float(25.0)),
                    ]),
                },
            ],
            meta: crate::log::LogMeta::default(),
        }
    }

    fn trace_with(cols: &[(&str, &[f64])], time: &[f64]) -> Trace {
        let mut t = Trace::new();
        for &ti in time {
            t.push_tick(ti);
        }
        for (path, vals) in cols {
            t.channels.insert(
                (*path).to_string(),
                vals.iter().map(|v| Value::Float(*v)).collect(),
            );
        }
        t
    }

    #[test]
    fn identity_trace_has_no_changed_channels() {
        // The no-op invariant at the unit level: a counterfactual trace equal to the
        // logged values produces an empty change set.
        let log = two_channel_log();
        let trace = trace_with(
            &[
                ("Root.CF.Sensor", &[10.0, 10.0]),
                ("Root.CF.Mid", &[25.0, 25.0]),
            ],
            &[0.0, 1.0],
        );
        let diff = Diff::between(&log, &trace);
        assert!(
            diff.changed_channels().is_empty(),
            "no-op must not flag changes"
        );
        assert_eq!(diff.channels["Root.CF.Mid"].max_abs_delta, 0.0);
        assert_eq!(diff.channels["Root.CF.Mid"].delta, vec![0.0, 0.0]);
    }

    #[test]
    fn divergent_channel_is_flagged_changed() {
        // Mid moved by +5 under the override; Sensor stayed at its logged value.
        let log = two_channel_log();
        let trace = trace_with(
            &[
                ("Root.CF.Sensor", &[10.0, 10.0]),
                ("Root.CF.Mid", &[30.0, 30.0]),
            ],
            &[0.0, 1.0],
        );
        let diff = Diff::between(&log, &trace);
        assert_eq!(diff.changed_channels(), vec!["Root.CF.Mid"]);
        let mid = &diff.channels["Root.CF.Mid"];
        assert!(mid.changed);
        assert_eq!(mid.delta, vec![5.0, 5.0]);
        assert_eq!(mid.max_abs_delta, 5.0);
        // Sensor is unchanged.
        assert!(!diff.channels["Root.CF.Sensor"].changed);
    }

    #[test]
    fn matches_log_by_root_stripped_and_leaf_name() {
        // The log wrote the bare leaf `Sensor`; the trace key is the canonical path.
        let log = Log {
            channels: vec![InputSeries {
                channel: "Sensor".to_string(),
                kind: InputKind::Series(vec![(0.0, Value::Float(1.0))]),
            }],
            meta: crate::log::LogMeta::default(),
        };
        let trace = trace_with(&[("Root.CF.Sensor", &[3.0])], &[0.0]);
        let diff = Diff::between(&log, &trace);
        assert_eq!(diff.channels["Root.CF.Sensor"].delta, vec![2.0]);
    }

    #[test]
    fn channel_absent_from_log_is_skipped() {
        // A purely-computed channel with no logged baseline is not in the diff.
        let log = two_channel_log();
        let trace = trace_with(&[("Root.CF.Computed", &[1.0, 2.0])], &[0.0, 1.0]);
        let diff = Diff::between(&log, &trace);
        assert!(!diff.channels.contains_key("Root.CF.Computed"));
    }

    #[test]
    fn non_numeric_column_is_skipped() {
        // A boolean column has no numeric diff; it is skipped, not an error.
        let log = two_channel_log();
        let mut trace = Trace::new();
        trace.push_tick(0.0);
        trace
            .channels
            .insert("Root.CF.Sensor".to_string(), vec![Value::Bool(true)]);
        let diff = Diff::between(&log, &trace);
        assert!(!diff.channels.contains_key("Root.CF.Sensor"));
    }

    #[test]
    fn eps_threshold_controls_changed_flag() {
        // A tiny delta below eps is not "changed".
        let log = two_channel_log();
        let trace = trace_with(&[("Root.CF.Mid", &[25.0 + 1e-12, 25.0])], &[0.0, 1.0]);
        let diff = Diff::between(&log, &trace);
        assert!(!diff.channels["Root.CF.Mid"].changed, "1e-12 < DEFAULT_EPS");
    }

    #[test]
    fn json_and_csv_render_deterministically() {
        let log = two_channel_log();
        let trace = trace_with(
            &[("Root.CF.Sensor", &[10.0]), ("Root.CF.Mid", &[30.0])],
            &[0.0],
        );
        let diff = Diff::between(&log, &trace);
        // JSON is stable and mentions both channels (sorted: Mid before Sensor).
        let json = diff.to_json();
        assert_eq!(json, diff.to_json());
        let mid = json.find("Root.CF.Mid").unwrap();
        let sensor = json.find("Root.CF.Sensor").unwrap();
        assert!(mid < sensor, "channels are sorted in the JSON");
        // CSV has a header row plus one data row.
        let csv = diff.to_csv();
        assert_eq!(csv.lines().count(), 2);
        assert!(csv.starts_with("time,"));
    }
}
