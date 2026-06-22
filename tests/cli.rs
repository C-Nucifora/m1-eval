// SPDX-License-Identifier: GPL-3.0-or-later
//! Integration tests for the `m1-eval` CLI binary.
//!
//! These drive the compiled binary end-to-end with `assert_cmd` against the
//! synthetic `tests/fixtures/mini` project, asserting the shared toolchain
//! exit-code contract: `0` on a clean run, `1` when the engine ran but reported
//! an evaluation error, `2` on a usage error (bad/missing arguments). They also
//! check that `--out` writes a trace file containing the expected output channel
//! and that `--coverage` prints the static report.

use assert_cmd::Command;
use std::path::{Path, PathBuf};

/// Absolute path to the synthetic mini fixture directory.
fn mini_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini")
}

/// A scenario TOML that runs the mini `Demo.Update` function for three ticks with
/// a constant speed and the calibrated gain, so `Output = Speed * Gain = 50`.
const SCENARIO: &str = r#"
mode = "function"
target = "Demo.Update"
duration_s = 0.03
base_rate_hz = 100.0

[[inputs]]
channel = "Root.Demo.Speed"
const = 20.0
"#;

/// Write the scenario TOML into a temp dir and return its path (and the dir guard).
fn write_scenario(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("scenario.toml");
    std::fs::write(&path, body).expect("write scenario");
    path
}

#[test]
fn ok_run_writes_trace_with_output_channel() {
    let tmp = tempfile::tempdir().unwrap();
    let scenario = write_scenario(tmp.path(), SCENARIO);
    let out = tmp.path().join("trace.json");
    let mini = mini_dir();

    Command::cargo_bin("m1-eval")
        .unwrap()
        .arg("--project")
        .arg(mini.join("Project.m1prj"))
        .arg("--config")
        .arg(mini.join("parameters.m1cfg"))
        .arg("--scenario")
        .arg(&scenario)
        .arg("--out")
        .arg(&out)
        .assert()
        .success();

    let trace = std::fs::read_to_string(&out).expect("trace file written");
    // The output column appears with the computed value 50.
    assert!(
        trace.contains("Root.Demo.Output"),
        "trace missing output channel: {trace}"
    );
    assert!(trace.contains("50"), "trace missing computed value: {trace}");
}

#[test]
fn csv_out_is_inferred_from_extension() {
    let tmp = tempfile::tempdir().unwrap();
    let scenario = write_scenario(tmp.path(), SCENARIO);
    let out = tmp.path().join("trace.csv");
    let mini = mini_dir();

    Command::cargo_bin("m1-eval")
        .unwrap()
        .arg("--project")
        .arg(mini.join("Project.m1prj"))
        .arg("--config")
        .arg(mini.join("parameters.m1cfg"))
        .arg("--scenario")
        .arg(&scenario)
        .arg("--out")
        .arg(&out)
        .assert()
        .success();

    let csv = std::fs::read_to_string(&out).expect("csv trace written");
    let first = csv.lines().next().unwrap_or_default();
    assert!(first.starts_with("time"), "csv header malformed: {first}");
    assert!(
        csv.contains("Root.Demo.Output"),
        "csv missing output channel: {csv}"
    );
}

#[test]
fn coverage_prints_report() {
    let mini = mini_dir();
    let assert = Command::cargo_bin("m1-eval")
        .unwrap()
        .arg("--project")
        .arg(mini.join("Project.m1prj"))
        .arg("--config")
        .arg(mini.join("parameters.m1cfg"))
        .arg("--coverage")
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    // The report labels every bucket deterministically.
    assert!(stdout.contains("Supported:"), "coverage stdout: {stdout}");
    assert!(stdout.contains("Stubbed:"), "coverage stdout: {stdout}");
    assert!(stdout.contains("Unsupported:"), "coverage stdout: {stdout}");
}

#[test]
fn missing_project_is_usage_error() {
    // No --project and no Project.m1prj discoverable from a temp cwd → exit 2.
    let tmp = tempfile::tempdir().unwrap();
    let scenario = write_scenario(tmp.path(), SCENARIO);

    Command::cargo_bin("m1-eval")
        .unwrap()
        .current_dir(tmp.path())
        .arg("--scenario")
        .arg(&scenario)
        .assert()
        .code(2);
}

#[test]
fn run_without_scenario_or_coverage_is_usage_error() {
    // A run needs either --scenario (to evaluate) or --coverage (static report).
    let mini = mini_dir();
    Command::cargo_bin("m1-eval")
        .unwrap()
        .arg("--project")
        .arg(mini.join("Project.m1prj"))
        .assert()
        .code(2);
}

#[test]
fn nonexistent_project_path_is_eval_error() {
    // A --project that points at a missing file: the engine tried to load and
    // failed → exit 1 (ran-but-reported), not a usage error.
    let tmp = tempfile::tempdir().unwrap();
    let scenario = write_scenario(tmp.path(), SCENARIO);

    Command::cargo_bin("m1-eval")
        .unwrap()
        .arg("--project")
        .arg(tmp.path().join("does-not-exist.m1prj"))
        .arg("--scenario")
        .arg(&scenario)
        .assert()
        .code(1);
}

#[test]
fn unresolved_target_is_eval_error() {
    // A scenario naming a function that does not exist in the project: the engine
    // ran and failed loud → exit 1.
    let tmp = tempfile::tempdir().unwrap();
    let body = r#"
mode = "function"
target = "Nope.DoesNotExist"
duration_s = 0.01
base_rate_hz = 100.0
"#;
    let scenario = write_scenario(tmp.path(), body);
    let mini = mini_dir();

    Command::cargo_bin("m1-eval")
        .unwrap()
        .arg("--project")
        .arg(mini.join("Project.m1prj"))
        .arg("--config")
        .arg(mini.join("parameters.m1cfg"))
        .arg("--scenario")
        .arg(&scenario)
        .assert()
        .code(1);
}
