// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end checks for the reusable conformance fixture API and CLI.

use assert_cmd::Command;
use m1_eval::{
    ConformanceError, ConformanceOptions, ProvenanceKind, run_conformance_fixture,
    run_conformance_suite,
};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/conformance")
        .join(name)
}

fn project_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn edited_mini_fixture(edit: impl FnOnce(String) -> String) -> (tempfile::TempDir, PathBuf) {
    let template = fixture("synthetic-mini.toml");
    let body = std::fs::read_to_string(template).expect("read template fixture");
    let root = project_fixture("mini")
        .canonicalize()
        .expect("canonical mini fixture");
    let body = body.replace(
        "root = \"../mini\"",
        &format!("root = {:?}", root.to_string_lossy()),
    );
    let body = edit(body);
    let temp = tempfile::tempdir().expect("temp fixture directory");
    let path = temp.path().join("fixture.toml");
    std::fs::write(&path, body).expect("write edited fixture");
    (temp, path)
}

fn edited_types_fixture(edit: impl FnOnce(String) -> String) -> (tempfile::TempDir, PathBuf) {
    let template = fixture("synthetic-types.toml");
    let body = std::fs::read_to_string(template).expect("read template fixture");
    let root = project_fixture("conformance-types")
        .canonicalize()
        .expect("canonical typed fixture");
    let body = body.replace(
        "root = \"../conformance-types\"",
        &format!("root = {:?}", root.to_string_lossy()),
    );
    let body = edit(body);
    let temp = tempfile::tempdir().expect("temp fixture directory");
    let path = temp.path().join("fixture.toml");
    std::fs::write(&path, body).expect("write edited fixture");
    (temp, path)
}

fn edited_initial_state_fixture(
    edit: impl FnOnce(String) -> String,
) -> (tempfile::TempDir, PathBuf) {
    let template = fixture("synthetic-initial-state.toml");
    let body = std::fs::read_to_string(template).expect("read template fixture");
    let root = project_fixture("ratemix")
        .canonicalize()
        .expect("canonical ratemix fixture");
    let body = body.replace(
        "root = \"../ratemix\"",
        &format!("root = {:?}", root.to_string_lossy()),
    );
    let body = edit(body);
    let temp = tempfile::tempdir().expect("temp fixture directory");
    let path = temp.path().join("fixture.toml");
    std::fs::write(&path, body).expect("write edited fixture");
    (temp, path)
}

fn copy_mini_bundle(temp: &tempfile::TempDir) -> PathBuf {
    let source_root = project_fixture("mini");
    let root = temp.path().join("project");
    std::fs::create_dir_all(root.join("Scripts")).expect("create temp project");
    std::fs::copy(
        source_root.join("Project.m1prj"),
        root.join("Project.m1prj"),
    )
    .expect("copy project descriptor");
    std::fs::copy(
        source_root.join("parameters.m1cfg"),
        root.join("parameters.m1cfg"),
    )
    .expect("copy calibration");
    std::fs::copy(
        source_root.join("Scripts/Demo.Update.m1scr"),
        root.join("Scripts/Demo.Update.m1scr"),
    )
    .expect("copy script");
    root
}

#[test]
fn committed_synthetic_fixtures_pass_without_claiming_capture_evidence() {
    let paths = vec![
        fixture("synthetic-mini.toml"),
        fixture("synthetic-initial-state.toml"),
        fixture("synthetic-tables.toml"),
        fixture("synthetic-types.toml"),
    ];
    let reports = run_conformance_suite(&paths, ConformanceOptions::default())
        .expect("synthetic conformance fixtures pass");
    assert_eq!(reports.len(), 4);
    assert!(
        reports
            .iter()
            .all(|report| report.provenance == ProvenanceKind::Synthetic)
    );
}

#[test]
fn independent_unix_time_fixture_covers_the_catalogue_without_claiming_m1_capture() {
    let reports = run_conformance_suite(
        &[fixture("independent-unix-time.toml")],
        ConformanceOptions::default(),
    )
    .expect("independent UnixTime fixture passes");
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].provenance, ProvenanceKind::Independent);
    assert_eq!(reports[0].assertions_checked, 21);

    let error = run_conformance_suite(
        &[fixture("independent-unix-time.toml")],
        ConformanceOptions {
            require_m1_sim_capture: true,
        },
    )
    .expect_err("independent evidence must not satisfy the M1 Sim gate");
    assert!(matches!(error, ConformanceError::MissingM1SimCapture));
}

