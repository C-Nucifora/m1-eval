// SPDX-License-Identifier: GPL-3.0-or-later
//! Gated EV-M1 acceptance smoke tests (off the default CI path).
//!
//! These load the real (proprietary, NOT committed) UQR EV-M1 project from a
//! local path given by the `M1_EVAL_EVM1_DIR` environment variable. They are
//! `#[ignore]`-by-default so a normal `cargo test` run never touches proprietary
//! data; run them explicitly when validating against the real corpus:
//!
//! ```text
//! M1_EVAL_EVM1_DIR=/path/to/UQR-EV/01.00.0166 \
//!   cargo test --test evm1_smoke -- --ignored
//! ```
//!
//! The directory must contain a `Project.m1prj` (and optionally a
//! `parameters.m1cfg` calibration alongside it).
//!
//! ## Phase-1.5 acceptance gate ([`evm1_phase15_categories_are_closed`])
//!
//! After P15-A…D, the `--coverage` Unsupported list must no longer contain any of
//! the categories Phase 1.5 closed: pure `Calculate.*` overloads, enum
//! `.AsInteger`, project-object `.Set`/`.Update` methods, or inline user-function
//! calls. This test asserts exactly that against the real project.

use std::path::{Path, PathBuf};

use m1_eval::{Engine, Scenario};

/// Resolve the EV-M1 project directory from `M1_EVAL_EVM1_DIR`. Returns `None`
/// (so the test silently passes as a no-op) when the variable is unset — the
/// gating mechanism for "no proprietary data available".
fn evm1_dir() -> Option<PathBuf> {
    std::env::var_os("M1_EVAL_EVM1_DIR").map(PathBuf::from)
}

/// Load the EV-M1 project + optional calibration into an [`Engine`].
fn load_evm1(dir: &Path) -> Engine {
    let project = dir.join("Project.m1prj");
    assert!(
        project.exists(),
        "M1_EVAL_EVM1_DIR={} has no Project.m1prj",
        dir.display()
    );
    let cfg = dir.join("parameters.m1cfg");
    let cfg = cfg.exists().then_some(cfg);
    Engine::load(&project, cfg.as_deref()).expect("EV-M1 project loads")
}

#[test]
#[ignore = "requires M1_EVAL_EVM1_DIR pointing at the proprietary EV-M1 project"]
fn evm1_phase15_categories_are_closed() {
    let Some(dir) = evm1_dir() else {
        eprintln!("M1_EVAL_EVM1_DIR unset; skipping EV-M1 Phase-1.5 coverage gate");
        return;
    };
    let engine = load_evm1(&dir);
    let report = engine.coverage();

    // Every Phase-1.5 category must be absent from the Unsupported list. We check
    // by the item *name* spelling so a regression in any one category is pinpointed.
    let unsupported: Vec<&str> = report.unsupported.iter().map(|i| i.name.as_str()).collect();

    // 1. Pure Calculate.* overloads (P15-A).
    let calc: Vec<&&str> = unsupported
        .iter()
        .filter(|n| n.starts_with("Calculate."))
        .collect();
    assert!(
        calc.is_empty(),
        "Calculate.* overloads still unsupported: {calc:?}"
    );

    // 2. Enum .AsInteger (P15-B).
    let as_int: Vec<&&str> = unsupported
        .iter()
        .filter(|n| n.ends_with(".AsInteger"))
        .collect();
    assert!(
        as_int.is_empty(),
        ".AsInteger conversions still unsupported: {as_int:?}"
    );

    // 3. Project-object setters / IO writers (P15-C): `<obj>.Set` and `<obj>.Update`.
    let set_update: Vec<&&str> = unsupported
        .iter()
        .filter(|n| n.ends_with(".Set") || n.ends_with(".Update"))
        .collect();
    assert!(
        set_update.is_empty(),
        "project-object .Set/.Update still unsupported: {set_update:?}"
    );

    // 4. Inline user-function calls (P15-D): the two EV-M1 control helpers must be
    //    Supported, never Unsupported (they classify as user functions now).
    for user_fn in ["Slip Control.Update", "Torque Vectoring.Update"] {
        assert!(
            !unsupported.contains(&user_fn),
            "user function {user_fn:?} still unsupported; unsupported={unsupported:?}"
        );
    }
}

/// Phase-2 acceptance gate ([`evm1_whole_project_runs_end_to_end`]).
///
/// Loads the real EV-M1 project and runs the **whole-project multi-rate
/// scheduler** for a short fixed duration. This is the strongest end-to-end check
/// that Phase 1.5 + Phase 2 together make the real corpus runnable: every
/// periodically-scheduled function executes at its own rate, the inline
/// user-function calls evaluate, the enum `.AsInteger` conversions resolve, and
/// the externally-driven CAN/sensor IO falls back to its documented stubs — all
/// without a single fail-loud `EvalError`.
///
/// External inputs are driven by the IO stubs (Task 7) plus any scenario
/// `[[inputs]]`; the schedule's CAN reads stub to documented defaults rather than
/// aborting, so the run completes offline. We assert the trace is non-empty (a
/// real tick grid over the duration) and that it carries the scheduled control
/// channels.
#[test]
#[ignore = "requires M1_EVAL_EVM1_DIR pointing at the proprietary EV-M1 project"]
fn evm1_whole_project_runs_end_to_end() {
    let Some(dir) = evm1_dir() else {
        eprintln!("M1_EVAL_EVM1_DIR unset; skipping EV-M1 whole-project smoke");
        return;
    };
    let engine = load_evm1(&dir);

    // A short whole-project run. No `target` is needed in whole-project mode; the
    // base tick is pinned at 500 Hz (the fastest EV-M1 control rate) so every
    // scheduled rate (500/200/50/10/2 Hz) divides it cleanly. 0.02 s = 10 base
    // ticks — enough for the slower loops to fire at least once.
    let scenario = Scenario::from_toml_str(
        r#"
mode = "whole-project"
duration_s = 0.02
base_rate_hz = 500.0
"#,
    )
    .expect("whole-project scenario parses");

    let trace = engine
        .run(&scenario)
        .expect("EV-M1 whole-project run completes without an EvalError");

    // 0.02 s at 500 Hz = 10 base ticks; the trace has a dense time axis.
    assert_eq!(trace.time.len(), 10, "expected 10 base ticks");
    assert!(
        !trace.channels.is_empty(),
        "whole-project run produced no channel columns"
    );
    // Every channel column is dense over the tick grid (zero-order hold fills the
    // ticks a slow function did not run on).
    for (name, col) in &trace.channels {
        assert_eq!(
            col.len(),
            trace.time.len(),
            "channel {name:?} column is not dense over the tick grid"
        );
    }

    // The scheduled control channels must appear in the trace. `Control.Drive
    // State` is written by the drive-state machine; the inverter torque channels
    // are written by the torque/slip control helpers. We match by suffix so the
    // canonical `Root.…` prefix does not have to be spelled out.
    let has_channel = |needle: &str| trace.channels.keys().any(|k| k.contains(needle));
    assert!(
        has_channel("Drive State"),
        "no Drive State control channel in the trace; channels: {:?}",
        trace.channels.keys().collect::<Vec<_>>()
    );
}
