// SPDX-License-Identifier: GPL-3.0-or-later
//! Clean-room binary `.ld` import, behind the `ld` cargo feature.
//!
//! This module is the only place the MIT [`motec_i2`] crate is used, and it is the
//! only place any `.ld` *file-format* type lives. It reads the *MoTeC i2 `.ld` file
//! format* — an independently reverse-engineered, license-compatible format — and,
//! for tests, writes a synthetic fixture of it. No MoTeC *software* is decompiled
//! and no MoTeC bytes (calibration, firmware, manual text, or sample logs) are
//! committed.
//!
//! [`motec_i2::LDReader`] parses the file *header* (device, venue, driver, the
//! channel-metadata pointer); [`motec_i2::LDWriter`] backs the synthetic CI
//! fixture. The channel-metadata walk and sample decode, however, are done *here*
//! by [`M1Channel`]/[`M1Datatype`] rather than by the crate, because the real M1
//! logs use a channel datatype (`type = 6, size = 4`) that `motec_i2` 0.2 does not
//! recognise (it errors the whole walk). Keeping our own walk lets us decode that
//! datatype while still reusing the crate for the parts it handles correctly. Only
//! `m1-eval`'s own [`crate::Log`] / [`crate::InputSeries`] / [`crate::Value`] cross
//! the module boundary — no `.ld`-format type leaks past the public API.
//!
//! ## The `type = 6, size = 4` datatype — a 32-bit signed integer
//!
//! Every public, independently reverse-engineered `.ld` description (the
//! `gotzl/ldparser` Python parser and the `afonso360/motec-i2` Rust crate) maps the
//! channel "datatype" field as: `0`/`3`/`5` → **signed integer** (width from the
//! size field: 2 → `i16`, 4 → `i32`), `7` → **float** (`f32`/`f16`), `8` → `f64`.
//! Type `6` appears in neither parser. It was identified empirically against the
//! real M1 corpus: in the logs, *every* `type=6` channel carries CAN/ID/status/
//! diagnostic data — `CAN.*.Base ID` = `1024` (`0x400`), `CAN.*.Base Address` =
//! `1536` (`0x600`), accumulator status bit-flags (`0`/`128`/`146`), per-cell/
//! segment indices (`0`..`18`), inverter serial counters, diagnostic codes — all
//! with trivial scaling (`mul = scale = 1`, `dec_places = 0`). Decoded as `i32`
//! these are clean, monotone-where-expected, non-negative integers; decoded as
//! `f32` the same bytes become subnormal nonsense (`~1e-42`). Type `6, size 4` is
//! therefore a **32-bit signed integer**, in the same integer family as `3`/`5` —
//! [`M1Datatype::I32`]. (We decode only what we can justify; an unrecognised
//! datatype is rejected fail-loud, never guessed.)
//!
//! ## Engineering-unit decode
//!
//! Each raw sample's physical (engineering-unit) value is recovered from the
//! channel's `scale` / `dec_places` / `mul` fields (the same formula `motec_i2`
//! documents): `value = raw / scale * 10^-dec_places * mul`. [`from_ld`] applies
//! that decode to produce `f64` values and derives each sample's time as
//! `index / sample_rate` seconds (zero-order-hold keyframes), mapping each `.ld`
//! channel to a [`crate::InputSeries`] under its verbatim name.
//!
//! ## Fail-loud discipline
//!
//! [`from_ld`] never returns a guessed value and never panics across the public
//! API: an unrecognised datatype, a non-zero sample `offset` (the documented
//! decode assumes zero), or a zero `sample_rate` (no time base) each yield an
//! [`EvalError`] instead.

use motec_i2::LDReader;
use std::io::Cursor;

use crate::error::EvalError;
use crate::log::{Log, LogMeta};
use crate::scenario::{InputKind, InputSeries};
use crate::value::Value;

/// Size in bytes of one channel-metadata block in the `.ld` file (matches the
/// layout `motec_i2` documents: four `u32` pointers/counts, a reserved `u16`, the
/// two-`u16` datatype, the `u16` sample rate, the four scaling `u16`/`i16`s, and
/// the fixed-width name/short-name/unit strings + trailing reserved bytes).
const CHANNEL_META_ENTRY_SIZE: usize = 124;