#[test]
fn json_fixture_uses_the_same_schema_and_runner() {
    let template = fixture("synthetic-mini.toml");
    let body = std::fs::read_to_string(template).expect("read template fixture");
    let root = project_fixture("mini")
        .canonicalize()
        .expect("canonical mini fixture");
    let body = body.replace(
        "root = \"../mini\"",
        &format!("root = {:?}", root.to_string_lossy()),
    );
    let document: toml::Value = toml::from_str(&body).expect("parse TOML fixture as a value");
    let json = serde_json::to_string_pretty(&document).expect("encode equivalent JSON fixture");
    let temp = tempfile::tempdir().expect("temp fixture directory");
    let path = temp.path().join("fixture.json");
    std::fs::write(&path, json).expect("write JSON fixture");

    let report = run_conformance_fixture(&path).expect("equivalent JSON fixture passes");
    assert_eq!(report.name, "synthetic mini calculation");
    assert_eq!(report.steps_checked, 3);
}

#[test]
fn rounded_step_times_are_normalized_before_input_sampling() {
    let (_temp, path) = edited_mini_fixture(|body| {
        let marker = "[[steps]]\ntime_s = 0.01\n\n[[steps.expected]]";
        let replacement = r#"[[steps]]
time_s = 0.0100000000001

[[steps.inputs]]
channel = "Root.Demo.Speed"
value = { type = "floating-point", value = "40" }

[[steps.expected]]"#;
        let body = body.replacen(marker, replacement, 1);
        let later_start = body
            .find("time_s = 0.0100000000001")
            .expect("edited second step exists");
        let (first_step, later_steps) = body.split_at(later_start);
        let later_steps = later_steps.replace(
            "value = { type = \"floating-point\", value = \"50\" }",
            "value = { type = \"floating-point\", value = \"100\" }",
        );
        format!("{first_step}{later_steps}")
    });

    run_conformance_fixture(&path).expect("accepted rounded time uses its exact grid tick");
}

#[test]
fn fixture_input_type_must_match_the_project_channel_family() {
    let (_temp, path) = edited_mini_fixture(|body| {
        body.replacen(
            "value = { type = \"floating-point\", value = \"20\" }",
            "value = { type = \"integer\", value = 20 }",
            1,
        )
    });
    let error = run_conformance_fixture(&path).expect_err("typed mismatch must not be coerced");
    let ConformanceError::InvalidFixture { detail, .. } = error else {
        panic!("expected invalid fixture, got {error}");
    };
    assert!(detail.contains("declares Integer, but the project stores FloatingPoint"));
}

#[test]
fn fixture_string_type_must_match_the_raw_project_channel_family() {
    let (_temp, path) = edited_types_fixture(|body| {
        body.replacen(
            "value = { type = \"string\", value = \"tick zero\" }",
            "value = { type = \"integer\", value = 1 }",
            1,
        )
    });
    let error = run_conformance_fixture(&path).expect_err("typed mismatch must not be coerced");
    let ConformanceError::InvalidFixture { detail, .. } = error else {
        panic!("expected invalid fixture, got {error}");
    };
    assert!(detail.contains("declares Integer, but the project stores String"));
}

#[test]
fn fixture_inputs_must_be_canonical_project_symbols() {
    let (_temp, path) = edited_mini_fixture(|body| {
        let input = r#"[[steps.inputs]]
channel = "Root.Demo.Speed"
value = { type = "floating-point", value = "20" }"#;
        let extra = r#"[[steps.inputs]]
channel = "Root.Not A Project Channel"
value = { type = "floating-point", value = "1" }"#;
        body.replacen(input, &format!("{input}\n\n{extra}"), 1)
    });
    let error = run_conformance_fixture(&path).expect_err("unknown input must not be ignored");
    let ConformanceError::InvalidFixture { detail, .. } = error else {
        panic!("expected invalid fixture, got {error}");
    };
    assert!(detail.contains("is not a project symbol"));
}

#[test]
fn fixture_inputs_must_name_project_channels() {
    let (_temp, path) = edited_mini_fixture(|body| {
        body.replacen(
            "channel = \"Root.Demo.Speed\"",
            "channel = \"Root.Demo.Gain\"",
            1,
        )
    });
    let error = run_conformance_fixture(&path).expect_err("parameters are not scenario channels");
    let ConformanceError::InvalidFixture { detail, .. } = error else {
        panic!("expected invalid fixture, got {error}");
    };
    assert!(detail.contains("Parameter, not a project Channel"));
}

#[test]
fn fixture_inputs_cannot_also_be_expected_outputs() {
    let (_temp, path) = edited_mini_fixture(|body| {
        body.replace(
            "channel = \"Root.Demo.Output\"",
            "channel = \"Root.Demo.Speed\"",
        )
        .replace(
            "value = { type = \"floating-point\", value = \"50\" }",
            "value = { type = \"floating-point\", value = \"20\" }",
        )
    });
    let error = run_conformance_fixture(&path).expect_err("an input is not a computed output");
    let ConformanceError::InvalidFixture { detail, .. } = error else {
        panic!("expected invalid fixture, got {error}");
    };
    assert!(
        detail.contains("expected outputs must be disjoint"),
        "unexpected detail: {detail}"
    );
}

