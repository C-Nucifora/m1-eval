// SPDX-License-Identifier: GPL-3.0-or-later
//! Clean-room binary `.ld` import, behind the `ld` cargo feature.
//!
//! This module is the only place the MIT [`motec_i2`] crate is used. It reads (and,
//! for tests, writes a synthetic fixture of) the *MoTeC i2 `.ld` file format* — an
//! independently reverse-engineered, license-compatible format, operating on the
//! user's own telemetry. No MoTeC *software* is decompiled and no MoTeC bytes
//! (calibration, firmware, manual text, or sample logs) are committed.
//!
//! All `motec_i2` types ([`motec_i2::LDReader`], [`motec_i2::ChannelMetadata`],
//! [`motec_i2::Sample`], …) stay inside this module. Only `m1-eval`'s own
//! [`crate::Log`] / [`crate::InputSeries`] / [`crate::Value`] cross the boundary —
//! the toolchain type never leaks past the public API.
//!
//! ## Engineering-unit decode
//!
//! [`motec_i2`]'s reader returns **raw** samples; the physical (engineering-unit)
//! value is recovered from the channel's `scale` / `dec_places` / `mul` fields.
//! [`from_ld`] applies that decode to produce `f64` values and derives each
//! sample's time as `index / sample_rate` seconds (zero-order-hold keyframes),
//! mapping each `.ld` channel to a [`crate::InputSeries`] under its verbatim name.
//!
//! ## Fail-loud discipline
//!
//! The upstream crate *panics* (or `unimplemented!()`s) on a handful of inputs:
//! `channel_data` panics on [`motec_i2::Datatype::Invalid`] and is unimplemented
//! for `F16`, and `Sample::decode_f64` asserts a zero `offset`. [`from_ld`] guards
//! every one of these *before* calling the crate, returning an [`EvalError`]
//! instead — never a guessed value and never a panic across the public API. A zero
//! `sample_rate` (no time base) is likewise rejected rather than dividing by zero.
//!
//! ## Provenance & milestones
//!
//! P3-D.T10 landed the feature, the optional `motec-i2` dependency, and the
//! synthetic-`.ld` writer fixture (the committed-CI fixture is generated at test
//! time via [`motec_i2::LDWriter`], so no proprietary bytes are committed). P3-D.T11
//! (this module's [`from_ld`]) adds the engineering-unit decode + time derivation.

use std::io::Cursor;

use motec_i2::{ChannelMetadata, Datatype, LDReader, Sample};

use crate::error::EvalError;
use crate::log::{Log, LogMeta};
use crate::scenario::{InputKind, InputSeries};
use crate::value::Value;

