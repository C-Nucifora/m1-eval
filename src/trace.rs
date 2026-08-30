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
//! Scenario values, adapters, and hardware stubs produce values the engine did
//! not compute. Those call names are flagged in [`Trace::external`].
//! [`Trace::hardware`] adds the resolved receiver, exact call site, and selected
//! route. Deterministic `System` calls have provenance but are not external.
//!
//! Internal channel and expression columns retain their M1 scalar family. The
//! established JSON and CSV formats are untyped compatibility outputs, so they
//! preserve numeric values but do not encode signedness metadata. Serialisation
//! is deterministic, with a `time` column followed by channels in sorted-name
//! order.

use crate::hardware::{HardwareProvenance, ResolvedReceiver};
use crate::value::{M1Scalar, Value};
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
    /// Names whose values are externally driven by a scenario, adapter, default,
    /// or documented hardware fallback rather than computed by the engine.
    /// Channel names still appear in [`Trace::channels`]; hardware call names
    /// have structured detail in [`Trace::hardware`].
    pub external: BTreeSet<String>,
    /// Unseeded inputs substituted with a type-correct startup default under the
    /// scenario's explicit `allow_default_inputs` opt-in, keyed by canonical
    /// channel path. Metadata only (not part of the JSON/CSV trace body): the
    /// honest record of every value the run GUESSED rather than computed.
    pub defaulted: BTreeMap<String, DefaultedInput>,
    /// Structured source records for hardware-backed call sites. A set keeps
    /// repeated ticks compact while retaining every route a site used.
    pub hardware: BTreeSet<HardwareProvenance>,
}

/// One reported default substitution: what value was substituted and which
/// script read it first.
#[derive(Debug, Clone, PartialEq)]
pub struct DefaultedInput {
    /// The substituted (type-correct startup default) value.
    pub value: Value,
    /// The script whose read first triggered the substitution.
    pub first_reader: String,
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

    /// Record a default-substituted input (first reader wins — later reads of
    /// the same channel do not overwrite the original report).
    pub fn mark_defaulted(&mut self, path: impl Into<String>, value: Value, reader: &str) {
        self.defaulted
            .entry(path.into())
            .or_insert_with(|| DefaultedInput {
                value,
                first_reader: reader.to_string(),
            });
    }

    /// Flag a channel or hardware call name as externally driven.
    pub fn mark_external(&mut self, path: impl Into<String>) {
        self.external.insert(path.into());
    }

    /// Whether a channel is flagged externally driven.
    pub fn is_external(&self, path: &str) -> bool {
        self.external.contains(path)
    }

    /// Record how one hardware call obtained its value.
    pub fn record_hardware(&mut self, provenance: HardwareProvenance) {
        self.hardware.insert(provenance);
    }

    /// Serialise the channel columns + time axis to JSON. The shape is
    /// `{ "time": [...], "channels": { path: [...] }, "external": [...],
    /// "hardware": [...] }`,
    /// values rendered by `value_json`. This historical untyped shape cannot
    /// expose M1 scalar-family metadata. JSON has no non-finite number syntax, so
    /// NaN and positive or negative infinity are written as `null`. Deterministic
    /// ordering comes from the `BTreeMap` and `BTreeSet` fields.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\"time\":[");
        out.push_str(&join(self.time.iter().map(|t| f64_json(*t))));
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
        out.push_str("],\"hardware\":[");
        out.push_str(&join(self.hardware.iter().map(hardware_json)));
        out.push_str("]}");
        out
    }

    /// Serialise to CSV: a `time` header column followed by one column per
    /// channel in sorted-name order. This historical untyped shape cannot expose
    /// M1 scalar-family metadata. Rows are ticks; a channel with no value at a
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
            out.push_str(&f64_text(*t));
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

/// Render one structured hardware provenance record.
fn hardware_json(item: &HardwareProvenance) -> String {
    let (receiver_kind, receiver_name) = match &item.receiver {
        ResolvedReceiver::Library { object } => ("library", object.as_str()),
        ResolvedReceiver::Project { path } => ("project", path.as_str()),
        ResolvedReceiver::Unresolved { spelling } => ("unresolved", spelling.as_str()),
    };
    format!(
        "{{\"receiver\":{{\"kind\":{},\"name\":{}}},\"source_call\":{},\"method\":{},\"script\":{},\"offset\":{},\"source\":{}}}",
        json_string(receiver_kind),
        json_string(receiver_name),
        json_string(&item.source_call),
        json_string(&item.method),
        json_string(item.site.script()),
        item.site.offset(),
        json_string(item.source.as_str()),
    )
}

/// Join an iterator of strings with commas.
fn join(items: impl Iterator<Item = String>) -> String {
    items.collect::<Vec<_>>().join(",")
}

