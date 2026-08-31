// SPDX-License-Identifier: GPL-3.0-or-later
//! The [`Trace`]: the evaluator's output — channel value columns over a shared
//! time axis, plus a per-expression value sink for introspection.
//!
//! A `Trace` is column-oriented. One shared [`Trace::time`] axis (`Vec<f64>`)
//! gives the tick instants; channel and expression values use compatibility
//! `Vec<Value>` columns. The runner calls [`Trace::push_tick`] once per tick to
//! extend the time axis, then [`Trace::record_channel`] /
//! [`Trace::record_expr`] for every value produced during that tick. Channel
//! records also retain an internal tick index and value provenance so consumers
//! that require exact alignment do not have to infer it from column length.
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
//! Scenario inputs, held initial state, adapters, and hardware stubs produce
//! values the engine did not compute. Those channel or call names are flagged in
//! [`Trace::external`]. [`Trace::hardware`] adds the resolved receiver, exact
//! call site, and selected route. Deterministic `System` calls have provenance
//! but are not external.
//!
//! Internal channel and expression columns retain their M1 scalar family. The
//! established JSON and CSV formats are untyped compatibility outputs, so they
//! preserve numeric values but do not encode signedness metadata. Serialisation
//! is deterministic, with a `time` column followed by channels in sorted-name
//! order. Ordered serial transfers and CAN frames are JSON metadata; they do not
//! become channel columns.

use crate::env::CallSite;
use crate::hardware::{EvalPhase, EvalTime, HardwareProvenance, ResolvedReceiver};
use crate::value::{M1Scalar, Value};
use std::collections::{BTreeMap, BTreeSet};

/// A column-oriented record of an evaluation run.
#[derive(Debug, Clone, Default)]
pub struct Trace {
    /// The shared tick time axis, in seconds. One entry per tick.
    pub time: Vec<f64>,
    /// Channel value columns, keyed by canonical path. Runner-held columns are
    /// dense after their first value; a channel first written mid-run cannot
    /// represent its empty prefix in this compatibility shape. A `BTreeMap`
    /// keeps channel order deterministic, while internal tick metadata retains
    /// exact alignment.
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
    /// Ordered byte transfers through the deterministic virtual serial adapter.
    /// Unlike de-duplicated hardware provenance, repeated transfers are retained.
    pub serial: Vec<SerialEvent>,
    /// Ordered frames received or transmitted through the deterministic virtual
    /// CAN adapter. Repeated transfers are retained for TX inspection.
    pub can: Vec<CanEvent>,
    /// Final channel value and provenance for each tick that recorded one.
    /// Kept separately from the compatibility columns, whose public shape
    /// cannot represent a missing value before a channel first appears.
    channel_ticks: BTreeMap<String, BTreeMap<usize, ChannelTick>>,
}

#[derive(Debug, Clone, PartialEq)]
struct ChannelTick {
    value: Value,
    external: bool,
}

/// Direction of one observable virtual serial transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerialDirection {
    /// Bytes copied from a scenario-controlled port into a handle receive buffer.
    Rx,
    /// Bytes copied from a handle transmit buffer onto a virtual port.
    Tx,
}

impl SerialDirection {
    fn as_str(self) -> &'static str {
        match self {
            SerialDirection::Rx => "rx",
            SerialDirection::Tx => "tx",
        }
    }
}

/// One ordered virtual serial byte event.
#[derive(Debug, Clone, PartialEq)]
pub struct SerialEvent {
    /// Receive or transmit.
    pub direction: SerialDirection,
    /// Evaluator time at which the script observed or emitted the bytes.
    pub time: EvalTime,
    /// Virtual M1 serial port number.
    pub port: i32,
    /// Stable nonzero handle used by the call.
    pub handle: u32,
    /// Bytes transferred in wire order.
    pub bytes: Vec<u8>,
    /// Exact `Serial.Receive` or `Serial.Transmit` call occurrence.
    pub site: CallSite,
}

/// Direction of one observable virtual CAN transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanTransferDirection {
    /// A scenario-controlled frame was consumed by a receive object.
    Rx,
    /// A script emitted a frame from a transmit buffer.
    Tx,
}

impl CanTransferDirection {
    fn as_str(self) -> &'static str {
        match self {
            CanTransferDirection::Rx => "rx",
            CanTransferDirection::Tx => "tx",
        }
    }
}

/// One ordered virtual CAN frame event.
#[derive(Debug, Clone, PartialEq)]
pub struct CanEvent {
    /// Receive or transmit.
    pub direction: CanTransferDirection,
    /// Evaluator time at which the script consumed or emitted the frame.
    pub time: EvalTime,
    /// M1 CAN bus number.
    pub bus: i32,
    /// Numeric CAN identifier.
    pub frame_id: u32,
    /// Standard 11-bit or extended 29-bit format.
    pub format: m1_can::CanFrameFormat,
    /// Payload bytes in wire order.
    pub bytes: Vec<u8>,
    /// Stable raw or DBC transmit/receive handle when the operation has one.
    pub handle: Option<u32>,
    /// Exact DBC message identity for DBC-backed operations.
    pub message: Option<String>,
    /// Exact hardware-call occurrence.
    pub site: CallSite,
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