#[test]
fn conditional_static_write_does_not_hide_an_external_initial_seed() {
    let temp = tempfile::tempdir().expect("temp fixture directory");
    let root = copy_mini_bundle(&temp);
    let script = std::fs::read_to_string(root.join("Scripts/Demo.Update.m1scr"))
        .expect("read script")
        .replace("Output = scaled;", "if (false)\n{\n\tOutput = scaled;\n}");
    std::fs::write(root.join("Scripts/Demo.Update.m1scr"), &script)
        .expect("write conditional script");
    let script_hash = format!("{:x}", Sha256::digest(script.as_bytes()));

    let template =
        std::fs::read_to_string(fixture("synthetic-mini.toml")).expect("read template fixture");
    let body = template
        .replace(
            "root = \"../mini\"",
            &format!("root = {:?}", root.to_string_lossy()),
        )
        .replace(
            "687fb0e0ac83f5a0c689a9769d82fbf97de4f3250216ea2e5947a4c18b936f09",
            &script_hash,
        )
        .replacen(
            "[[steps]]\ntime_s = 0.0",
            "[[initial_state]]\nchannel = \"Root.Demo.Output\"\nvalue = { type = \"floating-point\", value = \"50\" }\n\n[[steps]]\ntime_s = 0.0",
            1,
        );
    let path = temp.path().join("fixture.toml");
    std::fs::write(&path, body).expect("write conditional fixture");

    let error = run_conformance_fixture(&path)
        .expect_err("an unexecuted static write must not validate the initial seed");
    let ConformanceError::InvalidFixture { detail, .. } = error else {
        panic!("expected invalid fixture, got {error}");
    };
    assert!(
        detail.contains("externally supplied rather than evaluator-computed"),
        "unexpected detail: {detail}"
    );
}

#[test]
fn undeclared_io_stubs_cannot_supply_conformance_outputs() {
    let temp = tempfile::tempdir().expect("temp fixture directory");
    let root = copy_mini_bundle(&temp);
    let script = std::fs::read_to_string(root.join("Scripts/Demo.Update.m1scr"))
        .expect("read script")
        .replace("Output = scaled;", "Output = Logging.Used(0);");
    std::fs::write(root.join("Scripts/Demo.Update.m1scr"), &script)
        .expect("write IO-backed script");
    let script_hash = format!("{:x}", Sha256::digest(script.as_bytes()));

    let template =
        std::fs::read_to_string(fixture("synthetic-mini.toml")).expect("read template fixture");
    let body = template
        .replace(
            "root = \"../mini\"",
            &format!("root = {:?}", root.to_string_lossy()),
        )
        .replace(
            "687fb0e0ac83f5a0c689a9769d82fbf97de4f3250216ea2e5947a4c18b936f09",
            &script_hash,
        );
    let path = temp.path().join("fixture.toml");
    std::fs::write(&path, body).expect("write IO-backed fixture");

    let error = run_conformance_fixture(&path).expect_err("hidden IO source must not pass");
    let ConformanceError::InvalidFixture { detail, .. } = error else {
        panic!("expected invalid fixture, got {error}");
    };
    assert!(detail.contains("undeclared external source \"Logging.Used\""));
}

#[test]
fn suite_reloads_state_for_every_fixture() {
    let stateful = fixture("synthetic-initial-state.toml");
    let reports =
        run_conformance_suite(&[stateful.clone(), stateful], ConformanceOptions::default())
            .expect("both runs start from the fixture's initial state");
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0].assertions_checked, reports[1].assertions_checked);
}

#[test]
fn schedule_execution_expectations_are_strict() {
    let (_temp, path) =
        edited_initial_state_fixture(|body| body.replacen("plan_order = 1", "plan_order = 0", 1));
    let error = run_conformance_fixture(&path).expect_err("wrong schedule order must fail");
    let ConformanceError::InvalidFixture { detail, .. } = error else {
        panic!("expected invalid fixture, got {error}");
    };
    assert!(
        detail.contains("schedule execution 1 differs")
            && detail.contains("plan_order expected 0, got 1"),
        "unexpected detail: {detail}"
    );
}

