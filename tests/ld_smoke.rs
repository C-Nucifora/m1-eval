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
//! 3. At least one channel decodes to a *finite* engineering value over a
//!    *non-empty* time grid (`index / sample_rate` keyframes), proving the
//!    engineering-unit decode + time derivation produced usable numbers.
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

    // 3. At least one channel decodes to a finite engineering value over a
    //    non-empty time grid. We scan for the first channel that has keyframes and
    //    assert its samples are finite and its time axis is non-empty + ascending.
    let mut found_finite = false;
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
        // Every decoded engineering value is finite — never a NaN/inf guess.
        for (_, v) in points {
            let x = v
                .as_f64()
                .unwrap_or_else(|e| panic!("channel {:?} value not numeric: {e}", series.channel));
            assert!(
                x.is_finite(),
                "channel {:?} decoded a non-finite value {x}",
                series.channel
            );
        }
        found_finite = true;
    }
    assert!(
        found_finite,
        "no channel decoded a finite value over a non-empty time grid"
    );

    // The overall log duration is finite and non-negative (derived from the grid).
    let duration = log.duration_s();
    assert!(
        duration.is_finite() && duration >= 0.0,
        "log duration must be finite and non-negative, got {duration}"
    );
}