/// A channel sample datatype as it appears on disk in an M1 `.ld` file.
///
/// The on-disk "datatype" is a `(type, size)` pair of `u16`s. We support exactly
/// the interpretations attested by the public `.ld` parsers (`gotzl/ldparser`,
/// `afonso360/motec-i2`) plus the empirically-confirmed M1 `type=6, size=4` → `i32`
/// (see the module docs). Anything else is rejected fail-loud rather than guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum M1Datatype {
    /// 16-bit signed integer. On-disk `(type, size)`: `(0|3|5, 2)`.
    I16,
    /// 32-bit signed integer. On-disk `(type, size)`: `(0|3|5, 4)` and the
    /// M1-specific `(6, 4)` (see module docs — confirmed `i32`, not `f32`).
    I32,
    /// 32-bit IEEE-754 float. On-disk `(type, size)`: `(7, 4)`.
    F32,
}

impl M1Datatype {
    /// Width in bytes of one sample of this datatype on disk.
    fn size(self) -> usize {
        match self {
            M1Datatype::I16 => 2,
            M1Datatype::I32 | M1Datatype::F32 => 4,
        }
    }

    /// Resolve an on-disk `(type, size)` pair to a supported datatype, or `None`
    /// when the pair is not one we can justify decoding.
    ///
    /// Integer families: `0` (beacon), `3`, and `5` are all signed integers in the
    /// public parsers, and `6` is the M1 integer variant confirmed against the real
    /// corpus (module docs). Float family: `7` is a float. `f16` (`(7, 2)`) is not
    /// decoded (no attested-correct half-float decode here); `f64`/`(8, 8)` does not
    /// occur in this corpus and is likewise not guessed.
    fn from_type_and_size(ty: u16, size: u16) -> Option<Self> {
        match (ty, size) {
            (0, 2) | (3, 2) | (5, 2) => Some(M1Datatype::I16),
            (0, 4) | (3, 4) | (5, 4) | (6, 4) => Some(M1Datatype::I32),
            (7, 4) => Some(M1Datatype::F32),
            _ => None,
        }
    }
}

/// One channel's metadata, parsed from a 124-byte block of the `.ld` channel
/// linked list. Holds just what [`from_ld`] needs to locate and decode the
/// channel's samples and map it to an [`InputSeries`].
#[derive(Debug, Clone)]
struct M1Channel {
    /// File offset of the next metadata block (`0` ends the list).
    next_addr: u32,
    /// File offset of this channel's contiguous sample data.
    data_addr: u32,
    /// Number of samples stored for this channel.
    data_count: u32,
    /// Decoded sample datatype.
    datatype: M1Datatype,
    /// Sample rate in Hz (the time grid is `index / sample_rate` seconds).
    sample_rate: u16,
    /// Raw-sample offset. The documented engineering-unit decode assumes `0`; a
    /// non-zero value is rejected fail-loud rather than decoded wrong.
    offset: u16,
    /// Engineering-unit multiplier.
    mul: u16,
    /// Engineering-unit divisor.
    scale: u16,
    /// Decimal-places shift (`10^-dec_places`).
    dec_places: i16,
    /// Verbatim channel name (an M1 channel path; may contain spaces).
    name: String,
    /// Channel unit string (may be empty).
    unit: String,
}

impl M1Channel {
    /// Decode one raw sample value (already widened to `f64`) into engineering
    /// units, applying `scale` / `dec_places` / `mul`. Mirrors the formula the
    /// public `.ld` parsers document: `raw / scale * 10^-dec_places * mul`.
    ///
    /// Callers MUST have rejected a non-zero `offset` first (the decode assumes
    /// zero); this method does not re-check it.
    fn decode(&self, raw: f64) -> f64 {
        raw / self.scale as f64 * 10.0_f64.powi(-self.dec_places as i32) * self.mul as f64
    }
}