/// Import a [`Log`] from the bytes of a MoTeC `.ld` file (clean-room reader).
///
/// Reads the header (into [`LogMeta`] provenance), then every channel's metadata
/// and raw samples, applies the engineering-unit decode (`scale`/`dec_places`/
/// `mul`), and derives each sample's time as `index / sample_rate` seconds. Each
/// `.ld` channel becomes one [`InputSeries`] of kind [`InputKind::Series`] under
/// its verbatim `.ld` name (M1 identifiers may contain spaces, so the name is used
/// whole). `source` records provenance (typically the file path).
///
/// All [`motec_i2`] types stay inside this function; only `m1-eval`'s own
/// [`Log`]/[`InputSeries`]/[`Value`] cross out.
///
/// Fails loud — never a guessed value, never a panic — when a channel carries an
/// unsupported datatype ([`Datatype::Invalid`] or `F16`), a non-zero sample
/// `offset` (the decode the upstream crate implements assumes zero), or a zero
/// `sample_rate` (no time base to grid against).
pub fn from_ld(bytes: &[u8], source: impl Into<String>) -> Result<Log, EvalError> {
    let mut cursor = Cursor::new(bytes);
    let mut reader = LDReader::new(&mut cursor);

    let header = reader
        .read_header()
        .map_err(|e| EvalError::UnsupportedConstruct {
            kind: format!("`.ld` header parse failed: {e}"),
            at: 0,
        })?;
    let metas = reader
        .read_channels()
        .map_err(|e| EvalError::UnsupportedConstruct {
            kind: format!("`.ld` channel-metadata parse failed: {e}"),
            at: 0,
        })?;

    let mut channels = Vec::with_capacity(metas.len());
    let mut units = std::collections::BTreeMap::new();
    let mut duration_s = 0.0_f64;

    for meta in &metas {
        // Guard the upstream panics/asserts BEFORE touching the data: a guessed
        // value is never acceptable, and a panic must never cross the public API.
        check_decodable(meta)?;

        let raw = reader
            .channel_data(meta)
            .map_err(|e| EvalError::UnsupportedConstruct {
                kind: format!("`.ld` channel `{}` data read failed: {e}", meta.name),
                at: 0,
            })?;

        let rate = meta.sample_rate as f64;
        let points: Vec<(f64, Value)> = raw
            .iter()
            .enumerate()
            .map(|(i, sample)| {
                // Time of sample `i` = i / sample_rate seconds (zero-order hold).
                let t = i as f64 / rate;
                // Raw -> engineering units via the channel's scale/dec_places/mul.
                let value = decode_sample(sample, meta);
                (t, Value::Float(value))
            })
            .collect();

        if let Some((last_t, _)) = points.last() {
            duration_s = duration_s.max(*last_t);
        }
        if !meta.unit.is_empty() {
            units.insert(meta.name.clone(), meta.unit.clone());
        }

        channels.push(InputSeries {
            // Default name mapping: the `.ld` channel name is an M1 channel path
            // used verbatim. (A project-aware `ident::classify` match is a future
            // hook; verbatim is the documented default.)
            channel: meta.name.clone(),
            kind: InputKind::Series(points),
        });
    }

    // Keep only units for channels actually present (mirrors `from_csv`).
    units.retain(|channel, _| channels.iter().any(|s| &s.channel == channel));

    let channel_count = channels.len();
    let source = source.into();
    // Fold the most useful header provenance into the source string for
    // transparency (device + venue + driver), without leaking any `motec_i2` type.
    let provenance = format!(
        "{source} [device={} venue={} driver={} date={} {}]",
        header.device_type, header.venue, header.driver, header.date_string, header.time_string,
    );

    Ok(Log {
        channels,
        meta: LogMeta {
            source: provenance,
            duration_s,
            channel_count,
            units,
        },
    })
}

/// Reject a channel whose samples the upstream crate cannot decode without
/// panicking. Returns a fail-loud [`EvalError`] for an unsupported datatype, a
/// non-zero offset, or a zero sample rate — otherwise `Ok(())`.
fn check_decodable(meta: &ChannelMetadata) -> Result<(), EvalError> {
    match meta.datatype {
        // `channel_data` would `unimplemented!()` (F16) or `panic!` (Invalid).
        Datatype::F16 => {
            return Err(EvalError::UnsupportedConstruct {
                kind: format!(
                    "`.ld` channel `{}` uses datatype F16, which is not decodable",
                    meta.name
                ),
                at: 0,
            });
        }
        Datatype::Invalid => {
            return Err(EvalError::UnsupportedConstruct {
                kind: format!(
                    "`.ld` channel `{}` has an invalid/unsupported datatype",
                    meta.name
                ),
                at: 0,
            });
        }
        Datatype::Beacon16 | Datatype::Beacon32 | Datatype::I16 | Datatype::I32 | Datatype::F32 => {
        }
    }
    // `Sample::decode_f64` asserts a zero offset; honour that as a fail-loud check.
    if meta.offset != 0 {
        return Err(EvalError::UnsupportedConstruct {
            kind: format!(
                "`.ld` channel `{}` has a non-zero sample offset ({}), \
                 which the engineering-unit decode does not support",
                meta.name, meta.offset
            ),
            at: 0,
        });
    }
    // A zero sample rate has no time base; deriving `i / rate` would divide by zero.
    if meta.sample_rate == 0 {
        return Err(EvalError::UnsupportedConstruct {
            kind: format!(
                "`.ld` channel `{}` has a zero sample rate (no time base)",
                meta.name
            ),
            at: 0,
        });
    }
    Ok(())
}

