// SPDX-License-Identifier: GPL-3.0-or-later
//! Synthetic CAN-stub regression test (the CI-safe guard for the gated EV-M1
//! whole-project test).
//!
//! Loads `tests/fixtures/can_stub` — a hand-authored synthetic project with NO
//! proprietary content — whose single scheduled function reads from a CAN bus
//! (`CanComms.RxOpenStandard` to open a handle, `CanComms.GetUnsignedInteger` /
//! `CanComms.GetFloat` to read it). Offline there is no bus, so each Tier-3 IO
//! call returns its documented, type-correct externally-driven default instead of
//! failing loud. This proves the property the gated EV-M1 test depends on — a
//! whole-project run whose scripts read CAN COMPLETES, returning externally-driven
//! stub values — without touching any proprietary corpus, so it runs on plain
//! `cargo test`.

use std::path::Path;

use m1_eval::value::Value;
use m1_eval::{Engine, Scenario};

/// Load the synthetic CAN-stub fixture into an engine (no calibration).
fn can_stub_engine() -> Engine {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/can_stub");
    Engine::load(&dir.join("Project.m1prj"), None).expect("can_stub fixture loads")
}

#[test]
fn whole_project_run_with_can_reads_completes_with_external_stubs() {
    let engine = can_stub_engine();

    // A short whole-project run. The one 100 Hz function reads CAN every tick.
    // 0.05 s at 100 Hz = 5 ticks.
    let scenario = Scenario::from_toml_str(
        r#"
mode = "whole-project"
duration_s = 0.05
base_rate_hz = 100.0
"#,
    )
    .expect("whole-project scenario parses");

    let trace = engine
        .run(&scenario)
        .expect("CAN-reading whole-project run completes without an EvalError");

    // 0.05 s at 100 Hz = 5 ticks; the trace has a dense time axis.
    assert_eq!(trace.time.len(), 5, "expected 5 base ticks");

    // The output channel was written from `CanComms.GetFloat`, which has no offline
    // value — so it holds the documented FloatingPoint stub 0.0, dense over the
    // whole grid (zero-order hold), on every tick.
    let bus_value = trace
        .channels
        .get("Root.CanDemo.Bus Value")
        .expect("Bus Value channel present in the trace");
    assert_eq!(
        bus_value,
        &vec![Value::Float(0.0); 5],
        "Bus Value holds the CanComms.GetFloat external stub on every tick"
    );

    // The CAN reads are flagged externally driven (simulated input, not evaluated
    // output) so a consumer can distinguish them.
    assert!(
        trace.is_external("CanComms.GetFloat"),
        "CanComms.GetFloat is flagged externally driven"
    );
    assert!(
        trace.is_external("CanComms.RxOpenStandard"),
        "CanComms.RxOpenStandard is flagged externally driven"
    );
    assert!(
        trace.is_external("CanComms.GetUnsignedInteger"),
        "CanComms.GetUnsignedInteger is flagged externally driven"
    );
}

#[test]
fn single_function_run_with_can_reads_is_deterministic() {
    let engine = can_stub_engine();

    // The same fixture run as a single function over a few ticks; the CAN stubs are
    // deterministic (no wall-clock / RNG), so two runs produce identical traces.
    let scenario = Scenario::from_toml_str(
        r#"
mode = "function"
target = "Root.CanDemo.Read"
duration_s = 0.03
base_rate_hz = 100.0
"#,
    )
    .expect("function scenario parses");

    let first = engine.run(&scenario).expect("first run completes");
    let second = engine.run(&scenario).expect("second run completes");

    assert_eq!(first.time.len(), 3, "expected 3 ticks");
    // Determinism: identical channel columns across runs.
    assert_eq!(
        first.channels, second.channels,
        "the CAN-stub run is deterministic"
    );
    // The CAN-read output is the externally-driven float stub on every tick.
    assert_eq!(
        first.channels.get("Root.CanDemo.Bus Value"),
        Some(&vec![Value::Float(0.0); 3])
    );
}