#[test]
fn first_output_mismatch_names_step_time_channel_and_values() {
    let (_temp, path) = edited_mini_fixture(|body| {
        body.replacen(
            "value = { type = \"floating-point\", value = \"50\" }",
            "value = { type = \"floating-point\", value = \"51\" }",
            1,
        )
    });
    let error = run_conformance_fixture(&path).expect_err("edited expectation must fail");
    let ConformanceError::Mismatch(mismatch) = error else {
        panic!("expected output mismatch, got {error}");
    };
    assert_eq!(mismatch.step, 0);
    assert_eq!(mismatch.time_s, 0.0);
    assert_eq!(mismatch.channel, "Root.Demo.Output");
    assert!(mismatch.expected.contains("51"));
    assert!(mismatch.actual.as_ref().is_some_and(|value| {
        value
            .m1_scalar()
            .is_ok_and(|scalar| scalar.as_f64() == 50.0)
    }));
}

#[test]
fn project_hash_mismatch_fails_before_evaluation() {
    let (_temp, path) = edited_mini_fixture(|body| {
        body.replacen(
            "9e9dcd16bd3f11a2347b8d273ee1932fd02aa78e54e57ce312b11e605e2f6607",
            "0e9dcd16bd3f11a2347b8d273ee1932fd02aa78e54e57ce312b11e605e2f6607",
            1,
        )
    });
    assert!(matches!(
        run_conformance_fixture(&path),
        Err(ConformanceError::HashMismatch { .. })
    ));
}

#[test]
fn duplicate_script_basenames_are_rejected_as_nondeterministic() {
    let temp = tempfile::tempdir().expect("temp fixture directory");
    let root = copy_mini_bundle(&temp);
    std::fs::create_dir_all(root.join("Duplicate")).expect("create duplicate script directory");
    std::fs::copy(
        root.join("Scripts/Demo.Update.m1scr"),
        root.join("Duplicate/Demo.Update.m1scr"),
    )
    .expect("copy duplicate script");

    let template =
        std::fs::read_to_string(fixture("synthetic-mini.toml")).expect("read template fixture");
    let script_manifest = r#"[[project.files]]
path = "Scripts/Demo.Update.m1scr"
sha256 = "687fb0e0ac83f5a0c689a9769d82fbf97de4f3250216ea2e5947a4c18b936f09""#;
    let duplicate_manifest = r#"[[project.files]]
path = "Duplicate/Demo.Update.m1scr"
sha256 = "687fb0e0ac83f5a0c689a9769d82fbf97de4f3250216ea2e5947a4c18b936f09""#;
    let body = template
        .replace(
            "root = \"../mini\"",
            &format!("root = {:?}", root.to_string_lossy()),
        )
        .replacen(
            script_manifest,
            &format!("{script_manifest}\n\n{duplicate_manifest}"),
            1,
        );
    let path = temp.path().join("fixture.toml");
    std::fs::write(&path, body).expect("write duplicate fixture");

    let error = run_conformance_fixture(&path).expect_err("duplicate basenames must fail");
    let ConformanceError::InvalidFixture { detail, .. } = error else {
        panic!("expected invalid fixture, got {error}");
    };
    assert!(detail.contains("duplicate script basename"));
}

#[test]
fn cli_runs_a_repeatable_fixture_suite() {
    let assert = Command::cargo_bin("m1-eval")
        .unwrap()
        .arg("--conformance")
        .arg(fixture("synthetic-mini.toml"))
        .arg("--conformance")
        .arg(fixture("synthetic-initial-state.toml"))
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(stdout.contains("PASS synthetic mini calculation"));
    assert!(stdout.contains("PASS synthetic initial-state counters"));
    assert!(stdout.contains("conformance: 2 fixture(s) passed"));
}

#[test]
fn cli_rejects_conformance_mixed_with_a_normal_project_action() {
    Command::cargo_bin("m1-eval")
        .unwrap()
        .arg("--conformance")
        .arg(fixture("synthetic-mini.toml"))
        .arg("--project")
        .arg(project_fixture("mini/Project.m1prj"))
        .assert()
        .code(2);
}

#[test]
fn real_capture_gate_rejects_a_synthetic_only_suite() {
    let assert = Command::cargo_bin("m1-eval")
        .unwrap()
        .arg("--conformance")
        .arg(fixture("synthetic-mini.toml"))
        .arg("--require-m1-sim-capture")
        .assert()
        .code(1);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);
    assert!(stderr.contains("no fixture declared `m1-sim` provenance"));
}

#[test]
fn configured_private_m1_sim_fixtures_form_an_optional_gate() {
    let Some(paths) = std::env::var_os("M1_EVAL_M1_SIM_FIXTURES") else {
        return;
    };
    let paths: Vec<PathBuf> = std::env::split_paths(&paths).collect();
    assert!(
        !paths.is_empty(),
        "M1_EVAL_M1_SIM_FIXTURES was set but contained no paths"
    );
    run_conformance_suite(
        &paths,
        ConformanceOptions {
            require_m1_sim_capture: true,
        },
    )
    .expect("configured M1 Sim captures pass conformance");
}