    /// Record an evaluator-computed channel value for the current tick. Multiple
    /// assignments on one tick replace that tick's prior value, leaving the
    /// final assignment as the observable end-of-tick value. A channel first
    /// seen mid-run remains sparse in the public compatibility column; exact
    /// consumers use the retained tick index through the crate-private accessors.
    pub fn record_channel(&mut self, path: impl Into<String>, value: Value) {
        self.record_channel_tick(path.into(), value, false);
    }

    /// Record an externally supplied channel value for the current tick.
    pub(crate) fn record_external_channel(&mut self, path: impl Into<String>, value: Value) {
        let path = path.into();
        self.record_channel_tick(path.clone(), value, true);
        self.mark_external(path);
    }

    /// Record a zero-order hold, preserving the most recent value's provenance.
    /// With no prior sample, the value came from an executed startup function:
    /// scenario inputs and initial state are recorded explicitly before holds.
    pub(crate) fn record_held_channel(&mut self, path: impl Into<String>, value: Value) {
        let path = path.into();
        let tick = self.time.len().checked_sub(1);
        let external = tick
            .and_then(|tick| {
                self.channel_ticks
                    .get(&path)
                    .and_then(|samples| samples.range(..tick).next_back())
                    .map(|(_, sample)| sample.external)
            })
            .unwrap_or_else(|| self.external.contains(&path));
        self.record_channel_tick(path, value, external);
    }

    fn record_channel_tick(&mut self, path: String, value: Value, external: bool) {
        let Some(tick) = self.time.len().checked_sub(1) else {
            // Direct evaluator unit harnesses can record without opening a tick.
            self.channels.entry(path).or_default().push(value);
            return;
        };
        let replaced = self
            .channel_ticks
            .entry(path.clone())
            .or_default()
            .insert(
                tick,
                ChannelTick {
                    value: value.clone(),
                    external,
                },
            )
            .is_some();
        let column = self.channels.entry(path).or_default();
        if replaced && let Some(previous) = column.last_mut() {
            *previous = value;
        } else {
            column.push(value);
        }
    }

    /// Whether the channel already has a final value for the open tick.
    pub(crate) fn has_channel_value_at_current_tick(&self, path: &str) -> bool {
        self.time.len().checked_sub(1).is_some_and(|tick| {
            self.channel_ticks
                .get(path)
                .is_some_and(|samples| samples.contains_key(&tick))
        })
    }

    /// The final channel value recorded at one exact tick.
    pub(crate) fn channel_value_at_tick(&self, path: &str, tick: usize) -> Option<&Value> {
        self.channel_ticks
            .get(path)
            .and_then(|samples| samples.get(&tick))
            .map(|sample| &sample.value)
            .or_else(|| {
                self.channels
                    .get(path)
                    .filter(|column| column.len() == self.time.len())
                    .and_then(|column| column.get(tick))
            })
    }

    /// Whether the exact tick value came from an external source.
    pub(crate) fn channel_is_external_at_tick(&self, path: &str, tick: usize) -> bool {
        self.channel_ticks
            .get(path)
            .and_then(|samples| samples.get(&tick))
            .map(|sample| sample.external)
            .unwrap_or_else(|| self.external.contains(path))
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
        let path = path.into();
        self.external.insert(path.clone());
        if let Some(tick) = self.time.len().checked_sub(1)
            && let Some(sample) = self
                .channel_ticks
                .get_mut(&path)
                .and_then(|samples| samples.get_mut(&tick))
        {
            sample.external = true;
        }
    }

    /// Whether a channel is flagged externally driven.
    pub fn is_external(&self, path: &str) -> bool {
        self.external.contains(path)
    }

    /// Record how one hardware call obtained its value.
    pub fn record_hardware(&mut self, provenance: HardwareProvenance) {
        self.hardware.insert(provenance);
    }

    /// Append one virtual serial transfer in execution order.
    pub fn record_serial(&mut self, event: SerialEvent) {
        self.serial.push(event);
    }

    /// Append one virtual CAN frame in execution order.
    pub fn record_can(&mut self, event: CanEvent) {
        self.can.push(event);
    }

