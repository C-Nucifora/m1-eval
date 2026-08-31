// SPDX-License-Identifier: GPL-3.0-or-later
//! Loader-backed synthetic virtual-CAN acceptance tests.

use std::path::{Path, PathBuf};

use m1_eval::{
    AdapterReply, CallSite, CanTransferDirection, Engine, EvalError, EvalTime, HardwareAdapter,
    HardwareCall, HardwareValueSource, ResolvedReceiver, Scenario, Value,
};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/virtual_can")
}

fn engine() -> Engine {
    Engine::load(&fixture_dir().join("Project.m1prj"), None)
        .expect("synthetic virtual-CAN fixture loads")
}

fn scenario(extra: &str) -> Scenario {
    Scenario::from_toml_str(&format!(
        r#"
mode = "whole-project"
duration_s = 0.02
base_rate_hz = 100.0

[[inputs]]
channel = "Root.CAN Demo.Tx Value"
const = 5.0

[[can.rx]]
time_s = 0.0
bus = 0
id = 0x123
bytes = [2]

{extra}
"#
    ))
    .expect("virtual-CAN scenario parses")
}

#[test]
fn dbc_rx_decode_and_tx_capture_share_the_loaded_layout() {
    let trace = engine()
        .run(&scenario(""))
        .expect("virtual-CAN run succeeds");
    assert_eq!(
        trace.channels["Root.CAN Demo.Rx Value"],
        vec![Value::m1_float(5.0), Value::m1_float(5.0)]
    );

    let rx = trace
        .can
        .iter()
        .find(|event| event.direction == CanTransferDirection::Rx)
        .expect("scenario frame is consumed");
    assert_eq!(rx.frame_id, 0x123);
    assert_eq!(rx.bytes, [2]);
    assert_eq!(rx.handle, None);
    assert_eq!(rx.message.as_deref(), Some("Vehicle Network.Status Frame"));

    let tx = trace
        .can
        .iter()
        .filter(|event| event.direction == CanTransferDirection::Tx)
        .collect::<Vec<_>>();
    assert_eq!(tx.len(), 2);
    assert!(tx.iter().all(|event| event.frame_id == 0x321));
    assert!(tx.iter().all(|event| event.bytes == [2]));
    assert!(tx.iter().all(|event| event.handle.is_some()));
    assert!(
        tx.iter()
            .all(|event| { event.message.as_deref() == Some("Vehicle Network.Command Frame") })
    );
    assert!(trace.hardware.iter().any(|item| {
        item.source == HardwareValueSource::VirtualCanRx
            && item.receiver
                == ResolvedReceiver::Project {
                    path: "DBC.Vehicle Network.Status Frame".to_string(),
                }
            && item.canonical_call() == "DBC.Vehicle Network.Status Frame.Receive"
    }));
    assert!(trace.hardware.iter().any(|item| {
        item.source == HardwareValueSource::VirtualCan
            && item.canonical_call() == "DBC.Vehicle Network.Command Frame.Tx"
    }));
}

#[derive(Default)]
struct SetOwner {
    calls: Vec<HardwareCall>,
}

impl HardwareAdapter for SetOwner {
    fn call(&mut self, call: &HardwareCall) -> Result<AdapterReply, EvalError> {
        self.calls.push(call.clone());
        Ok(if call.method == "SetScaled" {
            AdapterReply::Value(Value::m1_integer(99))
        } else if call.method == "GetScaled" {
            panic!("wildcard scenario must precede the external adapter")
        } else {
            AdapterReply::Unhandled
        })
    }
}

#[test]
fn scenario_then_external_adapter_then_internal_can_have_exact_precedence() {
    let mut adapter = SetOwner::default();
    let trace = engine()
        .run_with_adapter(
            &scenario(
                r#"
[[io]]
call = "DBC.Vehicle Network.Status Frame.Count.GetScaled"
const = 9.0
"#,
            ),
            &mut adapter,
        )
        .expect("mixed-route virtual-CAN run succeeds");

    assert_eq!(
        trace.channels["Root.CAN Demo.Rx Value"],
        vec![Value::m1_float(9.0), Value::m1_float(9.0)],
        "scenario owns GetScaled before the adapter"
    );
    let set_call = adapter
        .calls
        .iter()
        .find(|call| call.method == "SetScaled")
        .expect("external adapter owns SetScaled before the virtual model");
    assert_eq!(
        set_call.receiver,
        ResolvedReceiver::Project {
            path: "DBC.Vehicle Network.Command Frame.Command".to_string(),
        }
    );
    assert_eq!(
        set_call.source_receiver,
        "DBC.Vehicle Network.Command Frame.Command"
    );
    assert_eq!(set_call.site, CallSite::new("CAN Transmit.m1scr", 221));
    assert_eq!(set_call.time, EvalTime::periodic(0, 0.0, 0.01, 0.01));
    let tx = trace
        .can
        .iter()
        .filter(|event| event.direction == CanTransferDirection::Tx)
        .collect::<Vec<_>>();
    assert!(
        tx.iter().all(|event| event.bytes == [0]),
        "a handled SetScaled call does not mutate the lower-priority virtual buffer"
    );
    assert!(trace.hardware.iter().any(|item| {
        item.source == HardwareValueSource::ScenarioWildcard
            && item.source_call == "DBC.Vehicle Network.Status Frame.Count.GetScaled"
    }));
    assert!(trace.hardware.iter().any(|item| {
        item.source == HardwareValueSource::Adapter
            && item.source_call == "DBC.Vehicle Network.Command Frame.Command.SetScaled"
    }));
    assert!(trace.hardware.iter().any(|item| {
        item.source == HardwareValueSource::VirtualCanRx && item.method == "Receive"
    }));
    assert!(
        trace
            .hardware
            .iter()
            .any(|item| { item.source == HardwareValueSource::VirtualCan && item.method == "Tx" })
    );
}
