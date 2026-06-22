// SPDX-License-Identifier: GPL-3.0-or-later
//! Deterministic `insta` snapshot tests on [`m1_eval::Trace`] output (M8).
//!
//! Same scenario, same project ⇒ byte-identical trace. The snapshots pin the
//! single-function and dependency-cone runner output (channel columns + time
//! axis, rendered as JSON) so a behaviour change is caught as a snapshot diff.
//! All fixtures are synthetic — no proprietary MoTeC content.

use m1_eval::{Engine, Scenario};
use std::path::Path;

/// Single-function run: `Demo.Update` computes `Output = Speed * Gain` each tick.
#[test]
fn single_function_trace_snapshot() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini");
    let engine = Engine::load(&dir.join("Project.m1prj"), Some(&dir.join("parameters.m1cfg")))
        .expect("mini fixture loads");

    let toml = r#"
mode = "function"
target = "Demo.Update"
duration_s = 0.05
base_rate_hz = 100.0

[[inputs]]
channel = "Root.Demo.Speed"
const = 20.0

[[inputs]]
channel = "Root.Demo.Gain"
const = 2.5
"#;
    let scenario = Scenario::from_toml_str(toml).expect("scenario parses");
    let trace = engine.run(&scenario).expect("run succeeds");

    insta::assert_snapshot!("single_function_trace", trace.to_json());
}

/// Single-function run with a stateful `Integral.Normal`: the running total
/// accumulates over the tick grid, pinned as a snapshot.
#[test]
fn integral_trace_snapshot() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/integral");
    let engine = Engine::load(&dir.join("Project.m1prj"), None).expect("integral fixture loads");

    let toml = r#"
mode = "function"
target = "Acc.Update"
duration_s = 0.5
base_rate_hz = 10.0

[[inputs]]
channel = "Root.Acc.Rate"
const = 2.0
"#;
    let scenario = Scenario::from_toml_str(toml).expect("scenario parses");
    let trace = engine.run(&scenario).expect("run succeeds");

    insta::assert_snapshot!("integral_trace", trace.to_json());
}

/// Dependency-cone run: targeting `Final` pulls in the producer (writes `Mid`)
/// before the consumer (reads `Mid`, writes `Final`). The ordered chain's trace
/// is pinned.
#[test]
fn cone_trace_snapshot() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cone");
    let engine = Engine::load(&dir.join("Project.m1prj"), None).expect("cone fixture loads");

    let toml = r#"
mode = "cone"
target = "Root.Chain.Final"
duration_s = 0.03
base_rate_hz = 100.0

[[inputs]]
channel = "Root.Chain.Raw"
const = 4.0
"#;
    let scenario = Scenario::from_toml_str(toml).expect("scenario parses");
    let trace = engine.run(&scenario).expect("run succeeds");

    insta::assert_snapshot!("cone_trace", trace.to_json());
}

/// Coverage report rendering is deterministic too — snapshot the mini fixture.
#[test]
fn coverage_render_snapshot() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini");
    let engine = Engine::load(&dir.join("Project.m1prj"), Some(&dir.join("parameters.m1cfg")))
        .expect("mini fixture loads");
    let report = engine.coverage();
    insta::assert_snapshot!("coverage_render", report.render());
}
