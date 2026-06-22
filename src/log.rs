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
//!
//! ## CSV log schema (the always-unencumbered import path)
//!
//! [`Log::from_csv`] reads the formalised i2-export shape — the same `time`-first
//! CSV that [`crate::scenario::Scenario::load_csv`] parses, with one documented
//! extension (a units row). It shares that parser; there is no second CSV reader.
//!
//! - **Row 1 (header):** `time,<channel name>,<channel name>,…`. The first column
//!   header MUST be `time` (case-insensitive); otherwise the import fails loud.
//!   Channel headers are M1 channel paths verbatim (spaces allowed, e.g.
//!   `Engine Speed`), RFC-4180 quoting per the shared `split_csv_row` tokenizer.
//! - **Optional row 2 (units):** if the second row's first cell is *non-numeric*
//!   (e.g. `s,rpm,km/h`), it is treated as a units header — diverted into
//!   [`LogMeta::units`] (channel → unit) and *not* read as a value row, matching
//!   real i2 exports. A numeric first cell means there is no units row.
//! - **Data rows:** `t_seconds,value,value,…`. `time` is seconds, ascending;
//!   numeric cells parse to [`Value::Float`]; an empty cell adds no keyframe (the
//!   zero-order hold simply keeps the prior value). A non-numeric value cell (one
//!   not in the units row) fails loud — no guessed value, ever.
//! - **Resampling:** at any tick `t` a channel is sampled by zero-order hold
//!   ([`InputSeries::sample`]) — the established, deterministic rule used
//!   throughout the engine.
//!
//! [`Value::Float`]: crate::value::Value::Float

use std::collections::BTreeMap;

use crate::error::EvalError;
use crate::scenario::{InputKind, InputSeries, parse_time_series_csv};

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
    /// Import a [`Log`] from a `time`-first CSV (see the module header for the
    /// schema). Each non-time column becomes one [`InputSeries`] of kind
    /// [`InputKind::Series`]; an optional i2-style units row is captured into
    /// [`LogMeta::units`]. `source` records provenance (typically the file path).
    ///
    /// Fails loud (no guessed value) when: the CSV is empty, its first column is
    /// not `time`, or a value cell outside the units row is non-numeric. A column
    /// with no non-empty cell is dropped (it would carry no keyframes).
    pub fn from_csv(csv: &str, source: impl Into<String>) -> Result<Log, EvalError> {
        // Reuse the shared time/series parser, asking it to divert an i2 units row.
        let parsed = parse_time_series_csv(csv, true)?;
        let channels: Vec<InputSeries> = parsed
            .columns
            .into_iter()
            .filter(|(_, points)| !points.is_empty())
            .map(|(channel, points)| InputSeries {
                channel,
                kind: InputKind::Series(points),
            })
            .collect();
        // Duration = the latest keyframe time across channels.
        let duration_s = channels
            .iter()
            .filter_map(|s| match &s.kind {
                InputKind::Series(points) => points.last().map(|(t, _)| *t),
                InputKind::Const(_) => None,
            })
            .fold(0.0_f64, f64::max);
        let channel_count = channels.len();
        // Keep `meta.units` consistent with the retained channels: a units entry
        // for a column that carried no data (and was dropped) is pruned, so every
        // unit refers to a channel actually present in the log.
        let mut units = parsed.units;
        units.retain(|channel, _| channels.iter().any(|s| &s.channel == channel));
        Ok(Log {
            channels,
            meta: LogMeta {
                source: source.into(),
                duration_s,
                channel_count,
                units,
            },
        })
    }

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
        let s = log
            .series_for("Engine Speed")
            .expect("Engine Speed present");
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

#[cfg(test)]
mod from_csv_tests {
    use super::*;
    use crate::error::EvalError;
    use crate::value::Value;

    /// A CSV with a header, an explicit units row, then ascending data rows.
    /// `Engine Speed` carries a space in its channel name (M1 idents may).
    const WITH_UNITS: &str = "time,Engine Speed,Wheel Speed\n\
                              s,rpm,km/h\n\
                              0.0,800,0\n\
                              0.5,1200,30\n\
                              1.0,3000,60\n";

    /// The same shape but with a numeric second row (no units row).
    const NO_UNITS: &str = "time,Engine Speed,Wheel Speed\n\
                            0.0,800,0\n\
                            0.5,1200,30\n";

    #[test]
    fn parses_two_channels() {
        let log = Log::from_csv(WITH_UNITS, "run.csv").expect("CSV parses");
        // Two non-time columns -> two channels.
        assert_eq!(log.channels.len(), 2);
        assert_eq!(log.meta.channel_count, 2);
        let names: Vec<&str> = log.channel_names().collect();
        assert_eq!(names, vec!["Engine Speed", "Wheel Speed"]);
        assert_eq!(log.meta.source, "run.csv");
    }

