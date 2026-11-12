// SPDX-License-Identifier: GPL-3.0-or-later
//! Source identity through the public loader and finite single-function runs.

use m1_eval::{Engine, Scenario, Value, load};
use std::path::{Path, PathBuf};

fn fixture(dir: &Path, filename: Option<&str>) -> PathBuf {
    let original = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini/Project.m1prj"),
    )
    .unwrap();
    let attribute = filename
        .map(|name| format!("Filename=\"{name}\" "))
        .unwrap_or_default();
    let project = dir.join("Project.m1prj");
    std::fs::write(
        &project,
        original.replace("Filename=\"Demo.Update.m1scr\" ", &attribute),
    )
    .unwrap();
    project
}

fn script(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn assert_selected(project: &Path, name: &str) {
    let loaded = load(project, None).unwrap();
    assert_eq!(loaded.scripts.len(), 1);
    assert_eq!(loaded.scripts[0].name, name);
    assert_eq!(loaded.scripts[0].cst.source(), "Output = Speed * 2;");
    assert_eq!(
        loaded.project.function_symbol_for_script(name).as_deref(),
        Some("Root.Demo.Update")
    );
    let scenario = Scenario::from_toml_str(
        "mode = \"function\"\ntarget = \"Root.Demo.Update\"\nduration_s = 0.03\nbase_rate_hz = 100.0\n[[inputs]]\nchannel = \"Root.Demo.Speed\"\nconst = 20.0\n",
    ).unwrap();
    let trace = Engine::load(project, None).unwrap().run(&scenario).unwrap();
    assert_eq!(
        trace.channels["Root.Demo.Output"],
        vec![Value::m1_float(40.0); 3]
    );
}

#[test]
fn explicit_relative_filename_selects_one_body_and_preserves_its_key() {
    for basename in ["Actual.m1scr", "Demo.Update.m1scr"] {
        for separator in ["/", "\\"] {
            for reverse in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let key = format!("Scripts{separator}{basename}");
                let project = fixture(temp.path(), Some(&key));
                let mut files = [
                    (format!("Scripts/{basename}"), "Output = Speed * 2;"),
                    (format!("backup/{basename}"), "Output = Speed * 3;"),
                ];
                if reverse {
                    files.reverse();
                }
                for (name, body) in files {
                    script(temp.path(), &name, body);
                }
                assert_selected(&project, &key);
            }
        }
    }
}

#[test]
fn bare_filename_ambiguity_fails_with_stable_physical_paths() {
    for reverse in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let project = fixture(temp.path(), Some("Actual.m1scr"));
        let mut names = ["a/Actual.m1scr", "b/Actual.m1scr"];
        if reverse {
            names.reverse();
        }
        for name in names {
            script(temp.path(), name, "Output = Speed * 2;");
        }
        let error = load(&project, None)
            .err()
            .expect("ambiguous filename must fail")
            .to_string();
        assert!(error.contains("ambiguous script Actual.m1scr"), "{error}");
        let first = temp.path().join("a/Actual.m1scr").display().to_string();
        let second = temp.path().join("b/Actual.m1scr").display().to_string();
        assert!(
            error.find(&first).unwrap() < error.find(&second).unwrap(),
            "{error}"
        );
    }
}

#[test]
fn unique_bare_and_conventional_sources_remain_supported() {
    for filename in [Some("Actual.m1scr"), None] {
        let temp = tempfile::tempdir().unwrap();
        let project = fixture(temp.path(), filename);
        let key = filename.unwrap_or("Demo.Update.m1scr");
        script(temp.path(), &format!("nested/{key}"), "Output = Speed * 2;");
        assert_selected(&project, key);
    }
}

#[test]
fn explicit_source_takes_precedence_over_conventional_and_unused_backups() {
    let temp = tempfile::tempdir().unwrap();
    let project = fixture(temp.path(), Some("Actual.m1scr"));
    script(temp.path(), "Scripts/Actual.m1scr", "Output = Speed * 2;");
    script(
        temp.path(),
        "backup/Demo.Update.m1scr",
        "Output = Speed * 3;",
    );
    script(temp.path(), "a/Unused.m1scr", "Output = Speed * 4;");
    script(temp.path(), "b/Unused.m1scr", "Output = Speed * 5;");
    assert_selected(&project, "Actual.m1scr");
}

#[test]
fn missing_explicit_relative_source_does_not_borrow_a_backup() {
    let temp = tempfile::tempdir().unwrap();
    let project = fixture(temp.path(), Some("Scripts/Demo.Update.m1scr"));
    script(
        temp.path(),
        "backup/Demo.Update.m1scr",
        "Output = Speed * 3;",
    );
    assert!(load(&project, None).unwrap().scripts.is_empty());
}

#[test]
fn malformed_selected_and_unused_sources_still_fail_preflight() {
    for broken in ["Scripts/Actual.m1scr", "backup/Unused.Broken.m1scr"] {
        let temp = tempfile::tempdir().unwrap();
        let project = fixture(temp.path(), Some("Scripts/Actual.m1scr"));
        script(temp.path(), "Scripts/Actual.m1scr", "Output = Speed * 2;");
        script(temp.path(), broken, "Output = 1.0; @@@");
        let error = load(&project, None)
            .err()
            .expect("syntax preflight must fail")
            .to_string();
        assert!(error.contains("syntax error"), "{error}");
        assert!(
            error.contains(Path::new(broken).file_name().unwrap().to_str().unwrap()),
            "{error}"
        );
    }
}
