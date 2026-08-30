// SPDX-License-Identifier: GPL-3.0-or-later
//! Synthetic end-to-end coverage for the deterministic virtual RS232 adapter.
//! No proprietary project data or captured M1 serial traffic is used here.

use m1_eval::{
    AdapterReply, Engine, EvalError, HardwareAdapter, HardwareCall, HardwareValueSource, M1Scalar,
    Scenario, SerialDirection, Value,
};
use std::path::Path;

fn engine() -> Engine {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/serial/Project.m1prj");
    Engine::load(&fixture, None).expect("synthetic serial fixture loads")
}

fn whole_project_scenario() -> Scenario {
    Scenario::from_toml_str(
        r#"
mode = "whole-project"
duration_s = 0.05
base_rate_hz = 100.0

[[serial.rx]]
time_s = 0.0
port = 0
bytes = [0x11]

[[serial.rx]]
time_s = 0.01
port = 0
bytes = [0x22]

[[serial.rx]]
time_s = 0.02
port = 0
bytes = [0x33]

[[serial.rx]]
time_s = 0.02
port = 0
bytes = [0x34]
"#,
    )
    .expect("virtual serial scenario parses")
}

fn unsigned(value: &Value) -> u32 {
    match value {
        Value::M1(M1Scalar::UnsignedInteger(value)) => *value,
        other => panic!("expected M1 unsigned value, got {other:?}"),
    }
}

#[test]
fn whole_project_serial_is_timed_rate_gated_nested_and_fresh_per_run() {
    let engine = engine();
    let scenario = whole_project_scenario();
    let first = engine.run(&scenario).expect("first serial run succeeds");
    let second = engine.run(&scenario).expect("second serial run succeeds");

    assert_eq!(first.to_json(), second.to_json(), "fresh runs are stable");
    assert_eq!(
        first.channels["Root.Serial Test.Init OK"],
        vec![Value::Bool(true); 5]
    );
    assert_eq!(
        first.channels["Root.Serial Test.Diagnostic"],
        vec![Value::m1_integer(1); 5]
    );
    assert_eq!(
        first.channels["Root.Serial Test.Startup Byte"],
        vec![Value::m1_integer(0x11); 5]
    );

    let handle_a = unsigned(&first.channels["Root.Serial Test.Handle A"][0]);
    let handle_b = unsigned(&first.channels["Root.Serial Test.Handle B"][0]);
    assert_ne!(handle_a, 0);
    assert_ne!(handle_b, 0);
    assert_ne!(handle_a, handle_b);
    assert!(
        first.channels["Root.Serial Test.Handle A"]
            .iter()
            .all(|value| unsigned(value) == handle_a)
    );
    assert!(
        first.channels["Root.Serial Test.Handle B"]
            .iter()
            .all(|value| unsigned(value) == handle_b)
    );

    // Receiver A runs at 100 Hz. Receiver B and the nested helper run at 50 Hz,
    // yet each handle has an independent cursor over the same virtual port.
    assert_eq!(
        first.channels["Root.Serial Test.Byte A"],
        vec![
            Value::m1_integer(0x11),
            Value::m1_integer(0x22),
            Value::m1_integer(0x33),
            Value::m1_integer(0x33),
            Value::m1_integer(0x33),
        ]
    );
    assert_eq!(
        first.channels["Root.Serial Test.Byte B"],
        vec![
            Value::m1_integer(0x11),
            Value::m1_integer(0x11),
            Value::m1_integer(0x22),
            Value::m1_integer(0x22),
            Value::m1_integer(0x22),
        ]
    );
    assert_eq!(
        first.channels["Root.Serial Test.Nested Byte"],
        vec![
            Value::m1_integer(0x11),
            Value::m1_integer(0x11),
            Value::m1_integer(0x22),
            Value::m1_integer(0x22),
            Value::m1_integer(-1),
        ]
    );
    assert_eq!(
        first.channels["Root.Serial Test.Available A"],
        vec![
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(false),
        ]
    );

    let tx = first
        .serial
        .iter()
        .filter(|event| event.direction == SerialDirection::Tx)
        .collect::<Vec<_>>();
    assert_eq!(tx.len(), 3);
    assert_eq!(
        tx.iter()
            .map(|event| event.time.elapsed_s)
            .collect::<Vec<_>>(),
        vec![0.0, 0.02, 0.04]
    );
    assert!(tx.iter().all(|event| {
        event.port == 0
            && event.bytes == [0x30, 0x35, 0x3f, 0xc0, 0, 0, b'O', b'K']
            && event.site.script() == "Serial Test.Transmit.m1scr"
    }));
    let rx = first
        .serial
        .iter()
        .filter(|event| event.direction == SerialDirection::Rx)
        .collect::<Vec<_>>();
    assert!(rx.iter().any(|event| {
        event.time.phase == m1_eval::EvalPhase::Startup
            && event.time.elapsed_s == 0.0
            && event.bytes == [0x11]
            && event.site.script() == "Serial Test.Startup Receive.m1scr"
    }));
    assert!(rx.iter().any(|event| {
        event.time.phase == m1_eval::EvalPhase::Periodic
            && event.time.elapsed_s == 0.02
            && event.bytes == [0x33, 0x34]
            && event.site.script() == "Serial Test.Receive A.m1scr"
    }));

    assert!(first.hardware.iter().any(|item| {
        item.canonical_call() == "Serial.Receive"
            && item.source == HardwareValueSource::VirtualSerialRx
    }));
    assert!(first.hardware.iter().any(|item| {
        item.canonical_call() == "Serial.Transmit"
            && item.source == HardwareValueSource::VirtualSerial
    }));
    assert!(first.external.contains("Serial.Receive"));
    assert!(!first.external.contains("Serial.Transmit"));

    let json: serde_json::Value =
        serde_json::from_str(&first.to_json()).expect("trace JSON is valid");
    assert_eq!(
        json["serial"].as_array().map(Vec::len),
        Some(first.serial.len())
    );
    assert!(
        !first.to_csv().contains("serial"),
        "CSV remains channel-only"
    );
}