    /// Serialise the channel columns + time axis to JSON. The shape is
    /// `{ "time": [...], "channels": { path: [...] }, "external": [...],
    /// "hardware": [...], "serial": [...], "can": [...] }`,
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
        out.push_str("],\"serial\":[");
        out.push_str(&join(self.serial.iter().map(serial_json)));
        out.push_str("],\"can\":[");
        out.push_str(&join(self.can.iter().map(can_json)));
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

/// Render one ordered virtual serial transfer.
fn serial_json(event: &SerialEvent) -> String {
    let phase = match event.time.phase {
        EvalPhase::Startup => "startup",
        EvalPhase::Periodic => "periodic",
    };
    let bytes = join(event.bytes.iter().map(u8::to_string));
    format!(
        "{{\"direction\":{},\"time_s\":{},\"phase\":{},\"base_tick\":{},\"port\":{},\"handle\":{},\"bytes\":[{}],\"script\":{},\"offset\":{}}}",
        json_string(event.direction.as_str()),
        f64_json(event.time.elapsed_s),
        json_string(phase),
        event.time.base_tick,
        event.port,
        event.handle,
        bytes,
        json_string(event.site.script()),
        event.site.offset(),
    )
}

/// Render one ordered virtual CAN frame.
fn can_json(event: &CanEvent) -> String {
    let phase = match event.time.phase {
        EvalPhase::Startup => "startup",
        EvalPhase::Periodic => "periodic",
    };
    let format = match event.format {
        m1_can::CanFrameFormat::Standard => "standard",
        m1_can::CanFrameFormat::Extended => "extended",
    };
    let bytes = join(event.bytes.iter().map(u8::to_string));
    let handle = event
        .handle
        .map_or_else(|| "null".to_string(), |value| value.to_string());
    let message = event
        .message
        .as_deref()
        .map_or_else(|| "null".to_string(), json_string);
    format!(
        "{{\"direction\":{},\"time_s\":{},\"phase\":{},\"base_tick\":{},\"bus\":{},\"id\":{},\"format\":{},\"bytes\":[{}],\"handle\":{},\"message\":{},\"script\":{},\"offset\":{}}}",
        json_string(event.direction.as_str()),
        f64_json(event.time.elapsed_s),
        json_string(phase),
        event.time.base_tick,
        event.bus,
        event.frame_id,
        json_string(format),
        bytes,
        handle,
        message,
        json_string(event.site.script()),
        event.site.offset(),
    )
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
            "{\"time\":[0],\"channels\":{\"A\":[true],\"B\":[2]},\"external\":[\"A\"],\"hardware\":[],\"serial\":[],\"can\":[]}"
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
            "{\"time\":[0],\"channels\":{\"Fixed\":[0.1000001],\"Float\":[0.1],\"Int\":[-2147483648],\"Uint\":[4294967295]},\"external\":[],\"hardware\":[],\"serial\":[],\"can\":[]}"
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
            "{\"time\":[null,null,null],\"channels\":{\"M1\":[null,null,null]},\"external\":[],\"hardware\":[],\"serial\":[],\"can\":[]}"
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

    #[test]
    fn serial_events_are_ordered_json_metadata_and_not_csv_columns() {
        let mut trace = Trace::new();
        trace.push_tick(0.25);
        trace.record_channel("Out", Value::m1_integer(1));
        trace.record_serial(SerialEvent {
            direction: SerialDirection::Tx,
            time: EvalTime::periodic(25, 0.25, 0.01, 0.02),
            port: 2,
            handle: 7,
            bytes: vec![0x1b, 0x30],
            site: CallSite::new("Demo.Send.m1scr", 42),
        });

        let json: serde_json::Value =
            serde_json::from_str(&trace.to_json()).expect("serial trace is valid JSON");
        assert_eq!(
            json["serial"],
            serde_json::json!([{
                "direction": "tx",
                "time_s": 0.25,
                "phase": "periodic",
                "base_tick": 25,
                "port": 2,
                "handle": 7,
                "bytes": [27, 48],
                "script": "Demo.Send.m1scr",
                "offset": 42
            }])
        );
        assert_eq!(trace.to_csv(), "time,Out\n0.25,1\n");
    }

    #[test]
    fn can_events_are_ordered_json_metadata_and_not_csv_columns() {
        let mut trace = Trace::new();
        trace.push_tick(0.5);
        trace.record_channel("Out", Value::m1_integer(1));
        trace.record_can(CanEvent {
            direction: CanTransferDirection::Rx,
            time: EvalTime::periodic(50, 0.5, 0.01, 0.02),
            bus: 1,
            frame_id: 0x18ff_1234,
            format: m1_can::CanFrameFormat::Extended,
            bytes: vec![0xde, 0xad],
            handle: Some(9),
            message: Some("Vehicle Network.Status Frame".to_string()),
            site: CallSite::new("Demo.Read.m1scr", 77),
        });

        let json: serde_json::Value =
            serde_json::from_str(&trace.to_json()).expect("CAN trace is valid JSON");
        assert_eq!(
            json["can"],
            serde_json::json!([{
                "direction": "rx",
                "time_s": 0.5,
                "phase": "periodic",
                "base_tick": 50,
                "bus": 1,
                "id": 0x18ff_1234,
                "format": "extended",
                "bytes": [222, 173],
                "handle": 9,
                "message": "Vehicle Network.Status Frame",
                "script": "Demo.Read.m1scr",
                "offset": 77
            }])
        );
        assert_eq!(trace.to_csv(), "time,Out\n0.5,1\n");
    }
}
