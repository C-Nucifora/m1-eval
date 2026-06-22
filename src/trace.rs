// SPDX-License-Identifier: GPL-3.0-or-later
//! The [`Trace`]: the evaluator's output — channel value columns over a shared
//! time axis, plus a per-expression value sink for introspection.
//!
//! A `Trace` is column-oriented. One shared [`Trace::time`] axis (`Vec<f64>`)
//! gives the tick instants; each channel and each recorded expression keeps a
//! `Vec<Value>` aligned to that axis. The runner calls [`Trace::push_tick`] once
//! per tick to extend the time axis, then [`Trace::record_channel`] /
//! [`Trace::record_expr`] for every value produced during that tick.
//!
//! ## Per-expression sink
//!
//! Beyond channel columns, the engine records the value of individual
//! expressions keyed by their `CallSite`-style identity `(script, byte_offset)`
//! (the visualiser/LSP overlay needs per-node values). The expr evaluator pushes
//! into [`Trace::exprs`] when a sink is active.
//!
//! ## Externally-driven channels
//!
//! Tier-3 IO stubs (M6 Task 19) produce values the engine cannot truly compute —
//! they come from the scenario or a documented stub. Those channels are flagged
//! in [`Trace::external`] so a consumer knows which columns are simulated input
//! rather than evaluated output.
//!
//! Serialisation is deterministic: JSON via `serde_json`, and a CSV with a
//! `time` column followed by one column per channel in sorted-name order so the
//! output is reproducible across runs.

use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};

/// A column-oriented record of an evaluation run.
#[derive(Debug, Clone, Default)]
pub struct Trace {
    /// The shared tick time axis, in seconds. One entry per tick.
    pub time: Vec<f64>,
    /// Channel value columns, keyed by canonical path. Each column is aligned to
    /// [`Trace::time`]. A `BTreeMap` keeps channel order deterministic.
    pub channels: BTreeMap<String, Vec<Value>>,
    /// Per-expression value columns, keyed by `(script_name, byte_offset)`. Used
    /// by the value overlay; sparse (only expressions the sink recorded appear).
    pub exprs: BTreeMap<(String, usize), Vec<Value>>,
    /// Channels whose values are externally driven (scenario-fed or a documented
    /// Tier-3 stub) rather than computed by the engine. Metadata only — these
    /// channels still appear in [`Trace::channels`].
    pub external: BTreeSet<String>,
}

impl Trace {
    /// An empty trace.
    pub fn new() -> Trace {
        Trace::default()
    }

    /// Begin a new tick at time `t`: extend the time axis. Channel/expression
    /// columns are filled by the `record_*` calls that follow for this tick.
    pub fn push_tick(&mut self, t: f64) {
        self.time.push(t);
    }

    /// Record a channel value for the current (most recent) tick. A channel seen
    /// for the first time mid-run is back-filled so its column stays aligned to
    /// the time axis: earlier ticks get no entry, so we left-pad nothing and
    /// simply append; callers that need dense columns record every tick.
    pub fn record_channel(&mut self, path: impl Into<String>, value: Value) {
        self.channels.entry(path.into()).or_default().push(value);
    }

    /// Record the value of one expression occurrence (keyed by its
    /// `(script, byte_offset)` identity) for the current tick.
    pub fn record_expr(&mut self, site: (String, usize), value: Value) {
        self.exprs.entry(site).or_default().push(value);
    }

    /// Flag a channel as externally driven (scenario-fed or a Tier-3 stub).
    pub fn mark_external(&mut self, path: impl Into<String>) {
        self.external.insert(path.into());
    }

    /// Whether a channel is flagged externally driven.
    pub fn is_external(&self, path: &str) -> bool {
        self.external.contains(path)
    }

    /// Serialise the channel columns + time axis to JSON. The shape is
    /// `{ "time": [...], "channels": { path: [...] }, "external": [...] }`,
    /// values rendered by `value_json`. Deterministic ordering (BTree maps).
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\"time\":[");
        out.push_str(&join(self.time.iter().map(|t| fmt_f64(*t))));
        out.push_str("],\"channels\":{");
        let mut first = true;
        for (path, col) in &self.channels {
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(&json_string(path));
            out.push(':');
            out.push('[');
            out.push_str(&join(col.iter().map(value_json)));
            out.push(']');
        }
        out.push_str("},\"external\":[");
        out.push_str(&join(self.external.iter().map(|p| json_string(p))));
        out.push_str("]}");
        out
    }

    /// Serialise to CSV: a `time` header column followed by one column per
    /// channel in sorted-name order. Rows are ticks; a channel with no value at a
    /// given tick leaves an empty cell so columns stay aligned to the time axis.
    pub fn to_csv(&self) -> String {
        let paths: Vec<&String> = self.channels.keys().collect();
        let mut out = String::from("time");
        for p in &paths {
            out.push(',');
            out.push_str(&csv_field(p));
        }
        out.push('\n');
        for (i, t) in self.time.iter().enumerate() {
            out.push_str(&fmt_f64(*t));
            for p in &paths {
                out.push(',');
                if let Some(v) = self.channels.get(*p).and_then(|c| c.get(i)) {
                    out.push_str(&csv_field(&value_csv(v)));
                }
            }
            out.push('\n');
        }
        out
    }
}