/// Decode one raw [`Sample`] to its engineering-unit `f64` value, applying the
/// channel's `scale`/`dec_places`/`mul`. Callers MUST have passed `meta` through
/// [`check_decodable`] first (so the upstream `offset == 0` assert cannot fire).
fn decode_sample(sample: &Sample, meta: &ChannelMetadata) -> f64 {
    sample.decode_f64(meta)
}

#[cfg(test)]
mod tests {
    use crate::error::EvalError;
    use motec_i2::{ChannelMetadata, Datatype, Header, LDReader, LDWriter, Sample};
    use std::io::Cursor;

    /// The fixed file offset (`0x3448` = 13384) at which `motec_i2`'s writer places
    /// the first channel-metadata block. The reader locates channels via the
    /// header's `channel_meta_ptr`, so a synthetic header MUST advertise this same
    /// offset for the linked-list walk to find the written channels.
    const CHANNEL_META_PTR: u32 = 0x3448;

    /// Build a synthetic `.ld` byte image with two channels:
    ///
    /// - `Sensor` — `I16` raw samples with non-trivial `mul`/`scale`/`dec_places`
    ///   scaling (so a later engineering-unit decode is exercised), 10 Hz;
    /// - `Other` — `F32` samples, unit-scaled, 5 Hz.
    ///
    /// Written entirely in-memory through [`LDWriter`] into a `Cursor<Vec<u8>>`;
    /// this is the synthetic CI fixture source — no proprietary bytes are involved.
    /// `offset` is held at 0 on every channel: `motec_i2`'s `decode_f64` only
    /// supports the zero-offset case, which is what a synthetic fixture should use.
    fn synth_ld_bytes() -> Vec<u8> {
        let header = Header {
            // The reader trusts this pointer; it must equal where the writer places
            // the first channel block.
            channel_meta_ptr: CHANNEL_META_PTR,
            channel_data_ptr: 0,
            event_ptr: 0,
            device_serial: 42,
            device_type: "M1".to_string(),
            device_version: 1,
            num_channels: 2,
            date_string: "23/06/2026".to_string(),
            time_string: "00:00:00".to_string(),
            driver: "synthetic".to_string(),
            vehicleid: "EV25".to_string(),
            venue: "synthetic".to_string(),
            session: "synthetic".to_string(),
            short_comment: "m1-eval synthetic fixture".to_string(),
        };

        let sensor_meta = ChannelMetadata {
            prev_addr: 0,
            next_addr: 0,
            data_addr: 0,
            data_count: 0,
            datatype: Datatype::I16,
            sample_rate: 10,
            offset: 0,
            mul: 1,
            scale: 1,
            dec_places: 1,
            name: "Sensor".to_string(),
            short_name: "Sensor".to_string(),
            unit: "V".to_string(),
        };
        let sensor_samples = vec![
            Sample::I16(100),
            Sample::I16(200),
            Sample::I16(300),
            Sample::I16(400),
        ];

        let other_meta = ChannelMetadata {
            prev_addr: 0,
            next_addr: 0,
            data_addr: 0,
            data_count: 0,
            datatype: Datatype::F32,
            sample_rate: 5,
            offset: 0,
            mul: 1,
            scale: 1,
            dec_places: 0,
            name: "Other".to_string(),
            short_name: "Other".to_string(),
            unit: "C".to_string(),
        };
        let other_samples = vec![Sample::F32(1.5), Sample::F32(2.5)];

        let mut cursor = Cursor::new(Vec::new());
        LDWriter::new(&mut cursor, header)
            .with_channel(sensor_meta, sensor_samples)
            .with_channel(other_meta, other_samples)
            .write()
            .expect("synthetic .ld writes");
        cursor.into_inner()
    }

