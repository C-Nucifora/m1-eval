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

/// Absolute path to the synthetic multirate fixture directory (two periodic
/// rate groups plus an On-Startup function), used by the whole-project tests.
fn multirate_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/multirate")
}

/// A whole-project scenario TOML for the multirate fixture: it pins the base
/// tick at 100 Hz and seeds the external `Seed`/`Slow Out` inputs the schedule
/// reads on the first tick. `mode = "whole-project"` needs no `target`.
const WHOLE_PROJECT_SCENARIO: &str = r#"
mode = "whole-project"
duration_s = 0.04
base_rate_hz = 100.0

[[inputs]]
channel = "Root.MR.Seed"
const = 3.0

[[inputs]]
channel = "Root.MR.Slow Out"
const = 6.0
"#;

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

/// Absolute path to the synthetic counterfactual fixture (a Sensor -> Mid ->
/// Result chain plus an unrelated Other), used by the counterfactual-replay tests.
fn counterfactual_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/counterfactual")
}

/// A self-consistent CSV log for the counterfactual fixture: Sensor=3 so Mid=6,
/// Result=7, Other=42 (what the scripts compute), held over 0.1 s.
const CF_LOG_CSV: &str = "time,Sensor,Mid,Result,Other\n0,3,6,7,42\n0.1,3,6,7,42\n";

#[test]
fn counterfactual_override_writes_trace_and_diff() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("log.csv");
    std::fs::write(&log, CF_LOG_CSV).expect("write log");
    let out = tmp.path().join("trace.json");
    let diff = tmp.path().join("diff.json");
    let cf = counterfactual_dir();

    Command::cargo_bin("m1-eval")
        .unwrap()
        .arg("--project")
        .arg(cf.join("Project.m1prj"))
        .arg("--log")
        .arg(&log)
        .arg("--override")
        .arg("Root.CF.Sensor=5")
        .arg("--out")
        .arg(&out)
        .arg("--diff")
        .arg(&diff)
        .assert()
        .success();

    // Overriding Sensor (3 -> 5) recomputes Mid and Result downstream; Other is
    // unrelated and stays at its logged value.
    let diff_body = std::fs::read_to_string(&diff).expect("diff file written");
    assert!(
        diff_body.contains("Root.CF.Mid"),
        "diff missing Mid: {diff_body}"
    );
    assert!(diff_body.contains("Root.CF.Result"), "diff missing Result");
    assert!(
        diff_body.contains("\"changed\":true"),
        "diff should flag a changed channel: {diff_body}"
    );
    assert!(out.exists(), "trace file written");
}

#[test]
fn override_without_log_is_usage_error() {
    // --override requires --log (clap `requires`): a usage error, exit 2.
    let cf = counterfactual_dir();
    Command::cargo_bin("m1-eval")
        .unwrap()
        .arg("--project")
        .arg(cf.join("Project.m1prj"))
        .arg("--override")
        .arg("Root.CF.Sensor=5")
        .assert()
        .code(2);
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
    assert!(
        trace.contains("50"),
        "trace missing computed value: {trace}"
    );
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
fn whole_project_flag_runs_every_scheduled_channel() {
    // Task 16: `--whole-project` overrides the scenario mode and drives the
    // multi-rate scheduler. The written trace header lists every scheduled
    // channel (the fast `Fast Out` and the slow `Slow Echo`), and the run exits 0.
    let tmp = tempfile::tempdir().unwrap();
    let scenario = write_scenario(tmp.path(), WHOLE_PROJECT_SCENARIO);
    let out = tmp.path().join("trace.csv");
    let mr = multirate_dir();

    Command::cargo_bin("m1-eval")
        .unwrap()
        .arg("--project")
        .arg(mr.join("Project.m1prj"))
        .arg("--scenario")
        .arg(&scenario)
        .arg("--whole-project")
        .arg("--out")
        .arg(&out)
        .assert()
        .success();

    let csv = std::fs::read_to_string(&out).expect("csv trace written");
    let header = csv.lines().next().unwrap_or_default();
    assert!(
        header.contains("Root.MR.Fast Out"),
        "header missing fast channel: {header}"
    );
    assert!(
        header.contains("Root.MR.Slow Echo"),
        "header missing slow channel: {header}"
    );
    // The On-Startup function never runs, so its channel is absent.
    assert!(
        !csv.contains("Root.MR.Started"),
        "startup channel must not appear: {csv}"
    );
}

#[test]
fn whole_project_with_function_is_usage_error() {
    // `--whole-project` is mutually exclusive with `--function`: combining them is
    // a usage error (clap ArgGroup) → exit 2, before the engine even loads.
    let tmp = tempfile::tempdir().unwrap();
    let scenario = write_scenario(tmp.path(), WHOLE_PROJECT_SCENARIO);
    let mr = multirate_dir();

    Command::cargo_bin("m1-eval")
        .unwrap()
        .arg("--project")
        .arg(mr.join("Project.m1prj"))
        .arg("--scenario")
        .arg(&scenario)
        .arg("--whole-project")
        .arg("--function")
        .arg("MR.Fast Reader")
        .assert()
        .code(2);
}

#[test]
fn whole_project_with_target_is_usage_error() {
    // `--whole-project` is mutually exclusive with `--target` too → exit 2.
    let tmp = tempfile::tempdir().unwrap();
    let scenario = write_scenario(tmp.path(), WHOLE_PROJECT_SCENARIO);
    let mr = multirate_dir();

    Command::cargo_bin("m1-eval")
        .unwrap()
        .arg("--project")
        .arg(mr.join("Project.m1prj"))
        .arg("--scenario")
        .arg(&scenario)
        .arg("--whole-project")
        .arg("--target")
        .arg("Root.MR.Fast Out")
        .assert()
        .code(2);
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