/// Join an iterator of strings with commas.
fn join(items: impl Iterator<Item = String>) -> String {
    items.collect::<Vec<_>>().join(",")
}

/// Format an `f64` without a trailing `.0`-less ambiguity but deterministically.
/// Integers print without a decimal point; others use the shortest round-trip.
fn fmt_f64(x: f64) -> String {
    if x.is_nan() {
        "NaN".to_string()
    } else if x.is_infinite() {
        if x > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else {
        // `{}` on f64 is the shortest representation that round-trips.
        format!("{x}")
    }
}

/// Render a [`Value`] as a JSON scalar. Numbers are bare; booleans `true`/`false`;
/// enums and strings are JSON strings.
fn value_json(v: &Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        Value::Int(x) => x.to_string(),
        Value::Uint(x) => x.to_string(),
        Value::Float(x) => fmt_f64(*x),
        Value::Enum { member, .. } => json_string(member),
        Value::Str(s) => json_string(s),
    }
}

/// Render a [`Value`] as a plain CSV cell (no quoting here — [`csv_field`] quotes).
fn value_csv(v: &Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        Value::Int(x) => x.to_string(),
        Value::Uint(x) => x.to_string(),
        Value::Float(x) => fmt_f64(*x),
        Value::Enum { member, .. } => member.clone(),
        Value::Str(s) => s.clone(),
    }
}

/// Quote a JSON string with the minimal escapes we need (quote and backslash).
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Quote a CSV field if it contains a comma, quote, or newline (RFC-4180 style).
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_ticks_and_record_channels_align_to_time() {
        let mut tr = Trace::new();
        tr.push_tick(0.0);
        tr.record_channel("Root.Demo.Out", Value::Float(1.0));
        tr.push_tick(0.1);
        tr.record_channel("Root.Demo.Out", Value::Float(2.0));

        assert_eq!(tr.time, vec![0.0, 0.1]);
        assert_eq!(
            tr.channels.get("Root.Demo.Out").unwrap(),
            &vec![Value::Float(1.0), Value::Float(2.0)]
        );
        // Column length tracks the time axis.
        assert_eq!(tr.channels["Root.Demo.Out"].len(), tr.time.len());
    }

    #[test]
    fn per_expression_sink_keys_on_site() {
        let mut tr = Trace::new();
        let site = ("Demo.Update.m1scr".to_string(), 42);
        tr.push_tick(0.0);
        tr.record_expr(site.clone(), Value::Int(7));
        tr.push_tick(0.1);
        tr.record_expr(site.clone(), Value::Int(8));
        assert_eq!(tr.exprs[&site], vec![Value::Int(7), Value::Int(8)]);
    }

    #[test]
    fn external_flag_round_trips() {
        let mut tr = Trace::new();
        tr.mark_external("Root.Demo.CanIn");
        assert!(tr.is_external("Root.Demo.CanIn"));
        assert!(!tr.is_external("Root.Demo.Out"));
    }

    #[test]
    fn to_csv_shape_has_header_and_rows() {
        let mut tr = Trace::new();
        tr.push_tick(0.0);
        tr.record_channel("Out", Value::Float(1.0));
        tr.push_tick(0.1);
        tr.record_channel("Out", Value::Float(2.0));

        let csv = tr.to_csv();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "time,Out");
        assert_eq!(lines[1], "0,1");
        assert_eq!(lines[2], "0.1,2");
        // Header + two data rows.
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn to_json_is_deterministic_and_well_formed() {
        let mut tr = Trace::new();
        tr.push_tick(0.0);
        tr.record_channel("B", Value::Int(2));
        tr.record_channel("A", Value::Bool(true));
        tr.mark_external("A");
        let json = tr.to_json();
        // BTreeMap ordering: A before B regardless of insertion order.
        assert_eq!(
            json,
            "{\"time\":[0],\"channels\":{\"A\":[true],\"B\":[2]},\"external\":[\"A\"]}"
        );
    }

    #[test]
    fn csv_quotes_fields_with_commas() {
        let mut tr = Trace::new();
        tr.push_tick(0.0);
        tr.record_channel("Root.A,B", Value::Str("x,y".to_string()));
        let csv = tr.to_csv();
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "time,\"Root.A,B\"");
        assert_eq!(lines[1], "0,\"x,y\"");
    }
}