    #[test]
    fn synthetic_ld_is_non_empty_and_rereadable() {
        // The writer produced bytes (it pre-writes a full header blob, then the
        // channel metadata + samples), so the fixture is non-trivial.
        let bytes = synth_ld_bytes();
        assert!(!bytes.is_empty(), "synthetic .ld must produce bytes");
        assert!(
            bytes.len() as u32 > CHANNEL_META_PTR,
            "fixture spans past the channel-metadata offset"
        );

        // Re-read it through the same crate's reader: the header round-trips, the
        // two channels survive with their names/units/datatypes/sample rates.
        let mut cursor = Cursor::new(bytes);
        let mut reader = LDReader::new(&mut cursor);

        let header = reader.read_header().expect("header parses");
        assert_eq!(header.device_type, "M1");
        assert_eq!(header.num_channels, 2);
        assert_eq!(header.channel_meta_ptr, CHANNEL_META_PTR);

        let channels = reader.read_channels().expect("channels parse");
        assert_eq!(channels.len(), 2, "two channels round-trip");

        let sensor = &channels[0];
        assert_eq!(sensor.name, "Sensor");
        assert_eq!(sensor.unit, "V");
        assert_eq!(sensor.datatype, Datatype::I16);
        assert_eq!(sensor.sample_rate, 10);
        assert_eq!(sensor.data_count, 4);

        let other = &channels[1];
        assert_eq!(other.name, "Other");
        assert_eq!(other.unit, "C");
        assert_eq!(other.datatype, Datatype::F32);
        assert_eq!(other.sample_rate, 5);
        assert_eq!(other.data_count, 2);
    }

    #[test]
    fn synthetic_ld_sample_values_survive_round_trip() {
        // The raw samples come back byte-identical through the reader, and the
        // crate's scaling decode turns them into the expected engineering values.
        let bytes = synth_ld_bytes();
        let mut cursor = Cursor::new(bytes);
        let mut reader = LDReader::new(&mut cursor);
        reader.read_header().expect("header parses");
        let channels = reader.read_channels().expect("channels parse");

        // Sensor: I16 raw {100,200,300,400}. decode = raw / scale * 10^-dec_places
        //         * mul. With scale=1, dec_places=1, mul=1 -> raw * 0.1.
        let sensor = &channels[0];
        let sensor_raw = reader.channel_data(sensor).expect("sensor data");
        assert_eq!(
            sensor_raw,
            vec![
                Sample::I16(100),
                Sample::I16(200),
                Sample::I16(300),
                Sample::I16(400),
            ]
        );
        let decoded: Vec<f64> = sensor_raw.iter().map(|s| s.decode_f64(sensor)).collect();
        let expected = [10.0_f64, 20.0, 30.0, 40.0];
        for (got, want) in decoded.iter().zip(expected.iter()) {
            assert!((got - want).abs() < 1e-9, "decoded {got} != {want}");
        }

        // Other: F32 {1.5, 2.5}, unit scaling (scale=1, dec_places=0, mul=1) -> id.
        let other = &channels[1];
        let other_raw = reader.channel_data(other).expect("other data");
        assert_eq!(other_raw, vec![Sample::F32(1.5), Sample::F32(2.5)]);
        let other_decoded: Vec<f64> = other_raw.iter().map(|s| s.decode_f64(other)).collect();
        assert!((other_decoded[0] - 1.5).abs() < 1e-9);
        assert!((other_decoded[1] - 2.5).abs() < 1e-9);
    }

    // ---- P3-D.T11: Log::from_ld (engineering-unit decode + time grid) ----

    use super::from_ld;
    use crate::scenario::InputKind;
    use crate::value::Value;

