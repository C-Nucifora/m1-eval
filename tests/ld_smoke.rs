// SPDX-License-Identifier: GPL-3.0-or-later
//! Gated real-`.ld` telemetry smoke test (off the default CI path).
//!
//! This loads a real (proprietary, NOT committed) MoTeC `.ld` telemetry file
//! from a local path given by the `M1_EVAL_LOG_DIR` environment variable, in the
//! same spirit as the EV-M1 project smoke (`M1_EVAL_EVM1_DIR`, see
//! `tests/evm1_smoke.rs`). It is `#[ignore]`-by-default so a normal `cargo test`
//! run never touches proprietary telemetry; run it explicitly when validating the
//! clean-room `.ld` reader against the real corpus:
//!
//! ```text
//! M1_EVAL_LOG_DIR=/path/to/logs \
//!   cargo test --features ld --test ld_smoke -- --ignored
//! ```
//!
//! The directory must contain at least one `.ld` file. The first one found (in
//! sorted order, for determinism) is loaded.
//!
//! ## What it asserts (STRUCTURAL only — no proprietary bytes committed)
//!
//! 1. The header parses: the attached [`m1_eval::Log`] carries a non-empty
//!    provenance string naming a MoTeC/M150-class device (the `.ld` reader folds
//!    the header `device_type` into [`m1_eval::LogMeta::source`]).
//! 2. The channel count is `> 0` (the reader walked the channel linked list).
//! 3. The decode produces physically usable numbers: every channel's time grid is
//!    non-decreasing, at least one channel is *entirely* finite over a non-empty
//!    grid, and the non-finite fraction across the whole log is tiny (< 1%). Real
//!    M1 telemetry stores a few genuine IEEE-754 sentinels (`±inf`/NaN that MoTeC's
//!    own firmware writes for uninitialised channels); those are a *correct* decode
//!    of the float bytes, so the test tolerates a small sentinel fraction while a
//!    byte-misalignment garbage decode (which would flood the log with non-finite
//!    values) is still caught.
//!
//! No channel name, unit, or value is hard-coded — the assertions are purely on
//! shape and finiteness, so nothing about the proprietary log enters the tree.
//!
//! The entire test is `#[cfg(feature = "ld")]`: without the `ld` feature there is
//! no `.ld` reader to exercise, and the file compiles to nothing.
#![cfg(feature = "ld")]

use std::path::PathBuf;

use m1_eval::{Engine, InputKind};

/// Resolve the telemetry directory from `M1_EVAL_LOG_DIR`. Returns `None` (so the
/// test silently passes as a no-op) when the variable is unset — the gating
/// mechanism for "no proprietary telemetry available".
fn log_dir() -> Option<PathBuf> {
    std::env::var_os("M1_EVAL_LOG_DIR").map(PathBuf::from)
}

/// Find the first `.ld` file under `dir` (sorted by name for determinism).
fn first_ld(dir: &std::path::Path) -> Option<PathBuf> {
    let mut ld_files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("ld"))
        })
        .collect();
    ld_files.sort();
    ld_files.into_iter().next()
}

/// Load a real `.ld` through the public [`Engine`] surface. The engine needs a
/// project to construct, so we reuse the committed synthetic `counterfactual`
/// fixture purely as a host — the log itself comes from `M1_EVAL_LOG_DIR`, and we
/// only inspect the attached [`m1_eval::Log`], never run anything against the
/// project. (Loading the log does not touch the project.)
fn host_engine() -> Engine {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/counterfactual");
    Engine::load(&dir.join("Project.m1prj"), None).expect("host fixture project loads")
}

