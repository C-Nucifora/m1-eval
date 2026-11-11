// SPDX-License-Identifier: GPL-3.0-or-later
//! Keep allocator-abort regressions inside memory- and time-limited children.

#[cfg(target_os = "linux")]
#[test]
fn enormous_expand_ranges_fail_without_aborting_or_hanging() {
    use assert_cmd::Command;
    use std::path::Path;
    use std::time::Duration;

    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("Project.m1prj");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mini/Project.m1prj"),
        &project,
    )
    .unwrap();
    let scenario = temp.path().join("run.toml");
    std::fs::write(
        &scenario,
        "mode = \"function\"\ntarget = \"Demo.Update\"\nduration_s = 0.01\nbase_rate_hz = 100.0\n",
    )
    .unwrap();
    for bounds in [
        "0 to 2147483647",
        "2147483647 to 0",
        "-2147483648 to 2147483647",
    ] {
        std::fs::write(
            temp.path().join("Demo.Update.m1scr"),
            format!("expand (i = {bounds}) {{ Output = 1.0; }}"),
        )
        .unwrap();
        let result = Command::new("bash")
            .args([
                "-c",
                "ulimit -c 0; ulimit -v 524288; ulimit -t 3; exec \"$@\"",
                "m1-eval-expand-test",
            ])
            .arg(env!("CARGO_BIN_EXE_m1-eval"))
            .arg("--project")
            .arg(&project)
            .arg("--scenario")
            .arg(&scenario)
            .timeout(Duration::from_secs(5))
            .assert()
            .code(1);
        let output = result.get_output();
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("expand iteration budget"));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn source_and_combined_call_nesting_fail_without_stack_overflow() {
    use assert_cmd::Command;
    use std::path::Path;
    use std::time::Duration;

    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("Project.m1prj");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/userfn/Project.m1prj"),
        &project,
    )
    .unwrap();
    let scenario = temp.path().join("run.toml");
    std::fs::write(&scenario, "mode = \"function\"\ntarget = \"Caller.Update\"\nduration_s = 0.01\nbase_rate_hz = 100.0\n").unwrap();
    for (depth, expected) in [(32768, "source nesting"), (512, "evaluation nesting")] {
        std::fs::write(
            temp.path().join("Caller.Update.m1scr"),
            "Output = Recur.Loop(1);",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("Recur.Loop.m1scr"),
            format!(
                "Out = {}Recur.Loop(In.n){};",
                "(".repeat(depth),
                ")".repeat(depth)
            ),
        )
        .unwrap();
        let result = Command::new("bash")
            .args([
                "-c",
                "ulimit -c 0; ulimit -v 524288; ulimit -t 3; ulimit -s 2048; exec \"$@\"",
                "m1-eval-depth-test",
            ])
            .arg(env!("CARGO_BIN_EXE_m1-eval"))
            .arg("--project")
            .arg(&project)
            .arg("--scenario")
            .arg(&scenario)
            .timeout(Duration::from_secs(5))
            .assert()
            .code(1);
        let output = result.get_output();
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn mixed_evaluation_shapes_fail_with_a_small_stack() {
    use assert_cmd::Command;
    use std::path::Path;
    use std::time::Duration;

    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("Project.m1prj");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/userfn/Project.m1prj"),
        &project,
    )
    .unwrap();
    let scenario = temp.path().join("run.toml");
    std::fs::write(&scenario, "mode = \"function\"\ntarget = \"Caller.Update\"\nduration_s = 0.01\nbase_rate_hz = 100.0\n").unwrap();
    std::fs::write(
        temp.path().join("Caller.Update.m1scr"),
        "Output = Recur.Loop(1);",
    )
    .unwrap();
    for source in [
        format!("Out = {}1.0;", "1.0 + ".repeat(900)),
        format!("Out = {}1.0;", "- ".repeat(900)),
        format!(
            "Out = {}1.0{};",
            "Calculate.Abs(".repeat(450),
            ")".repeat(450)
        ),
        format!(
            "{}{{ Out = Recur.Loop(In.n); }}",
            "if (false) {} else ".repeat(100)
        ),
        format!(
            "{}Out = Recur.Loop(In.n);{}",
            "expand (i = 1 to 1) {".repeat(200),
            "}".repeat(200)
        ),
    ] {
        std::fs::write(temp.path().join("Recur.Loop.m1scr"), &source).unwrap();
        let result = Command::new("bash")
            .args([
                "-c",
                "ulimit -c 0; ulimit -v 524288; ulimit -t 3; ulimit -s 2048; exec \"$@\"",
                "m1-eval-mixed-depth-test",
            ])
            .arg(env!("CARGO_BIN_EXE_m1-eval"))
            .arg("--project")
            .arg(&project)
            .arg("--scenario")
            .arg(&scenario)
            .timeout(Duration::from_secs(5))
            .assert()
            .code(1);
        let output = result.get_output();
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("evaluation nesting"),
            "{output:?}"
        );
    }
}
