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
//! ## Milestone status
//!
//! P3-D.T10 lands the feature, the optional `motec-i2` dependency, and the
//! synthetic-`.ld` writer fixture (the committed-CI fixture is generated at test
//! time via [`motec_i2::LDWriter`], so no proprietary bytes are committed). The
//! engineering-unit decode (`Log::from_ld`) lands in P3-D.T11.

#[cfg(test)]
mod tests {
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
}