#[test]
#[ignore = "requires M1_EVAL_LOG_DIR pointing at a directory of real (proprietary) .ld telemetry"]
fn real_ld_header_and_channels_decode() {
    let Some(dir) = log_dir() else {
        eprintln!("M1_EVAL_LOG_DIR unset; skipping real-.ld smoke");
        return;
    };
    let Some(ld_path) = first_ld(&dir) else {
        panic!("M1_EVAL_LOG_DIR={} contains no .ld file", dir.display());
    };

    // Load the real `.ld` through the public engine API. All `motec-i2` types stay
    // inside the library; only `m1-eval`'s own `Log` crosses out here.
    let mut engine = host_engine();
    engine
        .load_log(&ld_path)
        .unwrap_or_else(|e| panic!("real .ld {} loads: {e}", ld_path.display()));
    let log = engine.log().expect("log attached after load_log");

    // 1. The header parsed: the reader folds the device type into the provenance
    //    source string. A real MoTeC M1 log advertises an `M1`/`M150`-class device.
    let source = &log.meta.source;
    assert!(
        source.contains("device="),
        "provenance must carry the parsed header device: {source}"
    );
    let device_ok = source.contains("device=M1") || source.contains("M150");
    assert!(
        device_ok,
        "expected a MoTeC M1/M150-class device in the header provenance: {source}"
    );

    // 2. The channel list is non-empty (the reader walked the channel linked list).
    let channel_count = log.channel_names().count();
    assert!(
        channel_count > 0,
        "real .ld decoded zero channels; channel_count must be > 0"
    );
    assert_eq!(
        channel_count, log.meta.channel_count,
        "channel_count metadata must match the decoded channel list"
    );

    // 3. The decode produces *physically usable* numbers. We assert three things
    //    over every channel that has keyframes:
    //
    //    a. the time grid is non-empty and non-decreasing (`i / sample_rate` is
    //       monotone), and
    //    b. every decoded engineering value is numeric (coerces to `f64`), and
    //    c. the decode is not garbage: across the whole log the overwhelming
    //       majority of values are finite, and at least one channel is *entirely*
    //       finite over a non-empty grid (a clean continuous signal).
    //
    //    Real M1 telemetry does contain a small fraction of genuine IEEE-754
    //    sentinels — channels MoTeC's own firmware stores as `±inf`/NaN (e.g. an
    //    uninitialised suspension linearisation, or a divide-by-zero in a
    //    normalised temperature error). Those are a *correct* decode of the float
    //    bytes (`0x7f800000` is canonically `+inf`), not a misread; a blanket
    //    "every value is finite" assertion would wrongly reject them. Instead we
    //    require the non-finite fraction to be tiny (a byte-misalignment garbage
    //    decode would flood the log with non-finite values, not leave 99.9% clean).
    let mut found_all_finite_channel = false;
    let mut total_values: u64 = 0;
    let mut non_finite_values: u64 = 0;
    for series in &log.channels {
        let InputKind::Series(points) = &series.kind else {
            continue;
        };
        if points.is_empty() {
            continue;
        }
        // Time grid: non-empty and non-decreasing (i / sample_rate is monotone).
        let times: Vec<f64> = points.iter().map(|(t, _)| *t).collect();
        assert!(
            times.windows(2).all(|w| w[1] >= w[0]),
            "channel {:?} time grid must be non-decreasing",
            series.channel
        );
        let mut all_finite = true;
        for (_, v) in points {
            let x = v
                .as_f64()
                .unwrap_or_else(|e| panic!("channel {:?} value not numeric: {e}", series.channel));
            total_values += 1;
            if !x.is_finite() {
                non_finite_values += 1;
                all_finite = false;
            }
        }
        if all_finite {
            found_all_finite_channel = true;
        }
    }
    assert!(
        total_values > 0,
        "no channel decoded any value over a non-empty time grid"
    );
    assert!(
        found_all_finite_channel,
        "no channel decoded entirely-finite values over a non-empty time grid"
    );
    // A correct decode leaves the vast majority of values finite; only genuine
    // firmware sentinels are non-finite. Cap the non-finite fraction at 1%.
    let non_finite_fraction = non_finite_values as f64 / total_values as f64;
    assert!(
        non_finite_fraction < 0.01,
        "non-finite fraction {non_finite_fraction:.4} too high \
         ({non_finite_values}/{total_values}); decode is likely garbage, \
         not genuine IEEE sentinels"
    );

    // The overall log duration is finite and non-negative (derived from the grid).
    let duration = log.duration_s();
    assert!(
        duration.is_finite() && duration >= 0.0,
        "log duration must be finite and non-negative, got {duration}"
    );

    // Diagnostic summary for `--nocapture` runs (no proprietary value is asserted;
    // this is operator-facing output, not a committed expectation). Report the
    // channel count, time span, and the first finite (channel, time, value) triple.
    let mut sample_report = None;
    'outer: for series in &log.channels {
        let InputKind::Series(points) = &series.kind else {
            continue;
        };
        for (t, v) in points {
            if let Ok(x) = v.as_f64()
                && x.is_finite()
            {
                sample_report = Some((series.channel.clone(), *t, x));
                break 'outer;
            }
        }
    }
    eprintln!(
        "real-.ld smoke OK: {channel_count} channels, span {duration:.2}s, \
         non-finite {non_finite_values}/{total_values} ({non_finite_fraction:.4}), \
         sample {sample_report:?}"
    );
}
