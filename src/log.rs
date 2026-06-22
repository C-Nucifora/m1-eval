// SPDX-License-Identifier: GPL-3.0-or-later
//! Format-agnostic recorded-run model: the [`Log`].
//!
//! A [`Log`] is a recorded MoTeC run reduced to its load-bearing essence — a set
//! of per-channel time series ([`InputSeries`] of kind [`InputKind::Series`]) plus
//! provenance metadata ([`LogMeta`]). It is the ground-truth baseline for
//! counterfactual replay: every logged channel is treated as truth, the user
//! overrides one or more, and only the downstream cone is recomputed.
//!
//! This module owns only the *model* and its accessors. Concrete importers (CSV,
//! and the feature-gated `.ld` reader) live alongside it and produce a [`Log`];
//! nothing format-specific leaks into this type. A [`Log`] is, in effect, a
//! `Vec<InputSeries>` plus metadata, and resampling onto a tick grid is the same
//! zero-order-hold [`InputSeries::sample`] used everywhere else in the engine.
//!
//! Channel names are M1 channel paths used verbatim — M1 identifiers may contain
//! spaces (`Engine Speed`, `Drive State`), so names are never split on whitespace.

use std::collections::BTreeMap;

use crate::scenario::{InputKind, InputSeries};

/// A recorded run as a set of per-channel time series plus provenance metadata.
///
/// Each entry in `channels` is one logged channel, expected to be an
/// [`InputSeries`] of kind [`InputKind::Series`] (a constant-kind input is
/// accepted by the accessors but a real log is always keyframed). `meta` carries
/// transparency/provenance: where the log came from, how long it runs, how many
/// channels it has, and any per-channel units captured by the importer.
#[derive(Debug, Clone, PartialEq)]
pub struct Log {
    /// One time series per logged channel (kind = [`InputKind::Series`]).
    pub channels: Vec<InputSeries>,
    /// Provenance and shape metadata for transparency.
    pub meta: LogMeta,
}

/// Provenance and shape metadata for a [`Log`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LogMeta {
    /// Where the log came from (e.g. a file path or `"synthetic"`).
    pub source: String,
    /// Total run duration in seconds (the latest keyframe time across channels).
    pub duration_s: f64,
    /// Number of channels in the log.
    pub channel_count: usize,
    /// Optional per-channel units (channel name -> unit string), as captured by
    /// the importer (e.g. a CSV units row or `.ld` channel units).
    pub units: BTreeMap<String, String>,
}

impl Log {
    /// Look up the [`InputSeries`] for a channel by exact (verbatim) name.
    ///
    /// Names are matched whole — never split on `.` or whitespace — so a channel
    /// path containing spaces is found by its full path.
    pub fn series_for(&self, channel: &str) -> Option<&InputSeries> {
        self.channels.iter().find(|s| s.channel == channel)
    }

    /// Iterate the logged channel names in declaration order.
    pub fn channel_names(&self) -> impl Iterator<Item = &str> {
        self.channels.iter().map(|s| s.channel.as_str())
    }

    /// The run duration in seconds: the maximum keyframe time across all channels.
    ///
    /// Empty channels (and constant-kind inputs, which have no time axis)
    /// contribute nothing; a log with no keyframes anywhere has duration `0.0`.
    pub fn duration_s(&self) -> f64 {
        self.channels
            .iter()
            .filter_map(|s| match &s.kind {
                InputKind::Series(points) => points.last().map(|(t, _)| *t),
                InputKind::Const(_) => None,
            })
            .fold(0.0_f64, f64::max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;

    /// Build a hand-authored two-channel log: a `Sensor` ramp and an
    /// `Engine Speed` series (note the space in the channel name).
    fn hand_built_log() -> Log {
        let sensor = InputSeries {
            channel: "Root.CF.Sensor".to_string(),
            kind: InputKind::Series(vec![
                (0.0, Value::Float(1.0)),
                (0.5, Value::Float(2.0)),
                (1.0, Value::Float(3.0)),
            ]),
        };
        let engine_speed = InputSeries {
            channel: "Engine Speed".to_string(),
            kind: InputKind::Series(vec![
                (0.0, Value::Float(800.0)),
                (0.25, Value::Float(1200.0)),
            ]),
        };
        Log {
            channels: vec![sensor, engine_speed],
            meta: LogMeta {
                source: "hand-built".to_string(),
                duration_s: 1.0,
                channel_count: 2,
                units: BTreeMap::new(),
            },
        }
    }

    #[test]
    fn series_for_finds_channel_verbatim() {
        let log = hand_built_log();
        let s = log.series_for("Root.CF.Sensor").expect("Sensor present");
        assert_eq!(s.channel, "Root.CF.Sensor");
        // Zero-order-hold sampling round-trips through the stored series.
        assert_eq!(s.sample(0.75), Value::Float(2.0));
    }

    #[test]
    fn series_for_preserves_channel_with_space() {
        let log = hand_built_log();
        // A channel path containing a space is matched whole, never split.
        let s = log.series_for("Engine Speed").expect("Engine Speed present");
        assert_eq!(s.channel, "Engine Speed");
    }

    #[test]
    fn series_for_missing_channel_is_none() {
        let log = hand_built_log();
        assert!(log.series_for("No Such Channel").is_none());
    }

    #[test]
    fn channel_names_yields_all_in_order() {
        let log = hand_built_log();
        let names: Vec<&str> = log.channel_names().collect();
        assert_eq!(names, vec!["Root.CF.Sensor", "Engine Speed"]);
    }

    #[test]
    fn duration_is_latest_keyframe_across_channels() {
        let log = hand_built_log();
        // Sensor ends at t=1.0, Engine Speed ends at t=0.25; the max is 1.0.
        assert_eq!(log.duration_s(), 1.0);
    }

    #[test]
    fn duration_ignores_const_inputs_and_empty_log() {
        let log = Log {
            channels: vec![InputSeries {
                channel: "K".to_string(),
                kind: InputKind::Const(Value::Float(5.0)),
            }],
            meta: LogMeta::default(),
        };
        // A constant has no time axis; with no keyframes anywhere, duration is 0.
        assert_eq!(log.duration_s(), 0.0);
    }
}