    #[test]
    fn units_row_is_captured_not_a_value_row() {
        let log = Log::from_csv(WITH_UNITS, "run.csv").expect("CSV parses");
        // The units row went into meta.units, keyed by channel name.
        assert_eq!(
            log.meta.units.get("Engine Speed").map(String::as_str),
            Some("rpm")
        );
        assert_eq!(
            log.meta.units.get("Wheel Speed").map(String::as_str),
            Some("km/h")
        );
        // ...and was NOT consumed as a data keyframe: the first real keyframe is
        // at t=0.0 with value 800, so a sample at t=0.0 is 800 (not the units row).
        let es = log
            .series_for("Engine Speed")
            .expect("Engine Speed present");
        assert_eq!(es.sample(0.0), Value::Float(800.0));
        // Three data rows -> three keyframes (the units row added none).
        match &es.kind {
            InputKind::Series(points) => assert_eq!(points.len(), 3),
            other => panic!("expected Series, got {other:?}"),
        }
        // Duration is the last keyframe time (1.0), not affected by the units row.
        assert_eq!(log.duration_s(), 1.0);
    }

    #[test]
    fn numeric_second_row_means_no_units() {
        let log = Log::from_csv(NO_UNITS, "run.csv").expect("CSV parses");
        // No units row: meta.units is empty, and the first data row is a keyframe.
        assert!(log.meta.units.is_empty());
        let es = log.series_for("Engine Speed").expect("present");
        assert_eq!(es.sample(0.0), Value::Float(800.0));
        match &es.kind {
            InputKind::Series(points) => assert_eq!(points.len(), 2),
            other => panic!("expected Series, got {other:?}"),
        }
    }

    #[test]
    fn channel_name_with_space_survives_verbatim() {
        let log = Log::from_csv(WITH_UNITS, "run.csv").expect("CSV parses");
        // "Engine Speed" is matched whole; never split on whitespace.
        let s = log
            .series_for("Engine Speed")
            .expect("Engine Speed present");
        assert_eq!(s.channel, "Engine Speed");
    }

    #[test]
    fn zero_order_hold_at_inter_row_time() {
        let log = Log::from_csv(WITH_UNITS, "run.csv").expect("CSV parses");
        let es = log.series_for("Engine Speed").expect("present");
        // Between keyframes (0.5 and 1.0), the earlier keyframe is held.
        assert_eq!(es.sample(0.75), Value::Float(1200.0));
        // After the last keyframe, the last value is held.
        assert_eq!(es.sample(2.0), Value::Float(3000.0));
        // Before the first keyframe, the first value is held.
        assert_eq!(es.sample(-1.0), Value::Float(800.0));
    }

    #[test]
    fn non_numeric_value_cell_fails_loud() {
        // A non-numeric cell that is NOT in the units row (row 3 here) fails loud.
        let csv = "time,Engine Speed\n\
                   s,rpm\n\
                   0.0,800\n\
                   0.5,oops\n";
        match Log::from_csv(csv, "run.csv") {
            Err(EvalError::TypeError { .. }) => {}
            other => panic!("expected TypeError on non-numeric value cell, got {other:?}"),
        }
    }

    #[test]
    fn missing_time_first_column_fails_loud() {
        let csv = "t,Engine Speed\n0.0,800\n";
        match Log::from_csv(csv, "run.csv") {
            Err(EvalError::UnsupportedConstruct { .. }) => {}
            other => panic!("expected UnsupportedConstruct for missing `time`, got {other:?}"),
        }
    }

    #[test]
    fn empty_csv_fails_loud() {
        match Log::from_csv("", "run.csv") {
            Err(EvalError::UnsupportedConstruct { .. }) => {}
            other => panic!("expected UnsupportedConstruct for empty CSV, got {other:?}"),
        }
    }

    #[test]
    fn time_is_case_insensitive() {
        // The first column header `Time` (any case) is accepted.
        let csv = "Time,Engine Speed\n0.0,800\n0.5,1200\n";
        let log = Log::from_csv(csv, "run.csv").expect("Time header accepted");
        assert_eq!(log.channels.len(), 1);
    }

    #[test]
    fn units_pruned_for_dropped_empty_column() {
        // A column that has a unit but no data anywhere is dropped, and its unit
        // is pruned from meta.units so every unit refers to a present channel.
        let csv = "time,Engine Speed,Empty Channel\n\
                   s,rpm,V\n\
                   0.0,800,\n\
                   0.5,1200,\n";
        let log = Log::from_csv(csv, "run.csv").expect("CSV parses");
        // Only the populated channel survives.
        let names: Vec<&str> = log.channel_names().collect();
        assert_eq!(names, vec!["Engine Speed"]);
        // The dropped column's unit is gone; the kept channel's unit remains.
        assert_eq!(
            log.meta.units.get("Engine Speed").map(String::as_str),
            Some("rpm")
        );
        assert!(!log.meta.units.contains_key("Empty Channel"));
    }

    #[test]
    fn empty_value_cell_holds_previous() {
        // An empty cell adds no keyframe; the zero-order hold keeps the prior value.
        let csv = "time,Engine Speed\n0.0,800\n0.5,\n1.0,1200\n";
        let log = Log::from_csv(csv, "run.csv").expect("CSV parses");
        let es = log.series_for("Engine Speed").expect("present");
        // No keyframe at t=0.5, so 0.7 holds the 0.0 value (800).
        assert_eq!(es.sample(0.7), Value::Float(800.0));
        assert_eq!(es.sample(1.0), Value::Float(1200.0));
        // Two keyframes only (the empty cell added none).
        match &es.kind {
            InputKind::Series(points) => assert_eq!(points.len(), 2),
            other => panic!("expected Series, got {other:?}"),
        }
    }
}