    /// Build a synthetic single-channel `.ld` image from a fully-specified
    /// metadata + samples pair (so the fail-loud tests can vary one field at a
    /// time: datatype, offset, sample rate). The header advertises the writer's
    /// channel-metadata offset so the reader can find the channel.
    fn synth_one_channel(meta: ChannelMetadata, samples: Vec<Sample>) -> Vec<u8> {
        let header = Header {
            channel_meta_ptr: CHANNEL_META_PTR,
            channel_data_ptr: 0,
            event_ptr: 0,
            device_serial: 7,
            device_type: "M1".to_string(),
            device_version: 1,
            num_channels: 1,
            date_string: "23/06/2026".to_string(),
            time_string: "00:00:00".to_string(),
            driver: "synthetic".to_string(),
            vehicleid: "EV25".to_string(),
            venue: "synthetic".to_string(),
            session: "synthetic".to_string(),
            short_comment: "m1-eval synthetic fixture".to_string(),
        };
        let mut cursor = Cursor::new(Vec::new());
        LDWriter::new(&mut cursor, header)
            .with_channel(meta, samples)
            .write()
            .expect("synthetic single-channel .ld writes");
        cursor.into_inner()
    }

    /// A baseline `Sensor` metadata (I16, 10 Hz, scale=1/dec_places=1/mul=1) the
    /// fail-loud tests clone and perturb.
    fn sensor_meta() -> ChannelMetadata {
        ChannelMetadata {
            prev_addr: 0,
            next_addr: 0,
            data_addr: 0,
            data_count: 0,
            datatype: Datatype::I16,
            sample_rate: 10,
            offset: 0,
            mul: 1,
            scale: 1,
            dec_places: 1,
            name: "Sensor".to_string(),
            short_name: "Sensor".to_string(),
            unit: "V".to_string(),
        }
    }

    #[test]
    fn from_ld_reads_both_channels_with_names_and_units() {
        let log = from_ld(&synth_ld_bytes(), "run.ld").expect("from_ld parses");
        // Both channels present, names verbatim, in declaration order.
        let names: Vec<&str> = log.channel_names().collect();
        assert_eq!(names, vec!["Sensor", "Other"]);
        assert_eq!(log.meta.channel_count, 2);
        // Units captured from the channel metadata.
        assert_eq!(log.meta.units.get("Sensor").map(String::as_str), Some("V"));
        assert_eq!(log.meta.units.get("Other").map(String::as_str), Some("C"));
        // Provenance carries the original source plus header device/venue/driver.
        assert!(log.meta.source.contains("run.ld"), "{}", log.meta.source);
        assert!(log.meta.source.contains("device=M1"), "{}", log.meta.source);
    }

    #[test]
    fn from_ld_applies_engineering_unit_scaling() {
        let log = from_ld(&synth_ld_bytes(), "run.ld").expect("from_ld parses");
        let sensor = log.series_for("Sensor").expect("Sensor present");
        // I16 raw {100,200,300,400}; decode = raw / scale * 10^-dec_places * mul
        //   = raw / 1 * 10^-1 * 1 = raw * 0.1 -> {10, 20, 30, 40}.
        let InputKind::Series(points) = &sensor.kind else {
            panic!("expected a Series for Sensor");
        };
        let values: Vec<f64> = points
            .iter()
            .map(|(_, v)| v.as_f64().expect("numeric"))
            .collect();
        let expected = [10.0_f64, 20.0, 30.0, 40.0];
        for (got, want) in values.iter().zip(expected.iter()) {
            assert!((got - want).abs() < 1e-9, "decoded {got} != {want}");
        }
        // Hand-derived single value: the first sample (raw 100) -> 10.0.
        assert_eq!(sensor.sample(0.0), Value::Float(10.0));
    }

