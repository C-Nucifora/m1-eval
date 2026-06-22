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

use m1_eval::Engine;

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
    let unsupported: Vec<&str> = report
        .unsupported
        .iter()
        .map(|i| i.name.as_str())
        .collect();

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