/// Format an `f64` without a trailing `.0`-less ambiguity but deterministically.
/// Integers print without a decimal point; others use the shortest round-trip.
fn f64_text(x: f64) -> String {
    if x.is_nan() {
        "NaN".to_string()
    } else if x.is_infinite() {
        if x > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else {
        // `{}` on f64 is the shortest representation that round-trips.
        format!("{x}")
    }
}

/// Render an `f64` as a JSON number, or `null` when JSON cannot represent it.
fn f64_json(value: f64) -> String {
    if value.is_finite() {
        value.to_string()
    } else {
        "null".to_string()
    }
}

/// Render an M1-width scalar without widening binary32 before formatting.
fn m1_scalar_text(value: M1Scalar) -> String {
    match value {
        M1Scalar::FloatingPoint(value) if value.is_nan() => "NaN".to_string(),
        M1Scalar::FloatingPoint(value) if value.is_infinite() => {
            if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
        }
        M1Scalar::FloatingPoint(value) => value.to_string(),
        M1Scalar::Integer(value) => value.to_string(),
        M1Scalar::UnsignedInteger(value) => value.to_string(),
        M1Scalar::FixedPoint7dps(value) => value.to_string(),
    }
}

/// Render an M1 scalar as JSON. JSON cannot represent non-finite binary32 values.
fn m1_scalar_json(value: M1Scalar) -> String {
    match value {
        M1Scalar::FloatingPoint(value) if !value.is_finite() => "null".to_string(),
        _ => m1_scalar_text(value),
    }
}

/// Render a [`Value`] as a JSON scalar. Numbers are bare; booleans `true`/`false`;
/// enums and strings are JSON strings.
fn value_json(v: &Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        Value::M1(value) => m1_scalar_json(*value),
        Value::Enum { member, .. } => json_string(member),
        Value::Str(s) => json_string(s),
    }
}

/// Render a [`Value`] as a plain CSV cell (no quoting here — [`csv_field`] quotes).
fn value_csv(v: &Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        Value::M1(value) => m1_scalar_text(*value),
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
        tr.record_channel("Root.Demo.Out", Value::m1_float(1.0));
        tr.push_tick(0.1);
        tr.record_channel("Root.Demo.Out", Value::m1_float(2.0));

        assert_eq!(tr.time, vec![0.0, 0.1]);
        assert_eq!(
            tr.channels.get("Root.Demo.Out").unwrap(),
            &vec![Value::m1_float(1.0), Value::m1_float(2.0)]
        );
        // Column length tracks the time axis.
        assert_eq!(tr.channels["Root.Demo.Out"].len(), tr.time.len());
    }

    #[test]
    fn per_expression_sink_keys_on_site() {
        let mut tr = Trace::new();
        let site = ("Demo.Update.m1scr".to_string(), 42);
        tr.push_tick(0.0);
        tr.record_expr(site.clone(), Value::m1_integer(7));
        tr.push_tick(0.1);
        tr.record_expr(site.clone(), Value::m1_integer(8));
        assert_eq!(
            tr.exprs[&site],
            vec![Value::m1_integer(7), Value::m1_integer(8)]
        );
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
        tr.record_channel("Out", Value::m1_float(1.0));
        tr.push_tick(0.1);
        tr.record_channel("Out", Value::m1_float(2.0));

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
        tr.record_channel("B", Value::m1_integer(2));
        tr.record_channel("A", Value::Bool(true));
        tr.mark_external("A");
        let json = tr.to_json();
        // BTreeMap ordering: A before B regardless of insertion order.
        assert_eq!(
            json,
            "{\"time\":[0],\"channels\":{\"A\":[true],\"B\":[2]},\"external\":[\"A\"],\"hardware\":[]}"
        );
    }

    #[test]
    fn m1_width_scalars_serialize_without_host_width_artifacts() {
        let mut tr = Trace::new();
        tr.push_tick(0.0);
        tr.record_channel("Float", Value::M1(M1Scalar::FloatingPoint(0.1)));
        tr.record_channel(
            "Fixed",
            Value::M1(M1Scalar::FixedPoint7dps(
                crate::value::FixedPoint7dps::from_raw(1_000_001),
            )),
        );
        tr.record_channel("Int", Value::M1(M1Scalar::Integer(i32::MIN)));
        tr.record_channel("Uint", Value::M1(M1Scalar::UnsignedInteger(u32::MAX)));

        assert_eq!(
            tr.to_json(),
            "{\"time\":[0],\"channels\":{\"Fixed\":[0.1000001],\"Float\":[0.1],\"Int\":[-2147483648],\"Uint\":[4294967295]},\"external\":[],\"hardware\":[]}"
        );
    }

    #[test]
    fn non_finite_floats_serialize_as_valid_json_nulls() {
        let mut tr = Trace::new();
        for (time, m1) in [
            (f64::NAN, f32::NAN),
            (f64::INFINITY, f32::INFINITY),
            (f64::NEG_INFINITY, f32::NEG_INFINITY),
        ] {
            tr.push_tick(time);
            tr.record_channel("M1", Value::m1_float(m1));
        }

        let json = tr.to_json();
        assert_eq!(
            json,
            "{\"time\":[null,null,null],\"channels\":{\"M1\":[null,null,null]},\"external\":[],\"hardware\":[]}"
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("trace output must be valid JSON");
        assert_eq!(parsed["time"], serde_json::json!([null, null, null]));
        assert_eq!(
            parsed["channels"]["M1"],
            serde_json::json!([null, null, null])
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