    #[test]
    fn from_ld_derives_time_from_sample_rate() {
        let log = from_ld(&synth_ld_bytes(), "run.ld").expect("from_ld parses");
        // Sensor @ 10 Hz, 4 samples -> times 0.0, 0.1, 0.2, 0.3.
        let sensor = log.series_for("Sensor").expect("Sensor present");
        let InputKind::Series(points) = &sensor.kind else {
            panic!("expected a Series for Sensor");
        };
        let times: Vec<f64> = points.iter().map(|(t, _)| *t).collect();
        let expected = [0.0_f64, 0.1, 0.2, 0.3];
        for (got, want) in times.iter().zip(expected.iter()) {
            assert!((got - want).abs() < 1e-12, "time {got} != {want}");
        }
        // Other @ 5 Hz, 2 samples -> times 0.0, 0.2.
        let other = log.series_for("Other").expect("Other present");
        let InputKind::Series(other_points) = &other.kind else {
            panic!("expected a Series for Other");
        };
        let other_times: Vec<f64> = other_points.iter().map(|(t, _)| *t).collect();
        assert!((other_times[0] - 0.0).abs() < 1e-12);
        assert!((other_times[1] - 0.2).abs() < 1e-12);
        // Duration = the latest keyframe across channels (Sensor's 0.3 s).
        assert!(
            (log.duration_s() - 0.3).abs() < 1e-12,
            "{}",
            log.duration_s()
        );
    }

    #[test]
    fn from_ld_invalid_datatype_fails_loud() {
        // A channel with an Invalid datatype must fail loud — never a guessed value
        // and never a panic. The upstream reader actually rejects an unrecognized
        // datatype at *metadata-parse* time (an `Unrecognized Datatype` error), so
        // `read_channels` surfaces it before our per-channel guard; either way the
        // result is a fail-loud `UnsupportedConstruct`, not a panic.
        let mut meta = sensor_meta();
        meta.datatype = Datatype::Invalid;
        meta.name = "Bad".to_string();
        // Invalid has size 0, so no sample bytes; data_count stays 0.
        let bytes = synth_one_channel(meta, Vec::new());
        match from_ld(&bytes, "run.ld") {
            Err(EvalError::UnsupportedConstruct { kind, .. }) => {
                assert!(
                    kind.to_lowercase().contains("datatype")
                        || kind.to_lowercase().contains("invalid"),
                    "message names the bad datatype: {kind}"
                );
            }
            other => panic!("expected fail-loud on Invalid datatype, got {other:?}"),
        }
    }

    #[test]
    fn from_ld_f16_datatype_fails_loud() {
        // F16 is unimplemented in the upstream reader; we reject it before the call.
        let mut meta = sensor_meta();
        meta.datatype = Datatype::F16;
        meta.name = "Half".to_string();
        // Build the image but do not let the reader touch the (unimplemented) data:
        // F16 size is 2, so write a single 2-byte sample slot.
        let bytes = synth_one_channel(meta, vec![Sample::I16(0)]);
        match from_ld(&bytes, "run.ld") {
            Err(EvalError::UnsupportedConstruct { kind, .. }) => {
                assert!(kind.contains("F16"), "message names F16: {kind}");
            }
            other => panic!("expected fail-loud on F16 datatype, got {other:?}"),
        }
    }

    #[test]
    fn from_ld_nonzero_offset_fails_loud() {
        // A non-zero sample offset is unsupported by the decode (the upstream
        // `decode_f64` asserts offset == 0); we reject it rather than panic.
        let mut meta = sensor_meta();
        meta.offset = 5;
        let bytes = synth_one_channel(meta, vec![Sample::I16(100), Sample::I16(200)]);
        match from_ld(&bytes, "run.ld") {
            Err(EvalError::UnsupportedConstruct { kind, .. }) => {
                assert!(kind.contains("offset"), "message names the offset: {kind}");
            }
            other => panic!("expected fail-loud on non-zero offset, got {other:?}"),
        }
    }

    #[test]
    fn from_ld_zero_sample_rate_fails_loud() {
        // A zero sample rate has no time base; deriving i/rate would divide by zero.
        let mut meta = sensor_meta();
        meta.sample_rate = 0;
        let bytes = synth_one_channel(meta, vec![Sample::I16(100)]);
        match from_ld(&bytes, "run.ld") {
            Err(EvalError::UnsupportedConstruct { kind, .. }) => {
                assert!(
                    kind.contains("sample rate"),
                    "message names the sample rate: {kind}"
                );
            }
            other => panic!("expected fail-loud on zero sample rate, got {other:?}"),
        }
    }
}