#[derive(Default)]
struct StatusAdapter {
    calls: Vec<String>,
    diagnostic: i32,
}

impl HardwareAdapter for StatusAdapter {
    fn call(&mut self, call: &HardwareCall) -> Result<AdapterReply, EvalError> {
        self.calls.push(call.canonical_name());
        if call.canonical_name() == "Serial.PortDiagnostic" {
            Ok(Value::m1_integer(self.diagnostic).into())
        } else {
            Ok(AdapterReply::Unhandled)
        }
    }
}

fn init_scenario(extra: &str) -> Scenario {
    Scenario::from_toml_str(&format!(
        "mode = \"function\"\ntarget = \"Serial Test.Init\"\nduration_s = 0.01\nbase_rate_hz = 100.0\n{extra}"
    ))
    .expect("init scenario parses")
}

#[test]
fn io_override_then_external_adapter_precede_the_virtual_model() {
    let engine = engine();
    let mut adapter = StatusAdapter {
        diagnostic: 9,
        ..Default::default()
    };
    let trace = engine
        .run_with_adapter(&init_scenario(""), &mut adapter)
        .expect("external adapter supplies diagnostic");
    assert_eq!(
        trace.channels["Root.Serial Test.Diagnostic"],
        vec![Value::m1_integer(9)]
    );
    assert_eq!(
        adapter.calls,
        vec!["Serial.PortInit", "Serial.PortDiagnostic"]
    );
    assert!(trace.hardware.iter().any(|item| {
        item.canonical_call() == "Serial.PortDiagnostic"
            && item.source == HardwareValueSource::Adapter
    }));

    let mut adapter = StatusAdapter {
        diagnostic: 9,
        ..Default::default()
    };
    let scenario =
        init_scenario("\n[[io]]\ncall = \"Serial.PortDiagnostic\"\nconst = { integer = 7 }\n");
    let trace = engine
        .run_with_adapter(&scenario, &mut adapter)
        .expect("scenario override wins");
    assert_eq!(
        trace.channels["Root.Serial Test.Diagnostic"],
        vec![Value::m1_integer(7)]
    );
    assert_eq!(adapter.calls, vec!["Serial.PortInit"]);
    assert!(trace.hardware.iter().any(|item| {
        item.canonical_call() == "Serial.PortDiagnostic"
            && item.source == HardwareValueSource::ScenarioWildcard
    }));

    let mut adapter = StatusAdapter {
        diagnostic: 9,
        ..Default::default()
    };
    let scenario = init_scenario(
        "\n[[io]]\ncall = \"Serial.PortDiagnostic\"\nconst = { integer = 7 }\n\n[[io]]\ncall = \"Serial.PortDiagnostic\"\nscript = \"Serial Test.Init.m1scr\"\noffset = 124\nconst = { integer = 8 }\n",
    );
    let trace = engine
        .run_with_adapter(&scenario, &mut adapter)
        .expect("exact site override wins over wildcard and adapter");
    assert_eq!(
        trace.channels["Root.Serial Test.Diagnostic"],
        vec![Value::m1_integer(8)]
    );
    assert_eq!(adapter.calls, vec!["Serial.PortInit"]);
    assert!(trace.hardware.iter().any(|item| {
        item.canonical_call() == "Serial.PortDiagnostic"
            && item.source == HardwareValueSource::ScenarioExact
    }));
}

#[test]
fn coverage_and_runtime_agree_on_rs232_and_lin_support() {
    let engine = engine();
    let coverage = engine.coverage();
    for method in [
        "GetHandle",
        "GetUnsignedInteger",
        "PortDiagnostic",
        "PortInit",
        "Receive",
        "SetFloat",
        "SetString",
        "SetUnsignedInteger",
        "Transmit",
    ] {
        let name = format!("Serial.{method}");
        assert!(
            coverage.adapter_backed.iter().any(|item| item.name == name),
            "{name} should be adapter-backed: {coverage:?}"
        );
    }
    assert!(coverage.unsupported.iter().all(|item| {
        !matches!(
            item.name.as_str(),
            "Serial.GetHandle"
                | "Serial.GetUnsignedInteger"
                | "Serial.PortDiagnostic"
                | "Serial.PortInit"
                | "Serial.Receive"
                | "Serial.SetFloat"
                | "Serial.SetString"
                | "Serial.SetUnsignedInteger"
                | "Serial.Transmit"
        )
    }));
    engine
        .run(&whole_project_scenario())
        .expect("every adapter-backed serial method in the fixture executes");
}

#[test]
fn directly_constructed_serial_scenarios_are_validated_before_execution() {
    let engine = engine();
    let mut scenario = whole_project_scenario();
    scenario.serial.rx[0].time_s = f64::NAN;
    let error = engine
        .run(&scenario)
        .expect_err("non-finite direct scenario time fails");
    assert!(error.to_string().contains("invalid time_s NaN"));

    let mut scenario = whole_project_scenario();
    scenario.serial.rx[0].bytes = vec![0; 257];
    let error = engine
        .run(&scenario)
        .expect_err("oversized direct scenario chunk fails");
    assert!(error.to_string().contains("257 bytes"));
}
