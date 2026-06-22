// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end statement-executor integration test (milestone M7, Task 22).
//!
//! Loads the synthetic `tests/fixtures/mini` project and runs its
//! `Demo.Update.m1scr` script body through the public statement executor over a
//! seeded [`Env`], asserting the computed output channel. This is the
//! whole-script integration check the runner (M8) later wraps in a tick loop;
//! here we drive a single execution directly against the engine internals.

use m1_eval::env::{Env, StateStore};
use m1_eval::expr::EvalCtx;
use m1_eval::stmt::exec_script;
use m1_eval::trace::Trace;
use m1_eval::value::Value;
use std::path::Path;

/// Run `Demo.Update.m1scr` once with a seeded `Speed` and the calibrated `Gain`.
///
/// The fixture script is:
/// ```text
/// local scaled = Speed * Gain;
/// Output = scaled;
/// ```
/// With `Speed = 20` and `Gain = 2.5` (from `parameters.m1cfg`), the output is
/// `Output = 20 * 2.5 = 50`.
#[test]
fn demo_update_script_computes_output() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini");
    let loaded = m1_eval::load(
        &dir.join("Project.m1prj"),
        Some(&dir.join("parameters.m1cfg")),
    )
    .expect("mini fixture loads");

    // The one discovered script is the demo updater.
    let script = loaded
        .scripts
        .iter()
        .find(|s| s.name == "Demo.Update.m1scr")
        .expect("Demo.Update.m1scr discovered");

    let mut env = Env::new();
    env.set("Root.Demo.Speed", Value::Float(20.0));
    let mut state = StateStore::new();
    let mut trace = Trace::new();
    trace.push_tick(0.0);

    let root = script.cst.root();
    let mut ctx = EvalCtx {
        project: &loaded.project,
        calib: &loaded.calib,
        env: &mut env,
        state: &mut state,
        group: Some("Root.Demo"),
        fn_symbol: Some("Root.Demo.Update"),
        script_name: &script.name,
        dt: 0.01,
        scripts: &loaded.scripts,
        depth: 0,
        trace: Some(&mut trace),
    };

    exec_script(&root, &mut ctx).expect("script executes end to end");

    // The output channel holds the scaled speed.
    assert_eq!(env.get("Root.Demo.Output"), Some(&Value::Float(50.0)));
    // And it was recorded into the trace's channel column.
    assert_eq!(
        trace.channels.get("Root.Demo.Output"),
        Some(&vec![Value::Float(50.0)])
    );
}

/// A second execution with different inputs recomputes the output — the executor
/// is a pure function of the seeded environment plus calibration (determinism).
#[test]
fn demo_update_is_deterministic_in_inputs() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini");
    let loaded = m1_eval::load(
        &dir.join("Project.m1prj"),
        Some(&dir.join("parameters.m1cfg")),
    )
    .expect("mini fixture loads");
    let script = &loaded.scripts[0];
    let root = script.cst.root();

    let run = |speed: f64| -> Value {
        let mut env = Env::new();
        env.set("Root.Demo.Speed", Value::Float(speed));
        let mut state = StateStore::new();
        let mut ctx = EvalCtx {
            project: &loaded.project,
            calib: &loaded.calib,
            env: &mut env,
            state: &mut state,
            group: Some("Root.Demo"),
            fn_symbol: Some("Root.Demo.Update"),
            script_name: &script.name,
            dt: 0.01,
            scripts: &loaded.scripts,
            depth: 0,
            trace: None,
        };
        exec_script(&root, &mut ctx).expect("script executes");
        env.get("Root.Demo.Output").cloned().expect("output set")
    };

    assert_eq!(run(20.0), Value::Float(50.0));
    assert_eq!(run(8.0), Value::Float(20.0));
    // Same input, same output (no wall-clock / RNG).
    assert_eq!(run(20.0), run(20.0));
}