/// Read a little-endian `u16` at `off` from `bytes`, bounds-checked.
fn read_u16(bytes: &[u8], off: usize) -> Option<u16> {
    bytes
        .get(off..off + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
}

/// Read a little-endian `i16` at `off` from `bytes`, bounds-checked.
fn read_i16(bytes: &[u8], off: usize) -> Option<i16> {
    read_u16(bytes, off).map(|v| v as i16)
}

/// Read a little-endian `u32` at `off` from `bytes`, bounds-checked.
fn read_u32(bytes: &[u8], off: usize) -> Option<u32> {
    bytes
        .get(off..off + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Read a fixed-width, NUL-terminated string field of `len` bytes at `off`,
/// trimming at the first NUL (lossy on invalid UTF-8, as channel names are ASCII).
fn read_fixed_string(bytes: &[u8], off: usize, len: usize) -> Option<String> {
    let raw = bytes.get(off..off + len)?;
    let end = raw.iter().position(|&c| c == 0).unwrap_or(len);
    Some(String::from_utf8_lossy(&raw[..end]).into_owned())
}

/// Parse the channel-metadata block at file offset `addr`, returning the channel
/// and the byte offset of the next block. Fails loud on a truncated block or an
/// unrecognised datatype (`type`/`size` pair) — never guesses a datatype.
fn parse_channel(bytes: &[u8], addr: u32) -> Result<M1Channel, EvalError> {
    let at = addr as usize;
    let block = bytes.get(at..at + CHANNEL_META_ENTRY_SIZE).ok_or_else(|| {
        EvalError::UnsupportedConstruct {
            kind: format!(
                "`.ld` channel-metadata block at byte {at} is truncated \
                 (need {CHANNEL_META_ENTRY_SIZE} bytes)"
            ),
            at,
        }
    })?;

    // Layout (little-endian), matching the documented `.ld` channel block:
    //   +0  u32 prev_addr     (unused here)
    //   +4  u32 next_addr
    //   +8  u32 data_addr
    //   +12 u32 data_count
    //   +16 u16 reserved
    //   +18 u16 datatype_type
    //   +20 u16 datatype_size
    //   +22 u16 sample_rate
    //   +24 u16 offset
    //   +26 u16 mul
    //   +28 u16 scale
    //   +30 i16 dec_places
    //   +32 [u8;32] name
    //   +64 [u8;8]  short_name (unused here)
    //   +72 [u8;12] unit
    //   +84 [u8;40] reserved
    let field = |o: usize| -> Result<u16, EvalError> {
        read_u16(block, o).ok_or_else(|| EvalError::UnsupportedConstruct {
            kind: format!("`.ld` channel block at byte {at} is malformed"),
            at,
        })
    };
    let next_addr = read_u32(block, 4).expect("block is 124 bytes");
    let data_addr = read_u32(block, 8).expect("block is 124 bytes");
    let data_count = read_u32(block, 12).expect("block is 124 bytes");
    let datatype_type = field(18)?;
    let datatype_size = field(20)?;
    let datatype =
        M1Datatype::from_type_and_size(datatype_type, datatype_size).ok_or_else(|| {
            EvalError::UnsupportedConstruct {
                kind: format!(
                    "`.ld` channel at byte {at} uses an unrecognised datatype \
                 (type {datatype_type}, size {datatype_size}); refusing to guess"
                ),
                at,
            }
        })?;
    let sample_rate = field(22)?;
    let offset = field(24)?;
    let mul = field(26)?;
    let scale = field(28)?;
    let dec_places = read_i16(block, 30).expect("block is 124 bytes");
    let name = read_fixed_string(block, 32, 32).expect("block is 124 bytes");
    let unit = read_fixed_string(block, 72, 12).expect("block is 124 bytes");

    Ok(M1Channel {
        next_addr,
        data_addr,
        data_count,
        datatype,
        sample_rate,
        offset,
        mul,
        scale,
        dec_places,
        name,
        unit,
    })
}

/// Walk the channel-metadata linked list starting at `meta_ptr`, returning every
/// channel in file order. Guards against a cyclic/self-referential `next_addr`
/// (a malformed file must not spin forever).
fn read_channels(bytes: &[u8], meta_ptr: u32) -> Result<Vec<M1Channel>, EvalError> {
    let mut channels = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut next = meta_ptr;
    while next != 0 {
        if !visited.insert(next) {
            return Err(EvalError::UnsupportedConstruct {
                kind: format!("`.ld` channel list has a cycle at byte {next}"),
                at: next as usize,
            });
        }
        let channel = parse_channel(bytes, next)?;
        next = channel.next_addr;
        channels.push(channel);
    }
    Ok(channels)
}

/// Decode one channel's `data_count` samples to engineering-unit `f64` keyframes,
/// each paired with its time `index / sample_rate` seconds (zero-order hold).
///
/// Reads only this channel's contiguous sample region (no whole-file copy), so a
/// large multi-channel log is processed channel-by-channel. The caller has already
/// rejected a non-zero `offset` and a zero `sample_rate`.
fn decode_channel_points(bytes: &[u8], ch: &M1Channel) -> Result<Vec<(f64, Value)>, EvalError> {
    let count = ch.data_count as usize;
    let width = ch.datatype.size();
    let start = ch.data_addr as usize;
    let end = start
        .checked_add(count * width)
        .filter(|&e| e <= bytes.len())
        .ok_or_else(|| EvalError::UnsupportedConstruct {
            kind: format!(
                "`.ld` channel `{}` sample region [{start}, +{}] is out of bounds",
                ch.name,
                count * width
            ),
            at: start,
        })?;
    let data = &bytes[start..end];
    let rate = ch.sample_rate as f64;

    let mut points = Vec::with_capacity(count);
    for (i, chunk) in data.chunks_exact(width).enumerate() {
        let raw = match ch.datatype {
            M1Datatype::I16 => i16::from_le_bytes([chunk[0], chunk[1]]) as f64,
            M1Datatype::I32 => i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f64,
            M1Datatype::F32 => f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f64,
        };
        let t = i as f64 / rate;
        points.push((t, Value::Float(ch.decode(raw))));
    }
    Ok(points)
}

/// Reject a channel whose samples we cannot decode correctly. Returns a fail-loud
/// [`EvalError`] for a non-zero offset, a zero sample rate, or a zero
/// engineering-unit scale (the decode divisor) — otherwise `Ok(())`.
/// (An unrecognised datatype is already rejected at metadata-parse time.)
fn check_decodable(ch: &M1Channel) -> Result<(), EvalError> {
    // The documented engineering-unit decode assumes a zero raw-sample offset.
    if ch.offset != 0 {
        return Err(EvalError::UnsupportedConstruct {
            kind: format!(
                "`.ld` channel `{}` has a non-zero sample offset ({}), \
                 which the engineering-unit decode does not support",
                ch.name, ch.offset
            ),
            at: 0,
        });
    }
    // A zero sample rate has no time base; deriving `i / rate` would divide by zero.
    if ch.sample_rate == 0 {
        return Err(EvalError::UnsupportedConstruct {
            kind: format!(
                "`.ld` channel `{}` has a zero sample rate (no time base)",
                ch.name
            ),
            at: 0,
        });
    }
    // The engineering-unit decode divides by `scale`; a zero scale would divide by
    // zero and produce inf/NaN samples silently.
    if ch.scale == 0 {
        return Err(EvalError::UnsupportedConstruct {
            kind: format!(
                "`.ld` channel `{}` has a zero engineering-unit scale (divisor), \
                 which would produce inf/NaN samples",
                ch.name
            ),
            at: 0,
        });
    }
    Ok(())
}

/// Import a [`Log`] from the bytes of a MoTeC `.ld` file (clean-room reader).
///
/// Reads the header (into [`LogMeta`] provenance) via [`motec_i2::LDReader`], then
/// walks the channel-metadata linked list and every channel's raw samples *here*
/// (so the M1 `type=6` datatype is handled — see the module docs), applies the
/// engineering-unit decode (`scale`/`dec_places`/`mul`), and derives each sample's
/// time as `index / sample_rate` seconds. Each `.ld` channel becomes one
/// [`InputSeries`] of kind [`InputKind::Series`] under its verbatim `.ld` name (M1
/// identifiers may contain spaces, so the name is used whole). `source` records
/// provenance (typically the file path).
///
/// All `.ld`-format types stay inside this function; only `m1-eval`'s own
/// [`Log`]/[`InputSeries`]/[`Value`] cross out.
///
/// Fails loud — never a guessed value, never a panic — when a channel carries an
/// unrecognised datatype, a non-zero sample `offset` (the decode assumes zero), or
/// a zero `sample_rate` (no time base to grid against).
pub fn from_ld(bytes: &[u8], source: impl Into<String>) -> Result<Log, EvalError> {
    // The header parse `motec_i2` does is correct for M1 logs; reuse it (and keep
    // every `.ld`-format type inside this module).
    let mut cursor = Cursor::new(bytes);
    let header =
        LDReader::new(&mut cursor)
            .read_header()
            .map_err(|e| EvalError::UnsupportedConstruct {
                kind: format!("`.ld` header parse failed: {e}"),
                at: 0,
            })?;

    // Walk the channel list ourselves so the M1 `type=6` datatype decodes.
    let metas = read_channels(bytes, header.channel_meta_ptr)?;

    let mut channels = Vec::with_capacity(metas.len());
    let mut units = std::collections::BTreeMap::new();
    let mut duration_s = 0.0_f64;

    for meta in &metas {
        check_decodable(meta)?;
        let points = decode_channel_points(bytes, meta)?;

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
    // transparency (device + venue + driver), without leaking any format type.
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

#[cfg(test)]
mod tests {
    use super::{M1Channel, M1Datatype, check_decodable, from_ld};
    use crate::error::EvalError;
    use crate::scenario::InputKind;
    use crate::value::Value;
    use motec_i2::{ChannelMetadata, Datatype, Header, LDReader, LDWriter, Sample};
    use std::io::Cursor;

    /// The fixed file offset (`0x3448` = 13384) at which `motec_i2`'s writer places
    /// the first channel-metadata block. The reader locates channels via the
    /// header's `channel_meta_ptr`, so a synthetic header MUST advertise this same
    /// offset for the linked-list walk to find the written channels.
    const CHANNEL_META_PTR: u32 = 0x3448;

    /// A bare [`M1Channel`] with trivial scaling, for the unit-level decode tests.
    fn m1_channel(datatype: M1Datatype) -> M1Channel {
        M1Channel {
            next_addr: 0,
            data_addr: 0,
            data_count: 0,
            datatype,
            sample_rate: 10,
            offset: 0,
            mul: 1,
            scale: 1,
            dec_places: 0,
            name: "Test".to_string(),
            unit: String::new(),
        }
    }

    #[test]
    fn type6_size4_resolves_to_i32() {
        // The crux of this module: the real M1 datatype `(type=6, size=4)` that
        // `motec_i2` rejects is a 32-bit signed integer (see module docs).
        assert_eq!(
            M1Datatype::from_type_and_size(6, 4),
            Some(M1Datatype::I32),
            "type=6,size=4 must decode as i32"
        );
        // The integer family (0/3/5) and float (7) keep their attested meanings.
        assert_eq!(M1Datatype::from_type_and_size(3, 2), Some(M1Datatype::I16));
        assert_eq!(M1Datatype::from_type_and_size(3, 4), Some(M1Datatype::I32));
        assert_eq!(M1Datatype::from_type_and_size(5, 4), Some(M1Datatype::I32));
        assert_eq!(M1Datatype::from_type_and_size(7, 4), Some(M1Datatype::F32));
        // Unattested pairs are not guessed.
        assert_eq!(M1Datatype::from_type_and_size(7, 2), None); // f16
        assert_eq!(M1Datatype::from_type_and_size(8, 8), None); // f64
        assert_eq!(M1Datatype::from_type_and_size(42, 4), None);
    }

    #[test]
    fn i32_decode_matches_hand_derived_value() {
        // Hand-derived: a `type=6` CAN base-address channel stores raw i32 `1024`
        // (0x400) with trivial scaling, so the engineering value is exactly 1024.0.
        let ch = m1_channel(M1Datatype::I32);
        assert_eq!(ch.decode(1024.0), 1024.0);
        // With non-trivial scaling: raw 12345 / scale 1 * 10^-2 * mul 1 = 123.45.
        let mut scaled = m1_channel(M1Datatype::I32);
        scaled.dec_places = 2;
        assert!((scaled.decode(12345.0) - 123.45).abs() < 1e-9);
    }

    #[test]
    fn i32_little_endian_decode_of_known_bytes() {
        // 0x00000400 little-endian -> 1024; a status word 0x00000092 -> 146;
        // these are the exact integer values observed in the real corpus.
        let le_1024 = i32::from_le_bytes([0x00, 0x04, 0x00, 0x00]);
        assert_eq!(le_1024, 1024);
        let le_146 = i32::from_le_bytes([0x92, 0x00, 0x00, 0x00]);
        assert_eq!(le_146, 146);
    }

    /// Build a synthetic `.ld` byte image with two channels:
    ///
    /// - `Sensor` — `I16` raw samples with non-trivial `dec_places` scaling, 10 Hz;
    /// - `Other` — `F32` samples, unit-scaled, 5 Hz.
    ///
    /// Written entirely in-memory through [`LDWriter`] into a `Cursor<Vec<u8>>`;
    /// this is the synthetic CI fixture source — no proprietary bytes are involved.
    fn synth_ld_bytes() -> Vec<u8> {
        let header = Header {
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

        // The crate's reader still round-trips the header (we reuse it for that),
        // and our own walk recovers the two channels.
        let mut cursor = Cursor::new(bytes.clone());
        let mut reader = LDReader::new(&mut cursor);
        let header = reader.read_header().expect("header parses");
        assert_eq!(header.device_type, "M1");
        assert_eq!(header.num_channels, 2);
        assert_eq!(header.channel_meta_ptr, CHANNEL_META_PTR);

        let channels = super::read_channels(&bytes, header.channel_meta_ptr).expect("channels");
        assert_eq!(channels.len(), 2, "two channels round-trip");

        let sensor = &channels[0];
        assert_eq!(sensor.name, "Sensor");
        assert_eq!(sensor.unit, "V");
        assert_eq!(sensor.datatype, M1Datatype::I16);
        assert_eq!(sensor.sample_rate, 10);
        assert_eq!(sensor.data_count, 4);

        let other = &channels[1];
        assert_eq!(other.name, "Other");
        assert_eq!(other.unit, "C");
        assert_eq!(other.datatype, M1Datatype::F32);
        assert_eq!(other.sample_rate, 5);
        assert_eq!(other.data_count, 2);
    }

    #[test]
    fn synthetic_ld_sample_values_decode_correctly() {
        // Raw samples decode through our own reader, and the scaling decode turns
        // them into the expected engineering values.
        let bytes = synth_ld_bytes();
        let mut cursor = Cursor::new(bytes.clone());
        let header = LDReader::new(&mut cursor).read_header().expect("header");
        let channels = super::read_channels(&bytes, header.channel_meta_ptr).expect("channels");

        // Sensor: I16 raw {100,200,300,400}, scale=1/dec_places=1/mul=1 -> raw*0.1.
        let sensor = &channels[0];
        let points = super::decode_channel_points(&bytes, sensor).expect("sensor data");
        let values: Vec<f64> = points
            .iter()
            .map(|(_, v)| v.as_f64().expect("numeric"))
            .collect();
        for (got, want) in values.iter().zip([10.0_f64, 20.0, 30.0, 40.0].iter()) {
            assert!((got - want).abs() < 1e-9, "decoded {got} != {want}");
        }

        // Other: F32 {1.5, 2.5}, unit scaling -> identity.
        let other = &channels[1];
        let other_points = super::decode_channel_points(&bytes, other).expect("other data");
        assert!((other_points[0].1.as_f64().unwrap() - 1.5).abs() < 1e-9);
        assert!((other_points[1].1.as_f64().unwrap() - 2.5).abs() < 1e-9);
    }

    /// Build a synthetic single-channel `.ld` image from a fully-specified metadata
    /// + samples pair (so the fail-loud tests can vary one field at a time).
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
        let names: Vec<&str> = log.channel_names().collect();
        assert_eq!(names, vec!["Sensor", "Other"]);
        assert_eq!(log.meta.channel_count, 2);
        assert_eq!(log.meta.units.get("Sensor").map(String::as_str), Some("V"));
        assert_eq!(log.meta.units.get("Other").map(String::as_str), Some("C"));
        assert!(log.meta.source.contains("run.ld"), "{}", log.meta.source);
        assert!(log.meta.source.contains("device=M1"), "{}", log.meta.source);
    }

    #[test]
    fn from_ld_applies_engineering_unit_scaling() {
        let log = from_ld(&synth_ld_bytes(), "run.ld").expect("from_ld parses");
        let sensor = log.series_for("Sensor").expect("Sensor present");
        let InputKind::Series(points) = &sensor.kind else {
            panic!("expected a Series for Sensor");
        };
        let values: Vec<f64> = points
            .iter()
            .map(|(_, v)| v.as_f64().expect("numeric"))
            .collect();
        for (got, want) in values.iter().zip([10.0_f64, 20.0, 30.0, 40.0].iter()) {
            assert!((got - want).abs() < 1e-9, "decoded {got} != {want}");
        }
        assert_eq!(sensor.sample(0.0), Value::Float(10.0));
    }

    #[test]
    fn from_ld_derives_time_from_sample_rate() {
        let log = from_ld(&synth_ld_bytes(), "run.ld").expect("from_ld parses");
        let sensor = log.series_for("Sensor").expect("Sensor present");
        let InputKind::Series(points) = &sensor.kind else {
            panic!("expected a Series for Sensor");
        };
        let times: Vec<f64> = points.iter().map(|(t, _)| *t).collect();
        for (got, want) in times.iter().zip([0.0_f64, 0.1, 0.2, 0.3].iter()) {
            assert!((got - want).abs() < 1e-12, "time {got} != {want}");
        }
        let other = log.series_for("Other").expect("Other present");
        let InputKind::Series(other_points) = &other.kind else {
            panic!("expected a Series for Other");
        };
        assert!((other_points[0].0 - 0.0).abs() < 1e-12);
        assert!((other_points[1].0 - 0.2).abs() < 1e-12);
        assert!(
            (log.duration_s() - 0.3).abs() < 1e-12,
            "{}",
            log.duration_s()
        );
    }

    #[test]
    fn from_ld_unrecognised_datatype_fails_loud() {
        // A channel whose on-disk datatype is one we cannot justify must fail loud
        // — never a guessed value and never a panic. We hand-craft a metadata block
        // carrying an unattested `(type=99, size=4)` pair directly in the bytes.
        let mut bytes = synth_one_channel(sensor_meta(), vec![Sample::I16(0)]);
        // Overwrite the datatype_type u16 at the channel block's +18 with 99.
        let block = CHANNEL_META_PTR as usize;
        bytes[block + 18] = 99;
        bytes[block + 19] = 0;
        match from_ld(&bytes, "run.ld") {
            Err(EvalError::UnsupportedConstruct { kind, .. }) => {
                assert!(
                    kind.to_lowercase().contains("datatype"),
                    "message names the bad datatype: {kind}"
                );
            }
            other => panic!("expected fail-loud on unrecognised datatype, got {other:?}"),
        }
    }

    #[test]
    fn from_ld_nonzero_offset_fails_loud() {
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

    #[test]
    fn zero_scale_fails_loud() {
        // The engineering-unit decode divides by `scale`; a zero scale would yield
        // inf/NaN samples silently. Reject it up front, like the zero-sample-rate
        // guard.
        let mut ch = m1_channel(M1Datatype::I16);
        ch.scale = 0;
        assert!(
            check_decodable(&ch).is_err(),
            "a zero engineering-unit scale must fail loud, not decode to inf/NaN"
        );
    }
}
