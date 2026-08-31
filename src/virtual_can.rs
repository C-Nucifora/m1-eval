// SPDX-License-Identifier: GPL-3.0-or-later
//! Deterministic classic-CAN model for `CanComms` and M1 DBC objects.
//!
//! The adapter is constructed from the loader's exact m1-can runtime model and
//! one scenario. It owns all mutable state for one evaluation run: bus
//! initialization, stable nonzero handles, independent receive cursors, frame
//! buffers, and ordered transfers. No host clock or filesystem access occurs.

use crate::env::CallSite;
use crate::error::EvalError;
use crate::expr::coerce_for_scalar_kind;
use crate::hardware::{AdapterReply, HardwareAdapter, HardwareCall};
use crate::scenario::{CanRx, CanScenario};
use crate::trace::{CanEvent, CanTransferDirection};
use crate::value::{FixedPoint7dps, M1Scalar, M1ScalarKind, Value};
use m1_can::{
    CanDirection, CanEndian, CanFrameFormat, CanRuntimeMessage, CanRuntimeModel, CanRuntimeSignal,
};
use m1_typecheck::intrinsics;
use m1_typecheck::types::ValueType;
use std::collections::BTreeMap;

const MAX_FRAME_BYTES: usize = 8;

/// `CanComms` methods with deterministic virtual-CAN implementations.
pub(crate) const LIBRARY_METHODS: &[&str] = &[
    "GetBit",
    "GetFixed7DP",
    "GetFloat",
    "GetID",
    "GetInteger",
    "GetLength",
    "GetTicks",
    "GetUnsignedInteger",
    "Init",
    "RxMessage",
    "RxOpenExtended",
    "RxOpenStandard",
    "SetBit",
    "SetFixed7DP",
    "SetFloat",
    "SetInteger",
    "SetUnsignedInteger",
    "TxExtended",
    "TxInitialise",
    "TxOpen",
    "TxStandard",
    "XOR8",
];

/// DBC module methods backed by the runtime layout model.
pub(crate) const MODULE_METHODS: &[&str] = &["Init"];

/// DBC message methods backed by the runtime layout model.
pub(crate) const MESSAGE_METHODS: &[&str] = &["Receive", "Tx", "TxInitialise", "TxOpen"];

/// DBC signal methods backed by the runtime layout model.
pub(crate) const SIGNAL_GET_METHODS: &[&str] = &[
    "GetBit",
    "GetFloat",
    "GetInteger",
    "GetScaled",
    "GetUnsignedInteger",
];

pub(crate) const SIGNAL_SET_METHODS: &[&str] = &[
    "SetBit",
    "SetFloat",
    "SetInteger",
    "SetScaled",
    "SetUnsignedInteger",
];

pub(crate) fn is_library_method(method: &str) -> bool {
    LIBRARY_METHODS.contains(&method)
}

pub(crate) fn model_handles_project_call(
    model: &CanRuntimeModel,
    receiver: &str,
    method: &str,
) -> bool {
    model.modules.iter().any(|module| {
        (MODULE_METHODS.contains(&method) && module.aliases.iter().any(|alias| alias == receiver))
            || module.messages.iter().any(|message| {
                (message_method_is_supported(message.direction, method)
                    && message.aliases.iter().any(|alias| alias == receiver))
                    || (signal_method_is_supported(message.direction, method)
                        && message
                            .signals
                            .iter()
                            .any(|signal| signal.aliases.iter().any(|alias| alias == receiver)))
            })
    })
}

/// Resolve any accepted source alias to the project-qualified identity exposed
/// to adapters and provenance. m1-can lists the exact source path first and the
/// registered project aliases after it; normal loaded projects register the
/// `DBC.` spelling. A project which registers only the bare source path keeps
/// that identity.
pub(crate) fn model_project_receiver<'a>(
    model: &'a CanRuntimeModel,
    receiver: &str,
) -> Option<&'a str> {
    fn project_alias<'a>(path: &'a str, aliases: &'a [String]) -> &'a str {
        aliases
            .iter()
            .find(|alias| alias.starts_with("DBC."))
            .or_else(|| aliases.iter().find(|alias| alias.as_str() != path))
            .map_or(path, String::as_str)
    }

    for module in &model.modules {
        if module.aliases.iter().any(|alias| alias == receiver) {
            return Some(project_alias(&module.path, &module.aliases));
        }
        for message in &module.messages {
            if message.aliases.iter().any(|alias| alias == receiver) {
                return Some(project_alias(&message.path, &message.aliases));
            }
            for signal in &message.signals {
                if signal.aliases.iter().any(|alias| alias == receiver) {
                    return Some(project_alias(&signal.path, &signal.aliases));
                }
            }
        }
    }
    None
}

fn message_method_is_supported(direction: Option<CanDirection>, method: &str) -> bool {
    if !MESSAGE_METHODS.contains(&method) {
        return false;
    }
    match method {
        "Receive" => direction != Some(CanDirection::Tx),
        "Tx" | "TxInitialise" | "TxOpen" => direction != Some(CanDirection::Rx),
        _ => false,
    }
}

fn signal_method_is_supported(direction: Option<CanDirection>, method: &str) -> bool {
    if SIGNAL_GET_METHODS.contains(&method) {
        return direction != Some(CanDirection::Tx);
    }
    if SIGNAL_SET_METHODS.contains(&method) {
        return direction != Some(CanDirection::Rx);
    }
    false
}

/// Result of one built-in virtual-CAN route.
#[derive(Debug)]
pub(crate) struct CanReply {
    pub(crate) value: Value,
    pub(crate) event: Option<CanEvent>,
    pub(crate) external: bool,
}

impl CanReply {
    fn value(value: Value) -> Self {
        CanReply {
            value,
            event: None,
            external: false,
        }
    }

    fn external(value: Value) -> Self {
        CanReply {
            value,
            event: None,
            external: true,
        }
    }

    fn sourced(value: Value, external: bool) -> Self {
        if external {
            Self::external(value)
        } else {
            Self::value(value)
        }
    }

    fn event(value: Value, event: CanEvent, external: bool) -> Self {
        CanReply {
            value,
            event: Some(event),
            external,
        }
    }
}

#[derive(Debug, Clone)]
struct Injection {
    time_s: f64,
    bus: i32,
    frame_id: u32,
    format: CanFrameFormat,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BusConfig {
    kbaud: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RxConfig {
    bus: i32,
    match_id: u32,
    mask: u32,
    format: CanFrameFormat,
    big_endian: bool,
}

#[derive(Debug, Clone)]
struct ReceivedFrame {
    frame_id: u32,
    bytes: Vec<u8>,
    base_tick: u64,
}

#[derive(Debug, Clone)]
struct ReceiverState {
    config: RxConfig,
    cursor: usize,
    current: Option<ReceivedFrame>,
}

#[derive(Debug, Clone)]
enum HandleState {
    Transmit {
        big_endian: bool,
        bytes: Option<Vec<u8>>,
    },
    Receive(ReceiverState),
    DbcMessage {
        message: String,
        bytes: Option<Vec<u8>>,
    },
}

#[derive(Debug, Clone)]
struct ModuleDef {
    bus: Option<i32>,
}

#[derive(Debug, Clone)]
struct MessageDef {
    module: String,
    frame_id: u32,
    format: CanFrameFormat,
    dlc: usize,
    direction: Option<CanDirection>,
}

#[derive(Debug, Clone)]
struct SignalDef {
    message: String,
    raw_kind: ValueType,
    signed: bool,
    float: bool,
    endian: CanEndian,
    start_bit: u16,
    width: u16,
    scale: f64,
    offset: f64,
}

#[derive(Clone, Copy)]
enum ProjectArgKind {
    Handle,
    Integer,
    FloatingPoint,
    Boolean,
}

/// Fresh per-run deterministic CAN state.
pub(crate) struct VirtualCan {
    model: CanRuntimeModel,
    injections: Vec<Injection>,
    buses: BTreeMap<i32, BusConfig>,
    modules: BTreeMap<String, ModuleDef>,
    module_aliases: BTreeMap<String, String>,
    initialized_modules: BTreeMap<String, i32>,
    messages: BTreeMap<String, MessageDef>,
    message_aliases: BTreeMap<String, String>,
    signals: BTreeMap<String, SignalDef>,
    signal_aliases: BTreeMap<String, String>,
    handles: BTreeMap<u32, HandleState>,
    site_handles: BTreeMap<(CallSite, String), u32>,
    dbc_rx: BTreeMap<String, ReceiverState>,
    next_handle: u32,
}

impl VirtualCan {
    pub(crate) fn new(model: &CanRuntimeModel, scenario: &CanScenario) -> Result<Self, EvalError> {
        let injections = validated_injections(scenario)?;
        let mut modules = BTreeMap::new();
        let mut module_aliases = BTreeMap::new();
        let mut messages = BTreeMap::new();
        let mut message_aliases = BTreeMap::new();
        let mut signals = BTreeMap::new();
        let mut signal_aliases = BTreeMap::new();

        for module in &model.modules {
            let bus = module
                .bus_value
                .map(|value| {
                    let bus = i32::try_from(value).map_err(|_| {
                        can_model_error(format!(
                            "module `{}` resolves bus {value} outside the M1 Integer range",
                            module.path
                        ))
                    })?;
                    validate_bus_number("module binding", bus)?;
                    Ok(bus)
                })
                .transpose()?;
            insert_aliases(&mut module_aliases, &module.aliases, &module.path, "module")?;
            modules.insert(module.path.clone(), ModuleDef { bus });

            for message in &module.messages {
                validate_message_layout(message)?;
                insert_aliases(
                    &mut message_aliases,
                    &message.aliases,
                    &message.path,
                    "message",
                )?;
                let dlc = usize::from(message.dlc);
                messages.insert(
                    message.path.clone(),
                    MessageDef {
                        module: module.path.clone(),
                        frame_id: message.frame_id,
                        format: message.format,
                        dlc,
                        direction: message.direction,
                    },
                );

                for signal in &message.signals {
                    validate_signal_layout(message, signal)?;
                    insert_aliases(&mut signal_aliases, &signal.aliases, &signal.path, "signal")?;
                    signals.insert(
                        signal.path.clone(),
                        SignalDef {
                            message: message.path.clone(),
                            raw_kind: signal.raw_kind,
                            signed: signal.signed,
                            float: signal.float,
                            endian: signal.endian,
                            start_bit: signal.start_bit,
                            width: signal.width,
                            scale: signal.scale,
                            offset: signal.offset,
                        },
                    );
                }
            }
        }

        Ok(VirtualCan {
            model: model.clone(),
            injections,
            buses: BTreeMap::new(),
            modules,
            module_aliases,
            initialized_modules: BTreeMap::new(),
            messages,
            message_aliases,
            signals,
            signal_aliases,
            handles: BTreeMap::new(),
            site_handles: BTreeMap::new(),
            dbc_rx: BTreeMap::new(),
            next_handle: 1,
        })
    }

    pub(crate) fn empty() -> Self {
        VirtualCan::new(
            &CanRuntimeModel {
                modules: Vec::new(),
                skipped_scripts: Vec::new(),
            },
            &CanScenario::default(),
        )
        .expect("the empty virtual CAN model is valid")
    }

    pub(crate) fn model(&self) -> &CanRuntimeModel {
        &self.model
    }

    /// Route one call, retaining event/source detail used by evaluator traces.
    pub(crate) fn call_routed(
        &mut self,
        call: &HardwareCall,
    ) -> Result<Option<CanReply>, EvalError> {
        if call.receiver.name() == "CanComms" {
            if !is_library_method(&call.method) {
                return self.call_library(call);
            }
            let normalized = self.normalize_library_call(call)?;
            return self.call_library(&normalized);
        }
        let normalized = self.normalize_project_call(call)?;
        self.call_project(&normalized)
    }

    fn normalize_library_call(&self, call: &HardwareCall) -> Result<HardwareCall, EvalError> {
        let overloads = intrinsics::get().library_overloads("CanComms", &call.method);
        let Some(overload) = overloads
            .iter()
            .find(|overload| overload.params.len() == call.arguments.len())
        else {
            let mut accepted: Vec<_> = overloads
                .iter()
                .map(|overload| overload.params.len())
                .collect();
            accepted.sort_unstable();
            accepted.dedup();
            return Err(can_call_error(
                call,
                format!(
                    "expected {} argument(s), got {}",
                    accepted
                        .iter()
                        .map(usize::to_string)
                        .collect::<Vec<_>>()
                        .join(" or "),
                    call.arguments.len()
                ),
            ));
        };
        let mut normalized = call.clone();
        normalized.arguments = call
            .arguments
            .iter()
            .zip(&overload.params)
            .enumerate()
            .map(|(index, (value, parameter))| {
                let target = format!(
                    "{} argument {} `{}`",
                    call.canonical_name(),
                    index + 1,
                    parameter.name
                );
                match parameter.ty.as_str() {
                    "Handle" | "UnsignedInteger" => coerce_for_scalar_kind(
                        &target,
                        value.clone(),
                        M1ScalarKind::UnsignedInteger,
                    ),
                    "Integer" => {
                        coerce_for_scalar_kind(&target, value.clone(), M1ScalarKind::Integer)
                    }
                    "FloatingPoint" => normalize_can_float(call, index, value),
                    "FixedPoint7dps" => {
                        coerce_for_scalar_kind(&target, value.clone(), M1ScalarKind::FixedPoint7dps)
                    }
                    "Boolean" => match value {
                        Value::Bool(value) => Ok(Value::Bool(*value)),
                        other => Err(can_call_error(
                            call,
                            format!("argument {} must be Boolean, got {other:?}", index + 1),
                        )),
                    },
                    unsupported => Err(can_call_error(
                        call,
                        format!(
                            "parameter `{}` uses unsupported catalogue type `{unsupported}`",
                            parameter.name
                        ),
                    )),
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(normalized)
    }

    /// Normalize DBC arguments to the captured M1 call families after an
    /// external adapter has declined the call but before virtual state changes.
    fn normalize_project_call(&self, call: &HardwareCall) -> Result<HardwareCall, EvalError> {
        let signature = if resolve_alias(&self.module_aliases, call)?.is_some() {
            match call.method.as_str() {
                "Init" => Some(&[ProjectArgKind::Integer][..]),
                _ => None,
            }
        } else if resolve_alias(&self.message_aliases, call)?.is_some() {
            match call.method.as_str() {
                "Receive" | "TxOpen" => Some(&[][..]),
                "Tx" | "TxInitialise" => Some(&[ProjectArgKind::Handle][..]),
                _ => None,
            }
        } else if let Some(signal) = resolve_alias(&self.signal_aliases, call)? {
            let definition = &self.signals[&signal];
            let direction = self.messages[&definition.message].direction;
            if !signal_method_is_supported(direction, &call.method) {
                return Ok(call.clone());
            }
            match call.method.as_str() {
                "GetBit" | "GetInteger" | "GetUnsignedInteger" | "GetFloat" | "GetScaled" => {
                    Some(&[][..])
                }
                "SetBit" => Some(&[ProjectArgKind::Handle, ProjectArgKind::Boolean][..]),
                "SetInteger" | "SetUnsignedInteger" => {
                    Some(&[ProjectArgKind::Handle, ProjectArgKind::Integer][..])
                }
                "SetFloat" | "SetScaled" => {
                    Some(&[ProjectArgKind::Handle, ProjectArgKind::FloatingPoint][..])
                }
                _ => None,
            }
        } else {
            None
        };
        let Some(signature) = signature else {
            return Ok(call.clone());
        };
        require_argument_count(call, signature.len())?;

        let mut normalized = call.clone();
        normalized.arguments = call
            .arguments
            .iter()
            .zip(signature)
            .enumerate()
            .map(|(index, (value, kind))| {
                let target = format!("{} argument {}", call.canonical_name(), index + 1);
                match kind {
                    ProjectArgKind::Handle => coerce_for_scalar_kind(
                        &target,
                        value.clone(),
                        M1ScalarKind::UnsignedInteger,
                    ),
                    ProjectArgKind::Integer => {
                        coerce_for_scalar_kind(&target, value.clone(), M1ScalarKind::Integer)
                    }
                    ProjectArgKind::FloatingPoint => normalize_can_float(call, index, value),
                    ProjectArgKind::Boolean => match value {
                        Value::Bool(value) => Ok(Value::Bool(*value)),
                        other => Err(can_call_error(
                            call,
                            format!("argument {} must be Boolean, got {other:?}", index + 1),
                        )),
                    },
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(normalized)
    }

    fn call_library(&mut self, call: &HardwareCall) -> Result<Option<CanReply>, EvalError> {
        let reply = match call.method.as_str() {
            "Init" => {
                let bus = bus_arg(call, 0)?;
                let kbaud = positive_integer_arg(call, 1, "kbaud")?;
                self.initialize_bus(&call.method, bus, Some(kbaud))?;
                CanReply::value(Value::Bool(true))
            }
            "RxOpenStandard" | "RxOpenExtended" => {
                let format = if call.method == "RxOpenStandard" {
                    CanFrameFormat::Standard
                } else {
                    CanFrameFormat::Extended
                };
                let config = RxConfig {
                    bus: bus_arg(call, 0)?,
                    match_id: id_arg(call, 1, format, "match")?,
                    mask: id_arg(call, 2, format, "mask")?,
                    format,
                    big_endian: bool_arg(call, 3)?,
                };
                let handle = self.open_receive(call, config)?;
                CanReply::value(Value::m1_unsigned(handle))
            }
            "RxMessage" => self.raw_receive(call)?,
            "TxOpen" => {
                let big_endian = bool_arg(call, 0)?;
                let handle = self.open_transmit(call, big_endian)?;
                CanReply::value(Value::m1_unsigned(handle))
            }
            "TxInitialise" => {
                let handle = handle_arg(call, 0)?;
                let length = frame_length_arg(call, 1)?;
                let HandleState::Transmit { bytes, .. } = self.handle_mut(call, handle)? else {
                    return Err(can_call_error(
                        call,
                        format!("handle {handle} is not a raw transmit handle"),
                    ));
                };
                *bytes = Some(vec![0; length]);
                CanReply::value(Value::Bool(true))
            }
            "TxStandard" | "TxExtended" => self.raw_transmit(call)?,
            "GetBit" => {
                let handle = handle_arg(call, 0)?;
                let bit = bit_offset_arg(call, 1)?;
                let external = self.raw_read_is_external(call, handle)?;
                let bytes = self.handle_bytes(call, handle)?;
                CanReply::sourced(
                    Value::Bool(read_raw_bit(
                        bytes,
                        bit,
                        self.handle_endian(call, handle)?,
                        call,
                    )?),
                    external,
                )
            }
            "SetBit" => {
                let handle = handle_arg(call, 0)?;
                let bit = bit_offset_arg(call, 1)?;
                let value = bool_arg(call, 2)?;
                let (bytes, big_endian) = self.transmit_bytes_and_endian_mut(call, handle)?;
                write_raw_field(bytes, bit, 1, big_endian, u64::from(value), call)?;
                CanReply::value(Value::Bool(true))
            }
            "GetInteger" | "GetUnsignedInteger" => {
                let handle = handle_arg(call, 0)?;
                let bit_offset = bit_offset_arg(call, 1)?;
                let width = integer_bit_width(call, 2)?;
                let external = self.raw_read_is_external(call, handle)?;
                let (bytes, big_endian) = self.handle_bytes_and_endian(call, handle)?;
                let raw = read_raw_field(bytes, bit_offset, width, big_endian, call)?;
                if call.method == "GetInteger" {
                    CanReply::sourced(Value::m1_integer(sign_extend(raw, width) as i32), external)
                } else {
                    // The catalogue declares Integer for this unsigned reader.
                    CanReply::sourced(Value::m1_integer(raw as u32 as i32), external)
                }
            }
            "SetInteger" | "SetUnsignedInteger" => {
                let handle = handle_arg(call, 0)?;
                let bit_offset = bit_offset_arg(call, 1)?;
                let width = integer_bit_width(call, 2)?;
                let raw = if call.method == "SetInteger" {
                    encode_signed(integer_arg(call, 3)?, width, call)?
                } else {
                    encode_unsigned(integer_storage_u32_arg(call, 3)?, width, call)?
                };
                let (bytes, big_endian) = self.transmit_bytes_and_endian_mut(call, handle)?;
                write_raw_field(bytes, bit_offset, width, big_endian, raw, call)?;
                CanReply::value(Value::Bool(true))
            }
            "GetFloat" => {
                let handle = handle_arg(call, 0)?;
                let bit_offset = bit_offset_arg(call, 1)?;
                let external = self.raw_read_is_external(call, handle)?;
                let (bytes, big_endian) = self.handle_bytes_and_endian(call, handle)?;
                let raw = read_raw_field(bytes, bit_offset, 32, big_endian, call)?;
                CanReply::sourced(Value::m1_float(f32::from_bits(raw as u32)), external)
            }
            "SetFloat" => {
                let handle = handle_arg(call, 0)?;
                let bit_offset = bit_offset_arg(call, 1)?;
                let value = float_bits_arg(call, 2)?;
                let (bytes, big_endian) = self.transmit_bytes_and_endian_mut(call, handle)?;
                write_raw_field(
                    bytes,
                    bit_offset,
                    32,
                    big_endian,
                    u64::from(value.to_bits()),
                    call,
                )?;
                CanReply::value(Value::Bool(true))
            }
            "SetFixed7DP" => {
                let handle = handle_arg(call, 0)?;
                let bit_offset = bit_offset_arg(call, 1)?;
                let value = fixed_arg(call, 2)?;
                let (bytes, big_endian) = self.transmit_bytes_and_endian_mut(call, handle)?;
                write_raw_field(
                    bytes,
                    bit_offset,
                    32,
                    big_endian,
                    u64::from(value.raw() as u32),
                    call,
                )?;
                CanReply::value(Value::Bool(true))
            }
            "GetFixed7DP" => {
                let handle = handle_arg(call, 0)?;
                let bit_offset = bit_offset_arg(call, 1)?;
                let external = self.raw_read_is_external(call, handle)?;
                let (bytes, big_endian) = self.handle_bytes_and_endian(call, handle)?;
                let raw = read_raw_field(bytes, bit_offset, 32, big_endian, call)? as u32 as i32;
                CanReply::sourced(
                    Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(raw))),
                    external,
                )
            }
            "GetLength" => {
                let handle = handle_arg(call, 0)?;
                let external = self.raw_read_is_external(call, handle)?;
                let length = self.handle_bytes(call, handle)?.len();
                CanReply::sourced(Value::m1_integer(length as i32), external)
            }
            "GetID" | "GetTicks" => {
                let handle = handle_arg(call, 0)?;
                let current = self.received_frame(call, handle)?;
                let value = match call.method.as_str() {
                    "GetID" => Value::m1_integer(current.frame_id as i32),
                    "GetTicks" => Value::m1_unsigned(current.base_tick as u32),
                    _ => unreachable!(),
                };
                CanReply::external(value)
            }
            "XOR8" => {
                let handle = handle_arg(call, 0)?;
                let external = self.raw_read_is_external(call, handle)?;
                let bytes = self.handle_bytes(call, handle)?;
                let value = bytes.iter().fold(0_u8, |acc, byte| acc ^ byte);
                CanReply::sourced(Value::m1_integer(i32::from(value)), external)
            }
            _ => return Ok(None),
        };
        Ok(Some(reply))
    }

    fn call_project(&mut self, call: &HardwareCall) -> Result<Option<CanReply>, EvalError> {
        if let Some(module) = resolve_alias(&self.module_aliases, call)? {
            if call.method != "Init" {
                return Err(unsupported_can_object(call));
            }
            if call.arguments.len() != 1 {
                return Err(can_call_error(
                    call,
                    format!("expected 1 argument, got {}", call.arguments.len()),
                ));
            }
            let bus = bus_arg(call, 0)?;
            if let Some(expected) = self.modules[&module].bus
                && expected != bus
            {
                return Err(can_call_error(
                    call,
                    format!(
                        "module `{module}` is bound to bus {expected} in the loaded snapshot, but Init received bus {bus}"
                    ),
                ));
            }
            if let Some(previous) = self.initialized_modules.get(&module)
                && *previous != bus
            {
                return Err(can_call_error(
                    call,
                    format!(
                        "DBC module `{module}` was already initialized on bus {previous}, then Init received bus {bus}"
                    ),
                ));
            }
            self.initialize_bus(&call.method, bus, None)?;
            self.initialized_modules.insert(module, bus);
            return Ok(Some(CanReply::value(Value::Bool(true))));
        }

        if let Some(message) = resolve_alias(&self.message_aliases, call)? {
            let direction = self.messages[&message].direction;
            if !message_method_is_supported(direction, &call.method) {
                return Err(unsupported_can_object(call));
            }
            let reply = match call.method.as_str() {
                "Receive" => {
                    require_no_arguments(call)?;
                    self.dbc_receive(call, &message)?
                }
                "TxOpen" => {
                    require_no_arguments(call)?;
                    let handle = self.open_dbc_message(call, &message)?;
                    CanReply::value(Value::m1_unsigned(handle))
                }
                "TxInitialise" => {
                    require_argument_count(call, 1)?;
                    let handle = handle_arg(call, 0)?;
                    let dlc = self.messages[&message].dlc;
                    let bytes = self.dbc_message_bytes_mut(call, handle, &message)?;
                    *bytes = Some(vec![0; dlc]);
                    CanReply::value(Value::Bool(true))
                }
                "Tx" => {
                    require_argument_count(call, 1)?;
                    let handle = handle_arg(call, 0)?;
                    self.dbc_transmit(call, &message, handle)?
                }
                _ => return Err(unsupported_can_object(call)),
            };
            return Ok(Some(reply));
        }

        if let Some(signal) = resolve_alias(&self.signal_aliases, call)? {
            let definition = &self.signals[&signal];
            let direction = self.messages[&definition.message].direction;
            if !signal_method_is_supported(direction, &call.method) {
                return Err(unsupported_can_object(call));
            }
            let reply = match call.method.as_str() {
                "GetBit" | "GetInteger" | "GetUnsignedInteger" | "GetFloat" | "GetScaled" => {
                    require_no_arguments(call)?;
                    self.dbc_get(call, &signal)?
                }
                "SetBit" | "SetInteger" | "SetUnsignedInteger" | "SetFloat" | "SetScaled" => {
                    self.dbc_set(call, &signal)?
                }
                _ => return Err(unsupported_can_object(call)),
            };
            return Ok(Some(reply));
        }
        Ok(None)
    }

    fn initialize_bus(
        &mut self,
        method: &str,
        bus: i32,
        kbaud: Option<i32>,
    ) -> Result<(), EvalError> {
        match (self.buses.get_mut(&bus), kbaud) {
            (Some(existing), Some(requested)) if existing.kbaud == Some(requested) => {}
            (Some(existing), Some(requested)) if existing.kbaud.is_none() => {
                existing.kbaud = Some(requested);
            }
            (Some(existing), Some(requested)) => {
                return Err(can_method_error(
                    method,
                    format!(
                        "bus {bus} is already initialized at {} kbaud, then requested {requested} kbaud",
                        existing.kbaud.expect("guarded above")
                    ),
                ));
            }
            (Some(_), None) => {}
            (None, requested) => {
                self.buses.insert(bus, BusConfig { kbaud: requested });
            }
        }
        Ok(())
    }

    fn require_bus(&self, call: &HardwareCall, bus: i32) -> Result<(), EvalError> {
        if self.buses.contains_key(&bus) {
            Ok(())
        } else {
            Err(can_call_error(
                call,
                format!(
                    "bus {bus} is not initialized; call CanComms.Init or the DBC module Init first"
                ),
            ))
        }
    }

    fn open_receive(&mut self, call: &HardwareCall, config: RxConfig) -> Result<u32, EvalError> {
        let key = (call.site.clone(), call.method.clone());
        if let Some(handle) = self.site_handles.get(&key).copied() {
            let HandleState::Receive(existing) = &self.handles[&handle] else {
                unreachable!("receive site always points to a receive handle")
            };
            if existing.config != config {
                return Err(can_call_error(
                    call,
                    format!(
                        "call site already opened handle {handle} with different receive configuration"
                    ),
                ));
            }
            return Ok(handle);
        }
        let handle = self.allocate_handle(call)?;
        self.site_handles.insert(key, handle);
        self.handles.insert(
            handle,
            HandleState::Receive(ReceiverState {
                config,
                cursor: 0,
                current: None,
            }),
        );
        Ok(handle)
    }

    fn open_transmit(&mut self, call: &HardwareCall, big_endian: bool) -> Result<u32, EvalError> {
        let key = (call.site.clone(), call.method.clone());
        if let Some(handle) = self.site_handles.get(&key).copied() {
            let HandleState::Transmit {
                big_endian: existing,
                ..
            } = &self.handles[&handle]
            else {
                unreachable!("transmit site always points to a transmit handle")
            };
            if *existing != big_endian {
                return Err(can_call_error(
                    call,
                    format!(
                        "call site already opened handle {handle} with bigendian={existing}, then requested bigendian={big_endian}"
                    ),
                ));
            }
            return Ok(handle);
        }
        let handle = self.allocate_handle(call)?;
        self.site_handles.insert(key, handle);
        self.handles.insert(
            handle,
            HandleState::Transmit {
                big_endian,
                bytes: None,
            },
        );
        Ok(handle)
    }

    fn open_dbc_message(&mut self, call: &HardwareCall, message: &str) -> Result<u32, EvalError> {
        let key = (call.site.clone(), call.canonical_name());
        if let Some(handle) = self.site_handles.get(&key).copied() {
            let HandleState::DbcMessage {
                message: existing, ..
            } = &self.handles[&handle]
            else {
                unreachable!("DBC transmit site always points to a DBC handle")
            };
            if existing != message {
                return Err(can_call_error(
                    call,
                    format!(
                        "call site already opened handle {handle} for DBC message `{existing}`, then resolved to `{message}`"
                    ),
                ));
            }
            return Ok(handle);
        }
        let handle = self.allocate_handle(call)?;
        self.site_handles.insert(key, handle);
        self.handles.insert(
            handle,
            HandleState::DbcMessage {
                message: message.to_string(),
                bytes: None,
            },
        );
        Ok(handle)
    }

    fn allocate_handle(&mut self, call: &HardwareCall) -> Result<u32, EvalError> {
        self.allocate_handle_name(&call.canonical_name())
    }

    fn allocate_handle_name(&mut self, name: &str) -> Result<u32, EvalError> {
        let handle = self.next_handle;
        self.next_handle = self.next_handle.checked_add(1).ok_or_else(|| {
            can_method_error(name, "exhausted the nonzero 32-bit CAN handle space")
        })?;
        Ok(handle)
    }

    fn handle(&self, call: &HardwareCall, handle: u32) -> Result<&HandleState, EvalError> {
        self.handles.get(&handle).ok_or_else(|| {
            can_call_error(
                call,
                format!("invalid handle {handle}; handles are nonzero and owned by this run"),
            )
        })
    }

    fn handle_mut(
        &mut self,
        call: &HardwareCall,
        handle: u32,
    ) -> Result<&mut HandleState, EvalError> {
        self.handles.get_mut(&handle).ok_or_else(|| {
            can_call_error(
                call,
                format!("invalid handle {handle}; handles are nonzero and owned by this run"),
            )
        })
    }

    fn raw_receive(&mut self, call: &HardwareCall) -> Result<CanReply, EvalError> {
        let handle = handle_arg(call, 0)?;
        let config = match self.handle(call, handle)? {
            HandleState::Receive(state) => state.config.clone(),
            _ => {
                return Err(can_call_error(
                    call,
                    format!("handle {handle} is not a raw receive handle"),
                ));
            }
        };
        self.require_bus(call, config.bus)?;
        let cursor = match self.handle(call, handle)? {
            HandleState::Receive(state) => state.cursor,
            _ => unreachable!(),
        };
        let Some((cursor, injection)) =
            next_injection(&self.injections, cursor, call.time.elapsed_s, &config)
                .map(|(cursor, injection)| (cursor, injection.clone()))
        else {
            return Ok(CanReply::external(Value::Bool(false)));
        };
        let state = match self.handle_mut(call, handle)? {
            HandleState::Receive(state) => state,
            _ => unreachable!(),
        };
        state.cursor = cursor;
        state.current = Some(ReceivedFrame {
            frame_id: injection.frame_id,
            bytes: injection.bytes.clone(),
            base_tick: arrival_base_tick(injection.time_s, call.time.base_period_s, call)?,
        });
        Ok(CanReply::event(
            Value::Bool(true),
            CanEvent {
                direction: CanTransferDirection::Rx,
                time: call.time,
                bus: injection.bus,
                frame_id: injection.frame_id,
                format: injection.format,
                bytes: injection.bytes.clone(),
                handle: Some(handle),
                message: None,
                site: call.site.clone(),
            },
            true,
        ))
    }

    fn raw_transmit(&mut self, call: &HardwareCall) -> Result<CanReply, EvalError> {
        let handle = handle_arg(call, 0)?;
        let format = if call.method == "TxStandard" {
            CanFrameFormat::Standard
        } else {
            CanFrameFormat::Extended
        };
        let bus = bus_arg(call, 1)?;
        let frame_id = id_arg(call, 2, format, "ID")?;
        self.require_bus(call, bus)?;
        let bytes = match self.handle(call, handle)? {
            HandleState::Transmit {
                bytes: Some(bytes), ..
            } => bytes.clone(),
            HandleState::Transmit { bytes: None, .. } => {
                return Err(can_call_error(
                    call,
                    format!("handle {handle} has no payload; call CanComms.TxInitialise first"),
                ));
            }
            _ => {
                return Err(can_call_error(
                    call,
                    format!("handle {handle} is not a raw transmit handle"),
                ));
            }
        };
        Ok(CanReply::event(
            Value::Bool(true),
            CanEvent {
                direction: CanTransferDirection::Tx,
                time: call.time,
                bus,
                frame_id,
                format,
                bytes,
                handle: Some(handle),
                message: None,
                site: call.site.clone(),
            },
            false,
        ))
    }

    fn handle_bytes<'a>(&'a self, call: &HardwareCall, handle: u32) -> Result<&'a [u8], EvalError> {
        match self.handle(call, handle)? {
            HandleState::Transmit {
                bytes: Some(bytes),
                ..
            } => Ok(bytes),
            HandleState::Transmit { bytes: None, .. } => Err(can_call_error(
                call,
                format!("handle {handle} has no payload; call CanComms.TxInitialise first"),
            )),
            HandleState::Receive(state) => state
                .current
                .as_ref()
                .map(|frame| frame.bytes.as_slice())
                .ok_or_else(|| {
                    can_call_error(
                        call,
                        format!("receive handle {handle} has no current frame; call CanComms.RxMessage first"),
                    )
                }),
            HandleState::DbcMessage { message, .. } => Err(can_call_error(
                call,
                format!("handle {handle} belongs to DBC message `{message}`, not CanComms"),
            )),
        }
    }

    fn handle_bytes_and_endian<'a>(
        &'a self,
        call: &HardwareCall,
        handle: u32,
    ) -> Result<(&'a [u8], bool), EvalError> {
        let endian = match self.handle(call, handle)? {
            HandleState::Transmit { big_endian, .. } => *big_endian,
            HandleState::Receive(state) => state.config.big_endian,
            HandleState::DbcMessage { message, .. } => {
                return Err(can_call_error(
                    call,
                    format!("handle {handle} belongs to DBC message `{message}`, not CanComms"),
                ));
            }
        };
        Ok((self.handle_bytes(call, handle)?, endian))
    }

    fn handle_endian(&self, call: &HardwareCall, handle: u32) -> Result<bool, EvalError> {
        match self.handle(call, handle)? {
            HandleState::Transmit { big_endian, .. } => Ok(*big_endian),
            HandleState::Receive(state) => Ok(state.config.big_endian),
            HandleState::DbcMessage { message, .. } => Err(can_call_error(
                call,
                format!("handle {handle} belongs to DBC message `{message}`, not CanComms"),
            )),
        }
    }

    fn raw_read_is_external(&self, call: &HardwareCall, handle: u32) -> Result<bool, EvalError> {
        match self.handle(call, handle)? {
            HandleState::Transmit { .. } => Ok(false),
            HandleState::Receive(state) if state.current.is_some() => Ok(true),
            HandleState::Receive(_) => Err(can_call_error(
                call,
                format!(
                    "receive handle {handle} has no current frame; call CanComms.RxMessage first"
                ),
            )),
            HandleState::DbcMessage { message, .. } => Err(can_call_error(
                call,
                format!("handle {handle} belongs to DBC message `{message}`, not CanComms"),
            )),
        }
    }

    fn transmit_bytes_and_endian_mut<'a>(
        &'a mut self,
        call: &HardwareCall,
        handle: u32,
    ) -> Result<(&'a mut Vec<u8>, bool), EvalError> {
        match self.handle_mut(call, handle)? {
            HandleState::Transmit {
                big_endian,
                bytes: Some(bytes),
            } => Ok((bytes, *big_endian)),
            HandleState::Transmit { bytes: None, .. } => Err(can_call_error(
                call,
                format!("handle {handle} has no payload; call CanComms.TxInitialise first"),
            )),
            _ => Err(can_call_error(
                call,
                format!("handle {handle} is not a raw transmit handle"),
            )),
        }
    }

    fn received_frame(
        &self,
        call: &HardwareCall,
        handle: u32,
    ) -> Result<&ReceivedFrame, EvalError> {
        match self.handle(call, handle)? {
            HandleState::Receive(state) => state.current.as_ref().ok_or_else(|| {
                can_call_error(
                    call,
                    format!("receive handle {handle} has no current frame; call CanComms.RxMessage first"),
                )
            }),
            _ => Err(can_call_error(
                call,
                format!("handle {handle} is not a raw receive handle"),
            )),
        }
    }

    fn dbc_message_bytes_mut<'a>(
        &'a mut self,
        call: &HardwareCall,
        handle: u32,
        expected_message: &str,
    ) -> Result<&'a mut Option<Vec<u8>>, EvalError> {
        match self.handle_mut(call, handle)? {
            HandleState::DbcMessage { message, bytes } if message == expected_message => Ok(bytes),
            HandleState::DbcMessage { message, .. } => Err(can_call_error(
                call,
                format!(
                    "handle {handle} belongs to DBC message `{message}`, not `{expected_message}`"
                ),
            )),
            _ => Err(can_call_error(
                call,
                format!("handle {handle} is not a DBC transmit handle"),
            )),
        }
    }

    fn dbc_message_bytes<'a>(
        &'a self,
        call: &HardwareCall,
        handle: u32,
        expected_message: &str,
    ) -> Result<&'a [u8], EvalError> {
        match self.handle(call, handle)? {
            HandleState::DbcMessage {
                message,
                bytes: Some(bytes),
            } if message == expected_message => Ok(bytes),
            HandleState::DbcMessage {
                message,
                bytes: None,
            } if message == expected_message => Err(can_call_error(
                call,
                format!(
                    "DBC handle {handle} has no payload; call `{expected_message}.TxInitialise(handle)` first"
                ),
            )),
            HandleState::DbcMessage { message, .. } => Err(can_call_error(
                call,
                format!(
                    "handle {handle} belongs to DBC message `{message}`, not `{expected_message}`"
                ),
            )),
            _ => Err(can_call_error(
                call,
                format!("handle {handle} is not a DBC transmit handle"),
            )),
        }
    }

    fn module_bus_for_message(&self, call: &HardwareCall, message: &str) -> Result<i32, EvalError> {
        let module = &self.messages[message].module;
        self.initialized_modules.get(module).copied().ok_or_else(|| {
            can_call_error(
                call,
                format!("DBC module `{module}` is not initialized in this run; execute its Init call first"),
            )
        })
    }

    fn dbc_transmit(
        &self,
        call: &HardwareCall,
        message: &str,
        handle: u32,
    ) -> Result<CanReply, EvalError> {
        let bus = self.module_bus_for_message(call, message)?;
        self.require_bus(call, bus)?;
        let definition = &self.messages[message];
        let bytes = self.dbc_message_bytes(call, handle, message)?.to_vec();
        Ok(CanReply::event(
            Value::Bool(true),
            CanEvent {
                direction: CanTransferDirection::Tx,
                time: call.time,
                bus,
                frame_id: definition.frame_id,
                format: definition.format,
                bytes,
                handle: Some(handle),
                message: Some(message.to_string()),
                site: call.site.clone(),
            },
            false,
        ))
    }

    fn dbc_receive(&mut self, call: &HardwareCall, message: &str) -> Result<CanReply, EvalError> {
        let message = message.to_string();
        let bus = self.module_bus_for_message(call, &message)?;
        self.require_bus(call, bus)?;
        let message_def = &self.messages[&message];
        let config = RxConfig {
            bus,
            match_id: message_def.frame_id,
            mask: 0,
            format: message_def.format,
            big_endian: false,
        };
        let state = self
            .dbc_rx
            .entry(message.clone())
            .or_insert_with(|| ReceiverState {
                config: config.clone(),
                cursor: 0,
                current: None,
            });
        if state.config != config {
            return Err(can_call_error(
                call,
                format!("DBC message `{message}` changed bus or frame identity during one run"),
            ));
        }
        let Some((cursor, injection)) =
            next_injection(&self.injections, state.cursor, call.time.elapsed_s, &config)
        else {
            return Ok(CanReply::external(Value::Bool(false)));
        };
        if injection.bytes.len() != message_def.dlc {
            return Err(can_call_error(
                call,
                format!(
                    "scenario frame 0x{:X} on bus {bus} has {} bytes, but DBC message `{message}` declares DLC {}",
                    injection.frame_id,
                    injection.bytes.len(),
                    message_def.dlc
                ),
            ));
        }
        state.cursor = cursor;
        state.current = Some(ReceivedFrame {
            frame_id: injection.frame_id,
            bytes: injection.bytes.clone(),
            base_tick: arrival_base_tick(injection.time_s, call.time.base_period_s, call)?,
        });
        Ok(CanReply::event(
            Value::Bool(true),
            CanEvent {
                direction: CanTransferDirection::Rx,
                time: call.time,
                bus,
                frame_id: injection.frame_id,
                format: injection.format,
                bytes: injection.bytes.clone(),
                handle: None,
                message: Some(message),
                site: call.site.clone(),
            },
            true,
        ))
    }

    fn dbc_get(&self, call: &HardwareCall, signal: &str) -> Result<CanReply, EvalError> {
        let definition = &self.signals[signal];
        let message = &definition.message;
        let frame = self
            .dbc_rx
            .get(message)
            .and_then(|state| state.current.as_ref())
            .ok_or_else(|| {
                can_call_error(
                    call,
                    format!(
                        "DBC signal `{signal}` has no current frame; call `{message}.Receive()` first"
                    ),
                )
            })?;
        let raw = read_dbc_signal(&frame.bytes, definition, call)?;
        let value = match call.method.as_str() {
            "GetBit" => {
                require_signal_kind(call, signal, definition, SignalOperation::Boolean)?;
                Value::Bool(raw != 0)
            }
            "GetInteger" => {
                require_signal_kind(call, signal, definition, SignalOperation::Integer)?;
                Value::m1_integer(sign_extend(raw, definition.width) as i32)
            }
            "GetUnsignedInteger" => {
                require_signal_kind(call, signal, definition, SignalOperation::Integer)?;
                Value::m1_unsigned(u32::try_from(raw).map_err(|_| {
                    can_call_error(
                        call,
                        format!("DBC signal `{signal}` exceeds the M1 u32 width"),
                    )
                })?)
            }
            "GetFloat" => {
                require_signal_kind(call, signal, definition, SignalOperation::Float)?;
                Value::m1_float(f32::from_bits(raw as u32))
            }
            "GetScaled" => Value::m1_float(decode_scaled(call, signal, definition, raw)?),
            _ => unreachable!(),
        };
        Ok(CanReply::external(value))
    }

    fn dbc_set(&mut self, call: &HardwareCall, signal: &str) -> Result<CanReply, EvalError> {
        require_argument_count(call, 2)?;
        let definition = self.signals[signal].clone();
        let handle = handle_arg(call, 0)?;
        let raw = match call.method.as_str() {
            "SetBit" => {
                require_signal_kind(call, signal, &definition, SignalOperation::Boolean)?;
                u64::from(bool_arg(call, 1)?)
            }
            "SetInteger" => {
                require_signal_kind(call, signal, &definition, SignalOperation::Integer)?;
                encode_signed(integer_arg(call, 1)?, definition.width, call)?
            }
            "SetUnsignedInteger" => {
                require_signal_kind(call, signal, &definition, SignalOperation::Integer)?;
                encode_unsigned(integer_storage_u32_arg(call, 1)?, definition.width, call)?
            }
            "SetFloat" => {
                require_signal_kind(call, signal, &definition, SignalOperation::Float)?;
                u64::from(float_bits_arg(call, 1)?.to_bits())
            }
            "SetScaled" => {
                encode_scaled(call, signal, &definition, f64::from(float_arg(call, 1)?))?
            }
            _ => unreachable!(),
        };
        let message = definition.message.clone();
        let bytes = self
            .dbc_message_bytes_mut(call, handle, &message)?
            .as_mut()
            .ok_or_else(|| {
                can_call_error(
                    call,
                    format!(
                        "DBC handle {handle} has no payload; call `{message}.TxInitialise(handle)` first"
                    ),
                )
            })?;
        write_dbc_signal(bytes, &definition, raw, call)?;
        Ok(CanReply::value(Value::Bool(true)))
    }
}

impl HardwareAdapter for VirtualCan {
    fn call(&mut self, call: &HardwareCall) -> Result<AdapterReply, EvalError> {
        Ok(match self.call_routed(call)? {
            Some(reply) => AdapterReply::Value(reply.value),
            None => AdapterReply::Unhandled,
        })
    }
}

fn validated_injections(scenario: &CanScenario) -> Result<Vec<Injection>, EvalError> {
    let mut injections = Vec::with_capacity(scenario.rx.len());
    for (index, frame) in scenario.rx.iter().enumerate() {
        validate_scenario_frame(index, frame)?;
        injections.push((
            index,
            Injection {
                time_s: frame.time_s,
                bus: frame.bus,
                frame_id: frame.id,
                format: if frame.extended {
                    CanFrameFormat::Extended
                } else {
                    CanFrameFormat::Standard
                },
                bytes: frame.bytes.clone(),
            },
        ));
    }
    injections.sort_by(|(left_index, left), (right_index, right)| {
        left.time_s
            .total_cmp(&right.time_s)
            .then_with(|| left_index.cmp(right_index))
    });
    Ok(injections.into_iter().map(|(_, frame)| frame).collect())
}

fn validate_scenario_frame(index: usize, frame: &CanRx) -> Result<(), EvalError> {
    let declaration = index + 1;
    if !frame.time_s.is_finite() || frame.time_s < 0.0 {
        return Err(can_model_error(format!(
            "CAN rx declaration {declaration} has invalid time_s {} (expected a finite, non-negative time)",
            frame.time_s
        )));
    }
    validate_bus_number(&format!("CAN rx declaration {declaration}"), frame.bus)?;
    let maximum = if frame.extended { 0x1FFF_FFFF } else { 0x7FF };
    if frame.id > maximum {
        let format = if frame.extended {
            "extended"
        } else {
            "standard"
        };
        return Err(can_model_error(format!(
            "CAN rx declaration {declaration} has {format} identifier 0x{:X}, outside 0x0..=0x{maximum:X}",
            frame.id
        )));
    }
    if frame.bytes.len() > MAX_FRAME_BYTES {
        return Err(can_model_error(format!(
            "CAN rx declaration {declaration} has {} bytes, exceeding the {MAX_FRAME_BYTES}-byte classic CAN payload",
            frame.bytes.len()
        )));
    }
    Ok(())
}

fn validate_message_layout(message: &CanRuntimeMessage) -> Result<(), EvalError> {
    if usize::from(message.dlc) > MAX_FRAME_BYTES {
        return Err(can_model_error(format!(
            "DBC message `{}` declares DLC {}, exceeding the {MAX_FRAME_BYTES}-byte classic CAN payload supported by CanComms bit offsets",
            message.path, message.dlc
        )));
    }
    Ok(())
}

fn validate_signal_layout(
    message: &CanRuntimeMessage,
    signal: &CanRuntimeSignal,
) -> Result<(), EvalError> {
    if signal.width == 0 || signal.width > 64 {
        return Err(can_model_error(format!(
            "DBC signal `{}` has width {}, outside 1..=64",
            signal.path, signal.width
        )));
    }
    if signal.raw_kind == ValueType::Boolean && signal.width != 1 {
        return Err(can_model_error(format!(
            "DBC Boolean signal `{}` has width {}, expected exactly 1 bit",
            signal.path, signal.width
        )));
    }
    if (signal.raw_kind == ValueType::Float || signal.float)
        && (signal.raw_kind != ValueType::Float || !signal.float || signal.width != 32)
    {
        return Err(can_model_error(format!(
            "DBC float signal `{}` has width {} and float={}, expected exactly one 32-bit IEEE-754 layout",
            signal.path, signal.width, signal.float
        )));
    }
    if !signal.float && signal.width > 32 {
        return Err(can_model_error(format!(
            "DBC non-float signal `{}` has width {}, outside the evidenced 1..=32-bit runtime subset",
            signal.path, signal.width
        )));
    }
    dbc_positions(
        signal.start_bit,
        signal.width,
        signal.endian,
        usize::from(message.dlc),
    )
    .map(|_| ())
    .map_err(|detail| can_model_error(format!("DBC signal `{}`: {detail}", signal.path)))
}

fn insert_aliases(
    aliases: &mut BTreeMap<String, String>,
    values: &[String],
    canonical: &str,
    kind: &str,
) -> Result<(), EvalError> {
    for alias in values {
        if let Some(previous) = aliases.insert(alias.clone(), canonical.to_string())
            && previous != canonical
        {
            return Err(can_model_error(format!(
                "CAN {kind} alias `{alias}` is ambiguous between `{previous}` and `{canonical}`"
            )));
        }
    }
    Ok(())
}

fn resolve_alias(
    aliases: &BTreeMap<String, String>,
    call: &HardwareCall,
) -> Result<Option<String>, EvalError> {
    let canonical = aliases.get(call.receiver.name());
    let source = aliases.get(&call.source_receiver);
    match (canonical, source) {
        (Some(left), Some(right)) if left != right => Err(can_call_error(
            call,
            format!(
                "resolved receiver `{}` and source receiver `{}` name different CAN objects",
                call.receiver.name(),
                call.source_receiver
            ),
        )),
        (Some(value), _) | (_, Some(value)) => Ok(Some(value.clone())),
        (None, None) => Ok(None),
    }
}

fn next_injection<'a>(
    injections: &'a [Injection],
    cursor: usize,
    now: f64,
    config: &RxConfig,
) -> Option<(usize, &'a Injection)> {
    let mut next_cursor = cursor;
    while let Some(frame) = injections.get(next_cursor) {
        if frame.time_s > now {
            break;
        }
        next_cursor += 1;
        let comparable_mask = !config.mask
            & match config.format {
                CanFrameFormat::Standard => 0x7FF,
                CanFrameFormat::Extended => 0x1FFF_FFFF,
            };
        if frame.bus == config.bus
            && frame.format == config.format
            && (frame.frame_id & comparable_mask) == (config.match_id & comparable_mask)
        {
            return Some((next_cursor, frame));
        }
    }
    None
}

/// Base-grid tick on which a timed scenario frame first exists.
///
/// Exact grid points retain their integer tick. Off-grid arrivals round up to
/// the first tick that can observe them. A small relative tolerance prevents a
/// mathematically exact grid time represented just above an integer quotient
/// from being delayed by one tick.
fn arrival_base_tick(
    time_s: f64,
    base_period_s: f64,
    call: &HardwareCall,
) -> Result<u64, EvalError> {
    if !base_period_s.is_finite() || base_period_s <= 0.0 {
        return Err(can_call_error(
            call,
            format!("base period must be finite and positive, got {base_period_s}"),
        ));
    }
    let quotient = time_s / base_period_s;
    let nearest = quotient.round();
    let tolerance = f64::EPSILON * 16.0 * quotient.abs().max(1.0);
    let first_visible = if (quotient - nearest).abs() <= tolerance {
        nearest
    } else {
        quotient.ceil()
    };
    if !(0.0..=u64::MAX as f64).contains(&first_visible) {
        return Err(can_call_error(
            call,
            format!("scenario arrival time {time_s} cannot be represented as a base-grid tick"),
        ));
    }
    Ok(first_visible as u64)
}

fn read_raw_bit(
    bytes: &[u8],
    bit_offset: u16,
    big_endian: bool,
    call: &HardwareCall,
) -> Result<bool, EvalError> {
    Ok(read_raw_field(bytes, bit_offset, 1, big_endian, call)? != 0)
}

fn read_raw_field(
    bytes: &[u8],
    bit_offset: u16,
    width: u16,
    big_endian: bool,
    call: &HardwareCall,
) -> Result<u64, EvalError> {
    validate_raw_field(bytes.len(), bit_offset, width, big_endian, call)?;
    let mut value = 0_u64;
    for index in 0..width {
        let internal_bit = bit_offset + index;
        let (wire_byte, shift) = raw_wire_position(internal_bit, big_endian);
        value = (value << 1) | u64::from((bytes[wire_byte] >> shift) & 1);
    }
    Ok(value)
}

fn raw_wire_position(internal_bit: u16, big_endian: bool) -> (usize, u32) {
    let internal_byte = usize::from(internal_bit / 8);
    let wire_byte = if big_endian {
        internal_byte
    } else {
        MAX_FRAME_BYTES - 1 - internal_byte
    };
    // M1's canonical 64-bit CAN buffer numbers offset zero at the most
    // significant bit of its first internal byte.
    let shift = 7 - u32::from(internal_bit % 8);
    (wire_byte, shift)
}

fn write_raw_field(
    bytes: &mut [u8],
    bit_offset: u16,
    width: u16,
    big_endian: bool,
    value: u64,
    call: &HardwareCall,
) -> Result<(), EvalError> {
    validate_raw_field(bytes.len(), bit_offset, width, big_endian, call)?;
    for index in 0..width {
        let internal_bit = bit_offset + index;
        let (wire_byte, shift) = raw_wire_position(internal_bit, big_endian);
        let source_shift = u32::from(width - index - 1);
        let mask = 1_u8 << shift;
        if value & (1_u64 << source_shift) != 0 {
            bytes[wire_byte] |= mask;
        } else {
            bytes[wire_byte] &= !mask;
        }
    }
    Ok(())
}

fn validate_raw_field(
    bytes: usize,
    bit_offset: u16,
    width: u16,
    big_endian: bool,
    call: &HardwareCall,
) -> Result<(), EvalError> {
    let end = usize::from(bit_offset)
        .checked_add(usize::from(width))
        .ok_or_else(|| can_call_error(call, "bit range overflows"))?;
    if end > MAX_FRAME_BYTES * 8 {
        return Err(can_call_error(
            call,
            format!(
                "bit range {}..{end} exceeds the 64-bit CAN buffer",
                bit_offset
            ),
        ));
    }
    let active_start = if big_endian { 0 } else { 64 - bytes * 8 };
    let active_end = if big_endian { bytes * 8 } else { 64 };
    if usize::from(bit_offset) < active_start || end > active_end {
        return Err(can_call_error(
            call,
            format!(
                "bit range {}..{end} does not address the {bytes}-byte {}-aligned CAN payload (active range {active_start}..{active_end})",
                bit_offset,
                if big_endian {
                    "big-endian"
                } else {
                    "little-endian"
                }
            ),
        ));
    }
    Ok(())
}

fn dbc_positions(
    start_bit: u16,
    width: u16,
    endian: CanEndian,
    dlc: usize,
) -> Result<Vec<usize>, String> {
    let available = dlc * 8;
    let mut positions = Vec::with_capacity(usize::from(width));
    match endian {
        CanEndian::Little => {
            for index in 0..width {
                let position = usize::from(start_bit)
                    .checked_add(usize::from(index))
                    .ok_or_else(|| "bit range overflows".to_string())?;
                if position >= available {
                    return Err(format!(
                        "little-endian bit {position} exceeds DLC {dlc} ({available} bits)"
                    ));
                }
                positions.push(position);
            }
        }
        CanEndian::Big => {
            let mut position = i32::from(start_bit);
            for _ in 0..width {
                if position < 0 || usize::try_from(position).unwrap_or(usize::MAX) >= available {
                    return Err(format!(
                        "big-endian bit {position} exceeds DLC {dlc} ({available} bits)"
                    ));
                }
                positions.push(position as usize);
                position = if position % 8 == 0 {
                    position + 15
                } else {
                    position - 1
                };
            }
        }
    }
    Ok(positions)
}

fn read_dbc_signal(
    bytes: &[u8],
    signal: &SignalDef,
    call: &HardwareCall,
) -> Result<u64, EvalError> {
    let positions = dbc_positions(signal.start_bit, signal.width, signal.endian, bytes.len())
        .map_err(|detail| can_call_error(call, detail))?;
    let mut raw = 0_u64;
    match signal.endian {
        CanEndian::Little => {
            for (index, position) in positions.into_iter().enumerate() {
                let byte = bytes[position / 8];
                if byte & (1 << (position % 8)) != 0 {
                    raw |= 1_u64 << index;
                }
            }
        }
        CanEndian::Big => {
            for position in positions {
                raw = (raw << 1) | u64::from((bytes[position / 8] >> (position % 8)) & 1);
            }
        }
    }
    Ok(raw)
}

fn write_dbc_signal(
    bytes: &mut [u8],
    signal: &SignalDef,
    raw: u64,
    call: &HardwareCall,
) -> Result<(), EvalError> {
    let positions = dbc_positions(signal.start_bit, signal.width, signal.endian, bytes.len())
        .map_err(|detail| can_call_error(call, detail))?;
    let width = usize::from(signal.width);
    for (index, position) in positions.into_iter().enumerate() {
        let source_bit = match signal.endian {
            CanEndian::Little => index,
            CanEndian::Big => width - index - 1,
        };
        let mask = 1_u8 << (position % 8);
        if raw & (1_u64 << source_bit) != 0 {
            bytes[position / 8] |= mask;
        } else {
            bytes[position / 8] &= !mask;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum SignalOperation {
    Boolean,
    Integer,
    Float,
}

fn require_signal_kind(
    call: &HardwareCall,
    signal_name: &str,
    signal: &SignalDef,
    operation: SignalOperation,
) -> Result<(), EvalError> {
    let matches = match operation {
        SignalOperation::Boolean => signal.raw_kind == ValueType::Boolean && signal.width == 1,
        SignalOperation::Integer => {
            matches!(signal.raw_kind, ValueType::Integer | ValueType::Unsigned)
                && !signal.float
                && signal.width <= 32
        }
        SignalOperation::Float => signal.float && signal.width == 32,
    };
    if matches {
        Ok(())
    } else {
        let expected = match operation {
            SignalOperation::Boolean => "a one-bit Boolean",
            SignalOperation::Integer => "an integer of at most 32 bits",
            SignalOperation::Float => "a 32-bit IEEE float",
        };
        Err(can_call_error(
            call,
            format!("DBC signal `{signal_name}` is not {expected}"),
        ))
    }
}

fn decode_scaled(
    call: &HardwareCall,
    signal_name: &str,
    signal: &SignalDef,
    raw: u64,
) -> Result<f32, EvalError> {
    let raw_value = if signal.float {
        require_signal_kind(call, signal_name, signal, SignalOperation::Float)?;
        f64::from(f32::from_bits(raw as u32))
    } else if signal.signed {
        sign_extend(raw, signal.width) as f64
    } else {
        raw as f64
    };
    let physical = raw_value.mul_add(signal.scale, signal.offset);
    let narrowed = physical as f32;
    if !physical.is_finite() || narrowed.is_infinite() {
        return Err(can_call_error(
            call,
            format!(
                "DBC signal `{signal_name}` scaled value {physical} is outside finite M1 binary32"
            ),
        ));
    }
    Ok(narrowed)
}

fn encode_scaled(
    call: &HardwareCall,
    signal_name: &str,
    signal: &SignalDef,
    physical: f64,
) -> Result<u64, EvalError> {
    if !signal.scale.is_finite() || signal.scale == 0.0 || !signal.offset.is_finite() {
        return Err(can_call_error(
            call,
            format!("DBC signal `{signal_name}` has non-invertible scale/offset metadata"),
        ));
    }
    let raw = (physical - signal.offset) / signal.scale;
    if !raw.is_finite() {
        return Err(can_call_error(
            call,
            format!(
                "DBC signal `{signal_name}` physical value {physical} has no finite raw representation"
            ),
        ));
    }
    if signal.float {
        require_signal_kind(call, signal_name, signal, SignalOperation::Float)?;
        let narrowed = raw as f32;
        if narrowed.is_infinite() {
            return Err(can_call_error(
                call,
                format!("DBC signal `{signal_name}` raw float {raw} exceeds M1 binary32"),
            ));
        }
        return Ok(u64::from(narrowed.to_bits()));
    }
    let Some(rounded) = exact_grid_integer(raw) else {
        return Err(can_call_error(
            call,
            format!(
                "DBC signal `{signal_name}` physical value {physical} maps to non-integral raw value {raw}"
            ),
        ));
    };
    if signal.signed {
        let value = rounded as i128;
        let minimum = -(1_i128 << (signal.width - 1));
        let maximum = (1_i128 << (signal.width - 1)) - 1;
        if value < minimum || value > maximum {
            return Err(can_call_error(
                call,
                format!(
                    "DBC signal `{signal_name}` raw value {value} exceeds signed {}-bit range",
                    signal.width
                ),
            ));
        }
        Ok((value as i64 as u64) & bit_mask(signal.width))
    } else {
        if rounded < 0.0 || rounded > bit_mask(signal.width) as f64 {
            return Err(can_call_error(
                call,
                format!(
                    "DBC signal `{signal_name}` raw value {rounded} exceeds unsigned {}-bit range",
                    signal.width
                ),
            ));
        }
        Ok(rounded as u64)
    }
}

fn sign_extend(raw: u64, width: u16) -> i64 {
    let shift = 64 - u32::from(width);
    ((raw << shift) as i64) >> shift
}

fn bit_mask(width: u16) -> u64 {
    if width == 64 {
        u64::MAX
    } else {
        (1_u64 << width) - 1
    }
}

/// Accept only values on an integer raw grid, allowing at most a few host-f64
/// ULPs of arithmetic noise and never more than one millionth of a raw count.
/// This is intentionally fail-loud and does not claim M1 rounding parity.
fn exact_grid_integer(value: f64) -> Option<f64> {
    let rounded = value.round();
    let magnitude = rounded.abs().max(1.0);
    let exponent = ((magnitude.to_bits() >> 52) & 0x7ff) as i32;
    let ulp = if exponent == 0 {
        f64::from_bits(1)
    } else {
        2_f64.powi(exponent - 1023 - 52)
    };
    let tolerance = (ulp * 4.0).clamp(f64::EPSILON * 4.0, 1e-6);
    ((value - rounded).abs() <= tolerance).then_some(rounded)
}

fn encode_signed(value: i32, width: u16, call: &HardwareCall) -> Result<u64, EvalError> {
    let minimum = -(1_i64 << (width - 1));
    let maximum = (1_i64 << (width - 1)) - 1;
    let value64 = i64::from(value);
    if value64 < minimum || value64 > maximum {
        return Err(can_call_error(
            call,
            format!("signed value {value} exceeds the {width}-bit range {minimum}..={maximum}"),
        ));
    }
    Ok((value64 as u64) & bit_mask(width))
}

fn encode_unsigned(value: u32, width: u16, call: &HardwareCall) -> Result<u64, EvalError> {
    if u64::from(value) > bit_mask(width) {
        return Err(can_call_error(
            call,
            format!("unsigned value {value} exceeds the {width}-bit range"),
        ));
    }
    Ok(u64::from(value))
}

fn validate_bus_number(context: &str, bus: i32) -> Result<(), EvalError> {
    if (0..=2).contains(&bus) {
        Ok(())
    } else {
        Err(can_model_error(format!(
            "{context} has bus {bus}, outside the catalogue range 0..=2"
        )))
    }
}

fn bus_arg(call: &HardwareCall, index: usize) -> Result<i32, EvalError> {
    let bus = integer_arg(call, index)?;
    if (0..=2).contains(&bus) {
        Ok(bus)
    } else {
        Err(can_call_error(
            call,
            format!("bus must be in the catalogue range 0..=2, got {bus}"),
        ))
    }
}

/// Apply the captured CAN call-boundary widening for floating-point
/// parameters. M1 Integer and UnsignedInteger values widen to binary32, while
/// FixedPoint7dps remains a distinct family and is not an implicit CAN float.
fn normalize_can_float(
    call: &HardwareCall,
    index: usize,
    value: &Value,
) -> Result<Value, EvalError> {
    match value {
        Value::M1(M1Scalar::FloatingPoint(value)) => Ok(Value::m1_float(*value)),
        Value::M1(M1Scalar::Integer(value)) => Ok(Value::m1_float(*value as f32)),
        Value::M1(M1Scalar::UnsignedInteger(value)) => Ok(Value::m1_float(*value as f32)),
        other => Err(can_call_error(
            call,
            format!(
                "argument {} must be M1 FloatingPoint, Integer, or UnsignedInteger, got {other:?}",
                index + 1
            ),
        )),
    }
}

fn id_arg(
    call: &HardwareCall,
    index: usize,
    format: CanFrameFormat,
    name: &str,
) -> Result<u32, EvalError> {
    let value = numeric_u32_arg(call, index)?;
    let maximum = match format {
        CanFrameFormat::Standard => 0x7FF,
        CanFrameFormat::Extended => 0x1FFF_FFFF,
    };
    if value <= maximum {
        Ok(value)
    } else {
        Err(can_call_error(
            call,
            format!("{name} 0x{value:X} exceeds the 0x{maximum:X} frame identifier limit"),
        ))
    }
}

fn handle_arg(call: &HardwareCall, index: usize) -> Result<u32, EvalError> {
    match call.arguments.get(index) {
        Some(Value::M1(M1Scalar::UnsignedInteger(value))) => Ok(*value),
        Some(value) => Err(can_call_error(
            call,
            format!("argument {} must be an M1 Handle, got {value:?}", index + 1),
        )),
        None => Err(missing_arg(call, index)),
    }
}

fn integer_arg(call: &HardwareCall, index: usize) -> Result<i32, EvalError> {
    match call.arguments.get(index) {
        Some(Value::M1(M1Scalar::Integer(value))) => Ok(*value),
        Some(value) => Err(can_call_error(
            call,
            format!("argument {} must be M1 Integer, got {value:?}", index + 1),
        )),
        None => Err(missing_arg(call, index)),
    }
}

fn numeric_u32_arg(call: &HardwareCall, index: usize) -> Result<u32, EvalError> {
    match call.arguments.get(index) {
        Some(Value::M1(M1Scalar::UnsignedInteger(value))) => Ok(*value),
        Some(Value::M1(M1Scalar::Integer(value))) if *value >= 0 => Ok(*value as u32),
        Some(value) => Err(can_call_error(
            call,
            format!(
                "argument {} must be a non-negative M1 Integer or UnsignedInteger, got {value:?}",
                index + 1
            ),
        )),
        None => Err(missing_arg(call, index)),
    }
}

/// Interpret the catalogue's signed `Integer` storage bits as an unsigned
/// payload. This is only for `SetUnsignedInteger.value`; identifiers, masks,
/// bus numbers, and lengths retain their non-negative range checks.
fn integer_storage_u32_arg(call: &HardwareCall, index: usize) -> Result<u32, EvalError> {
    match call.arguments.get(index) {
        Some(Value::M1(M1Scalar::Integer(value))) => Ok(*value as u32),
        Some(Value::M1(M1Scalar::UnsignedInteger(value))) => Ok(*value),
        Some(value) => Err(can_call_error(
            call,
            format!(
                "argument {} must be normalized M1 Integer storage, got {value:?}",
                index + 1
            ),
        )),
        None => Err(missing_arg(call, index)),
    }
}

fn positive_integer_arg(call: &HardwareCall, index: usize, name: &str) -> Result<i32, EvalError> {
    let value = integer_arg(call, index)?;
    if value > 0 {
        Ok(value)
    } else {
        Err(can_call_error(
            call,
            format!("{name} must be positive, got {value}"),
        ))
    }
}

fn bool_arg(call: &HardwareCall, index: usize) -> Result<bool, EvalError> {
    match call.arguments.get(index) {
        Some(Value::Bool(value)) => Ok(*value),
        Some(value) => Err(can_call_error(
            call,
            format!("argument {} must be Boolean, got {value:?}", index + 1),
        )),
        None => Err(missing_arg(call, index)),
    }
}

fn float_arg(call: &HardwareCall, index: usize) -> Result<f32, EvalError> {
    match call.arguments.get(index) {
        Some(Value::M1(M1Scalar::FloatingPoint(value))) if value.is_finite() => Ok(*value),
        Some(value) => Err(can_call_error(
            call,
            format!(
                "argument {} must be finite M1 FloatingPoint, got {value:?}",
                index + 1
            ),
        )),
        None => Err(missing_arg(call, index)),
    }
}

fn float_bits_arg(call: &HardwareCall, index: usize) -> Result<f32, EvalError> {
    match call.arguments.get(index) {
        Some(Value::M1(M1Scalar::FloatingPoint(value))) => Ok(*value),
        Some(value) => Err(can_call_error(
            call,
            format!(
                "argument {} must be M1 FloatingPoint, got {value:?}",
                index + 1
            ),
        )),
        None => Err(missing_arg(call, index)),
    }
}

fn fixed_arg(call: &HardwareCall, index: usize) -> Result<FixedPoint7dps, EvalError> {
    match call.arguments.get(index) {
        Some(Value::M1(M1Scalar::FixedPoint7dps(value))) => Ok(*value),
        Some(value) => Err(can_call_error(
            call,
            format!(
                "argument {} must be M1 FixedPoint7dps, got {value:?}",
                index + 1
            ),
        )),
        None => Err(missing_arg(call, index)),
    }
}

fn bit_offset_arg(call: &HardwareCall, index: usize) -> Result<u16, EvalError> {
    let value = integer_arg(call, index)?;
    if (0..=63).contains(&value) {
        Ok(value as u16)
    } else {
        Err(can_call_error(
            call,
            format!("bit offset must be in 0..=63, got {value}"),
        ))
    }
}

fn integer_bit_width(call: &HardwareCall, index: usize) -> Result<u16, EvalError> {
    let value = integer_arg(call, index)?;
    if (1..=32).contains(&value) {
        Ok(value as u16)
    } else {
        Err(can_call_error(
            call,
            format!("integer bit length must be in 1..=32, got {value}"),
        ))
    }
}

fn frame_length_arg(call: &HardwareCall, index: usize) -> Result<usize, EvalError> {
    let value = integer_arg(call, index)?;
    if (0..=MAX_FRAME_BYTES as i32).contains(&value) {
        Ok(value as usize)
    } else {
        Err(can_call_error(
            call,
            format!("frame length must be in 0..={MAX_FRAME_BYTES}, got {value}"),
        ))
    }
}

fn require_no_arguments(call: &HardwareCall) -> Result<(), EvalError> {
    require_argument_count(call, 0)
}

fn require_argument_count(call: &HardwareCall, expected: usize) -> Result<(), EvalError> {
    if call.arguments.len() == expected {
        Ok(())
    } else {
        Err(can_call_error(
            call,
            format!(
                "expected {expected} argument{}, got {}",
                if expected == 1 { "" } else { "s" },
                call.arguments.len()
            ),
        ))
    }
}

fn missing_arg(call: &HardwareCall, index: usize) -> EvalError {
    can_call_error(call, format!("missing argument {}", index + 1))
}

fn can_call_error(call: &HardwareCall, detail: impl Into<String>) -> EvalError {
    EvalError::BadCall {
        detail: format!("{}: {}", call.canonical_name(), detail.into()),
    }
}

fn can_method_error(method: &str, detail: impl Into<String>) -> EvalError {
    EvalError::BadCall {
        detail: format!("CanComms.{method}: {}", detail.into()),
    }
}

fn can_model_error(detail: impl Into<String>) -> EvalError {
    EvalError::UnsupportedConstruct {
        kind: format!("virtual CAN configuration error: {}", detail.into()),
        at: 0,
    }
}

fn unsupported_can_object(call: &HardwareCall) -> EvalError {
    EvalError::UnsupportedBuiltin {
        object: call.source_receiver.clone(),
        method: call.method.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{EvalTime, ResolvedReceiver};
    use m1_can::{CanRuntimeModule, CanRuntimeSignal};

    fn library_call(method: &str, site: usize, arguments: Vec<Value>, time: f64) -> HardwareCall {
        HardwareCall {
            receiver: ResolvedReceiver::Library {
                object: "CanComms".to_string(),
            },
            source_receiver: "CanComms".to_string(),
            method: method.to_string(),
            site: CallSite::new("CAN.Test.m1scr", site),
            arguments,
            time: EvalTime::periodic((time * 100.0) as u64, time, 0.01, 0.01),
        }
    }

    fn project_call(
        receiver: &str,
        method: &str,
        site: usize,
        arguments: Vec<Value>,
        time: f64,
    ) -> HardwareCall {
        HardwareCall {
            receiver: ResolvedReceiver::Project {
                path: receiver.to_string(),
            },
            source_receiver: receiver.to_string(),
            method: method.to_string(),
            site: CallSite::new("CAN.Project Test.m1scr", site),
            arguments,
            time: EvalTime::periodic((time * 100.0) as u64, time, 0.01, 0.01),
        }
    }

    #[derive(Clone, Copy)]
    struct SignalSpec {
        raw_kind: ValueType,
        signed: bool,
        endian: CanEndian,
        start_bit: u16,
        width: u16,
        scale: f64,
        offset: f64,
    }

    impl SignalSpec {
        fn new(raw_kind: ValueType, start_bit: u16, width: u16) -> Self {
            SignalSpec {
                raw_kind,
                signed: false,
                endian: CanEndian::Little,
                start_bit,
                width,
                scale: 1.0,
                offset: 0.0,
            }
        }

        fn signed(mut self) -> Self {
            self.signed = true;
            self
        }

        fn big_endian(mut self) -> Self {
            self.endian = CanEndian::Big;
            self
        }

        fn scaled(mut self, scale: f64, offset: f64) -> Self {
            self.scale = scale;
            self.offset = offset;
            self
        }
    }

    fn signal(path: &str, spec: SignalSpec) -> CanRuntimeSignal {
        CanRuntimeSignal {
            path: path.to_string(),
            aliases: vec![path.to_string(), format!("DBC.{path}")],
            raw_type: match spec.raw_kind {
                ValueType::Integer => "s32",
                ValueType::Unsigned => "u32",
                ValueType::Float => "f32",
                ValueType::Boolean => "bool",
                _ => "unknown",
            }
            .to_string(),
            raw_kind: spec.raw_kind,
            signed: spec.signed,
            float: spec.raw_kind == ValueType::Float,
            endian: spec.endian,
            start_bit: spec.start_bit,
            width: spec.width,
            scale: spec.scale,
            offset: spec.offset,
        }
    }

    fn dbc_model() -> CanRuntimeModel {
        let rx_path = "Vehicle Network.Status Frame";
        let tx_path = "Vehicle Network.Command Frame";
        let other_path = "Vehicle Network.Other Command";
        CanRuntimeModel {
            modules: vec![CanRuntimeModule {
                path: "Vehicle Network".to_string(),
                aliases: vec![
                    "Vehicle Network".to_string(),
                    "DBC.Vehicle Network".to_string(),
                ],
                source_path: "dbc/vendor files/Vehicle Network.m1dbc".to_string(),
                initialised: true,
                bus: Some("0".to_string()),
                bus_kind: "literal".to_string(),
                bus_value: Some(0),
                bus_calibrated: false,
                messages: vec![
                    CanRuntimeMessage {
                        path: rx_path.to_string(),
                        aliases: vec![rx_path.to_string(), format!("DBC.{rx_path}")],
                        frame_id: 0x123,
                        format: CanFrameFormat::Standard,
                        dlc: 2,
                        direction: Some(CanDirection::Rx),
                        signals: vec![
                            signal(
                                &format!("{rx_path}.Signed Count"),
                                SignalSpec::new(ValueType::Integer, 0, 8)
                                    .signed()
                                    .scaled(2.0, 1.0),
                            ),
                            signal(
                                &format!("{rx_path}.Motorola Count"),
                                SignalSpec::new(ValueType::Unsigned, 7, 16).big_endian(),
                            ),
                        ],
                    },
                    CanRuntimeMessage {
                        path: tx_path.to_string(),
                        aliases: vec![tx_path.to_string(), format!("DBC.{tx_path}")],
                        frame_id: 0x321,
                        format: CanFrameFormat::Standard,
                        dlc: 4,
                        direction: Some(CanDirection::Tx),
                        signals: vec![
                            signal(
                                &format!("{tx_path}.Command"),
                                SignalSpec::new(ValueType::Unsigned, 0, 8),
                            ),
                            signal(
                                &format!("{tx_path}.Scaled Command"),
                                SignalSpec::new(ValueType::Unsigned, 15, 16)
                                    .big_endian()
                                    .scaled(2.0, 1.0),
                            ),
                            signal(
                                &format!("{tx_path}.Float Command"),
                                SignalSpec::new(ValueType::Float, 0, 32),
                            ),
                            signal(
                                &format!("{tx_path}.Low Nibble"),
                                SignalSpec::new(ValueType::Unsigned, 0, 4),
                            ),
                            signal(
                                &format!("{tx_path}.Overlapping Nibble"),
                                SignalSpec::new(ValueType::Unsigned, 2, 4),
                            ),
                        ],
                    },
                    CanRuntimeMessage {
                        path: other_path.to_string(),
                        aliases: vec![other_path.to_string(), format!("DBC.{other_path}")],
                        frame_id: 0x322,
                        format: CanFrameFormat::Standard,
                        dlc: 1,
                        direction: Some(CanDirection::Tx),
                        signals: vec![signal(
                            &format!("{other_path}.Other"),
                            SignalSpec::new(ValueType::Unsigned, 0, 8),
                        )],
                    },
                ],
            }],
            skipped_scripts: Vec::new(),
        }
    }

    fn project_invoke(
        can: &mut VirtualCan,
        receiver: &str,
        method: &str,
        site: usize,
        arguments: Vec<Value>,
        time: f64,
    ) -> Result<CanReply, EvalError> {
        can.call_routed(&project_call(receiver, method, site, arguments, time))?
            .ok_or_else(|| EvalError::UnsupportedBuiltin {
                object: receiver.to_string(),
                method: method.to_string(),
            })
    }

    fn invoke(
        can: &mut VirtualCan,
        method: &str,
        site: usize,
        arguments: Vec<Value>,
        time: f64,
    ) -> CanReply {
        can.call_routed(&library_call(method, site, arguments, time))
            .expect("CAN call succeeds")
            .expect("method is handled")
    }

    fn initialize(can: &mut VirtualCan) {
        invoke(
            can,
            "Init",
            1,
            vec![Value::m1_integer(0), Value::m1_integer(500)],
            0.0,
        );
    }

    fn open_rx(can: &mut VirtualCan, site: usize) -> u32 {
        let reply = invoke(
            can,
            "RxOpenStandard",
            site,
            vec![
                Value::m1_integer(0),
                Value::m1_integer(0x123),
                Value::m1_integer(0),
                Value::Bool(false),
            ],
            0.0,
        );
        match reply.value {
            Value::M1(M1Scalar::UnsignedInteger(handle)) => handle,
            other => panic!("unexpected handle {other:?}"),
        }
    }

    #[test]
    fn raw_receivers_have_stable_nonzero_handles_and_independent_cursors() {
        let scenario = CanScenario {
            rx: vec![CanRx {
                time_s: 0.1,
                bus: 0,
                id: 0x123,
                extended: false,
                bytes: vec![0x34, 0x12],
            }],
        };
        let mut can = VirtualCan::new(
            &CanRuntimeModel {
                modules: Vec::new(),
                skipped_scripts: Vec::new(),
            },
            &scenario,
        )
        .unwrap();
        initialize(&mut can);
        let first = open_rx(&mut can, 10);
        assert_eq!(open_rx(&mut can, 10), first);
        let second = open_rx(&mut can, 20);
        assert_ne!(first, 0);
        assert_ne!(first, second);

        for handle in [first, second] {
            let received = invoke(
                &mut can,
                "RxMessage",
                30,
                vec![Value::m1_unsigned(handle)],
                0.1,
            );
            assert_eq!(received.value, Value::Bool(true));
            assert_eq!(received.event.unwrap().bytes, vec![0x34, 0x12]);
        }
    }

    #[test]
    fn raw_fields_follow_m1s_reversed_little_and_msb_first_big_numbering() {
        for (big_endian, expected) in [
            (false, vec![0x00, 0x00, 0x50, 0x03]),
            (true, vec![0x03, 0x50, 0x00, 0x00]),
        ] {
            let mut can = VirtualCan::empty();
            let handle =
                match invoke(&mut can, "TxOpen", 10, vec![Value::Bool(big_endian)], 0.0).value {
                    Value::M1(M1Scalar::UnsignedInteger(handle)) => handle,
                    other => panic!("unexpected handle {other:?}"),
                };
            invoke(
                &mut can,
                "TxInitialise",
                20,
                vec![Value::m1_unsigned(handle), Value::m1_integer(4)],
                0.0,
            );
            // Little/Intel reverses wire bytes into the fixed 64-bit internal
            // buffer, so wire bytes 2/3 occupy internal bits 32..47. Big/
            // Motorola keeps wire order and permits a sub-byte field.
            let (offset, width) = if big_endian { (4, 12) } else { (32, 16) };
            invoke(
                &mut can,
                "SetUnsignedInteger",
                30,
                vec![
                    Value::m1_unsigned(handle),
                    Value::m1_integer(offset),
                    Value::m1_integer(width),
                    Value::m1_integer(0x350),
                ],
                0.0,
            );
            assert_eq!(
                can.handle_bytes(&library_call("X", 0, vec![], 0.0), handle)
                    .unwrap(),
                expected
            );
        }
    }

    #[test]
    fn raw_short_payloads_use_canonical_64_bit_offsets() {
        let call = library_call("SetUnsignedInteger", 35, vec![], 0.0);
        let mut big = vec![0; 2];
        write_raw_field(&mut big, 0, 16, true, 0x1234, &call).unwrap();
        assert_eq!(big, vec![0x12, 0x34]);

        let mut little = vec![0; 2];
        write_raw_field(&mut little, 48, 16, false, 0x1234, &call).unwrap();
        assert_eq!(little, vec![0x34, 0x12]);
        let error = write_raw_field(&mut little, 0, 1, false, 1, &call)
            .expect_err("little DLC2 has no wire byte at canonical offset zero");
        assert!(error.to_string().contains("active range 48..64"), "{error}");

        let mut bits = vec![0; 8];
        write_raw_field(&mut bits, 56, 1, false, 1, &call).unwrap();
        write_raw_field(&mut bits, 63, 1, false, 1, &call).unwrap();
        assert_eq!(bits[0], 0x81);
        let mut big_bits = vec![0; 8];
        write_raw_field(&mut big_bits, 0, 1, true, 1, &call).unwrap();
        write_raw_field(&mut big_bits, 7, 1, true, 1, &call).unwrap();
        assert_eq!(big_bits[0], 0x81);
    }

    #[test]
    fn raw_codec_preserves_special_float_fixed_and_unsigned_storage_bits() {
        for (big_endian, offset, expected_nan) in [
            (true, 0, vec![0x7f, 0xc0, 0x12, 0x34]),
            (false, 32, vec![0x34, 0x12, 0xc0, 0x7f]),
        ] {
            let mut can = VirtualCan::empty();
            let handle =
                match invoke(&mut can, "TxOpen", 40, vec![Value::Bool(big_endian)], 0.0).value {
                    Value::M1(M1Scalar::UnsignedInteger(handle)) => handle,
                    other => panic!("unexpected handle {other:?}"),
                };
            invoke(
                &mut can,
                "TxInitialise",
                41,
                vec![Value::m1_integer(handle as i32), Value::m1_unsigned(4)],
                0.0,
            );
            let nan = f32::from_bits(0x7fc0_1234);
            invoke(
                &mut can,
                "SetFloat",
                42,
                vec![
                    Value::m1_integer(handle as i32),
                    Value::m1_unsigned(offset),
                    Value::m1_float(nan),
                ],
                0.0,
            );
            assert_eq!(
                can.handle_bytes(&library_call("X", 0, vec![], 0.0), handle)
                    .unwrap(),
                expected_nan
            );
            let read = invoke(
                &mut can,
                "GetFloat",
                43,
                vec![Value::m1_integer(handle as i32), Value::m1_unsigned(offset)],
                0.0,
            );
            let Value::M1(M1Scalar::FloatingPoint(read)) = read.value else {
                panic!("expected float")
            };
            assert_eq!(read.to_bits(), nan.to_bits());
            assert!(
                !invoke(
                    &mut can,
                    "GetFloat",
                    44,
                    vec![Value::m1_unsigned(handle), Value::m1_integer(offset as i32)],
                    0.0,
                )
                .external
            );

            invoke(
                &mut can,
                "SetFixed7DP",
                45,
                vec![
                    Value::m1_unsigned(handle),
                    Value::m1_integer(offset as i32),
                    Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(
                        -12_500_000,
                    ))),
                ],
                0.0,
            );
            let expected_fixed = if big_endian {
                vec![0xff, 0x41, 0x43, 0xe0]
            } else {
                vec![0xe0, 0x43, 0x41, 0xff]
            };
            assert_eq!(
                can.handle_bytes(&library_call("X", 0, vec![], 0.0), handle)
                    .unwrap(),
                expected_fixed
            );
            let non_fixed = can
                .call_routed(&library_call(
                    "SetFixed7DP",
                    45,
                    vec![
                        Value::m1_unsigned(handle),
                        Value::m1_integer(offset as i32),
                        Value::m1_integer(-1),
                    ],
                    0.0,
                ))
                .expect_err("SetFixed7DP requires exact fixed storage");
            assert!(matches!(non_fixed, EvalError::TypeError { .. }));

            let fixed_float = can
                .call_routed(&library_call(
                    "SetFloat",
                    46,
                    vec![
                        Value::m1_unsigned(handle),
                        Value::m1_integer(offset as i32),
                        Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(1))),
                    ],
                    0.0,
                ))
                .expect_err("SetFloat does not implicitly widen FixedPoint7dps");
            assert!(fixed_float.to_string().contains("FloatingPoint"));

            invoke(
                &mut can,
                "SetUnsignedInteger",
                47,
                vec![
                    Value::m1_unsigned(handle),
                    Value::m1_integer(offset as i32),
                    Value::m1_integer(32),
                    Value::m1_unsigned(u32::MAX),
                ],
                0.0,
            );
            assert_eq!(
                can.handle_bytes(&library_call("X", 0, vec![], 0.0), handle)
                    .unwrap(),
                &[0xff; 4]
            );
        }
    }

    #[test]
    fn raw_zero_dlc_arrival_ticks_and_bus_lifecycle_are_exact() {
        let scenario = CanScenario {
            rx: vec![
                CanRx {
                    time_s: 0.02,
                    bus: 0,
                    id: 0x123,
                    extended: false,
                    bytes: vec![1, 2, 3],
                },
                CanRx {
                    time_s: 0.025,
                    bus: 0,
                    id: 0x123,
                    extended: false,
                    bytes: vec![],
                },
            ],
        };
        let mut can = VirtualCan::new(
            &CanRuntimeModel {
                modules: Vec::new(),
                skipped_scripts: Vec::new(),
            },
            &scenario,
        )
        .unwrap();
        initialize(&mut can);
        let rx = open_rx(&mut can, 50);
        for (expected_tick, expected_length) in [(2, 3), (3, 0)] {
            assert_eq!(
                invoke(
                    &mut can,
                    "RxMessage",
                    51,
                    vec![Value::m1_integer(rx as i32)],
                    0.09,
                )
                .value,
                Value::Bool(true)
            );
            assert_eq!(
                invoke(
                    &mut can,
                    "GetTicks",
                    52,
                    vec![Value::m1_integer(rx as i32)],
                    0.09,
                )
                .value,
                Value::m1_unsigned(expected_tick)
            );
            let length = invoke(
                &mut can,
                "GetLength",
                53,
                vec![Value::m1_integer(rx as i32)],
                0.09,
            );
            assert_eq!(length.value, Value::m1_integer(expected_length));
            assert!(length.external, "received lengths retain RX provenance");
        }

        let tx = match invoke(&mut can, "TxOpen", 54, vec![Value::Bool(true)], 0.0).value {
            Value::M1(M1Scalar::UnsignedInteger(handle)) => handle,
            other => panic!("unexpected handle {other:?}"),
        };
        let uninitialized_length = can
            .call_routed(&library_call(
                "GetLength",
                55,
                vec![Value::m1_unsigned(tx)],
                0.0,
            ))
            .expect_err("an uninitialized TX handle has no specified length");
        assert!(
            uninitialized_length.to_string().contains("TxInitialise"),
            "{uninitialized_length}"
        );
        let before = can
            .call_routed(&library_call(
                "TxStandard",
                56,
                vec![
                    Value::m1_unsigned(tx),
                    Value::m1_integer(0),
                    Value::m1_integer(0x100),
                ],
                0.0,
            ))
            .expect_err("never-initialized TX fails");
        assert!(before.to_string().contains("TxInitialise"), "{before}");
        invoke(
            &mut can,
            "TxInitialise",
            57,
            vec![Value::m1_unsigned(tx), Value::m1_integer(3)],
            0.0,
        );
        let length = invoke(&mut can, "GetLength", 58, vec![Value::m1_unsigned(tx)], 0.0);
        assert_eq!(length.value, Value::m1_integer(3));
        assert!(!length.external, "TX length is evaluator-owned state");
        invoke(
            &mut can,
            "TxInitialise",
            59,
            vec![Value::m1_unsigned(tx), Value::m1_integer(0)],
            0.0,
        );
        let empty_length = invoke(&mut can, "GetLength", 60, vec![Value::m1_unsigned(tx)], 0.0);
        assert_eq!(empty_length.value, Value::m1_integer(0));
        assert!(!empty_length.external, "DLC 0 TX remains evaluator-owned");
        let sent = invoke(
            &mut can,
            "TxStandard",
            61,
            vec![
                Value::m1_unsigned(tx),
                Value::m1_integer(0),
                Value::m1_integer(0x100),
            ],
            0.0,
        );
        assert_eq!(sent.event.unwrap().bytes, Vec::<u8>::new());

        let conflict = can
            .call_routed(&library_call(
                "Init",
                62,
                vec![Value::m1_unsigned(0), Value::m1_unsigned(250)],
                0.0,
            ))
            .expect_err("known kbaud conflicts");
        assert!(conflict.to_string().contains("500 kbaud"), "{conflict}");
    }

    #[test]
    fn dbc_message_receive_shares_frame_and_raw_accessor_selects_interpretation() {
        let scenario = CanScenario {
            rx: vec![CanRx {
                time_s: 0.0,
                bus: 0,
                id: 0x123,
                extended: false,
                bytes: vec![0xfe, 0x00],
            }],
        };
        let mut can = VirtualCan::new(&dbc_model(), &scenario).unwrap();
        project_invoke(
            &mut can,
            "DBC.Vehicle Network",
            "Init",
            60,
            vec![Value::m1_unsigned(0)],
            0.0,
        )
        .unwrap();
        assert_eq!(
            project_invoke(
                &mut can,
                "DBC.Vehicle Network.Status Frame",
                "Receive",
                61,
                vec![],
                0.0,
            )
            .unwrap()
            .value,
            Value::Bool(true)
        );
        let signed_signal = "DBC.Vehicle Network.Status Frame.Signed Count";
        assert_eq!(
            project_invoke(&mut can, signed_signal, "GetInteger", 62, vec![], 0.0)
                .unwrap()
                .value,
            Value::m1_integer(-2)
        );
        assert_eq!(
            project_invoke(
                &mut can,
                signed_signal,
                "GetUnsignedInteger",
                63,
                vec![],
                0.0,
            )
            .unwrap()
            .value,
            Value::m1_unsigned(254)
        );
        assert_eq!(
            project_invoke(
                &mut can,
                "Vehicle Network.Status Frame.Motorola Count",
                "GetUnsignedInteger",
                64,
                vec![],
                0.0,
            )
            .unwrap()
            .value,
            Value::m1_unsigned(0xfe00),
            "Motorola decode traverses byte zero MSB-first then byte one"
        );
        let error = project_invoke(&mut can, signed_signal, "Receive", 65, vec![], 0.0)
            .expect_err("Receive belongs to the message, not a child signal");
        assert!(matches!(error, EvalError::UnsupportedBuiltin { .. }));
    }

    #[test]
    fn dbc_tx_handles_own_independent_buffers_and_validate_message_identity() {
        let mut can = VirtualCan::new(&dbc_model(), &CanScenario::default()).unwrap();
        project_invoke(
            &mut can,
            "Vehicle Network",
            "Init",
            70,
            vec![Value::m1_unsigned(0)],
            0.0,
        )
        .unwrap();
        let message = "Vehicle Network.Command Frame";
        let open = |can: &mut VirtualCan, site| match project_invoke(
            can,
            message,
            "TxOpen",
            site,
            vec![],
            0.0,
        )
        .unwrap()
        .value
        {
            Value::M1(M1Scalar::UnsignedInteger(handle)) => handle,
            other => panic!("unexpected handle {other:?}"),
        };
        let first = open(&mut can, 71);
        let second = open(&mut can, 72);
        assert_ne!(first, second);
        for (handle, value) in [(first, 0x12), (second, 0x34)] {
            project_invoke(
                &mut can,
                message,
                "TxInitialise",
                73,
                vec![Value::m1_integer(handle as i32)],
                0.0,
            )
            .unwrap();
            project_invoke(
                &mut can,
                "Vehicle Network.Command Frame.Command",
                "SetUnsignedInteger",
                74,
                vec![Value::m1_integer(handle as i32), Value::m1_unsigned(value)],
                0.0,
            )
            .unwrap();
        }
        let first_event = project_invoke(
            &mut can,
            message,
            "Tx",
            75,
            vec![Value::m1_integer(first as i32)],
            0.0,
        )
        .unwrap()
        .event
        .unwrap();
        let second_event = project_invoke(
            &mut can,
            message,
            "Tx",
            76,
            vec![Value::m1_unsigned(second)],
            0.0,
        )
        .unwrap()
        .event
        .unwrap();
        assert_eq!(first_event.bytes[0], 0x12);
        assert_eq!(second_event.bytes[0], 0x34);
        assert_eq!(first_event.handle, Some(first));
        assert_eq!(second_event.handle, Some(second));

        let wrong = project_invoke(
            &mut can,
            "Vehicle Network.Other Command",
            "TxInitialise",
            77,
            vec![Value::m1_unsigned(first)],
            0.0,
        )
        .expect_err("message handles cannot cross buffers");
        assert!(wrong.to_string().contains("Command Frame"), "{wrong}");

        let scaled_handle = open(&mut can, 78);
        project_invoke(
            &mut can,
            message,
            "TxInitialise",
            79,
            vec![Value::m1_unsigned(scaled_handle)],
            0.0,
        )
        .unwrap();
        for (signal, method) in [
            ("Vehicle Network.Command Frame.Scaled Command", "SetScaled"),
            ("Vehicle Network.Command Frame.Float Command", "SetFloat"),
        ] {
            let error = project_invoke(
                &mut can,
                signal,
                method,
                80,
                vec![
                    Value::m1_unsigned(scaled_handle),
                    Value::M1(M1Scalar::FixedPoint7dps(FixedPoint7dps::from_raw(1))),
                ],
                0.0,
            )
            .expect_err("DBC floating parameters do not implicitly widen FixedPoint7dps");
            assert!(error.to_string().contains("FloatingPoint"), "{error}");
        }
        project_invoke(
            &mut can,
            "Vehicle Network.Command Frame.Scaled Command",
            "SetScaled",
            80,
            vec![Value::m1_unsigned(scaled_handle), Value::m1_integer(5)],
            0.0,
        )
        .unwrap();
        let event = project_invoke(
            &mut can,
            message,
            "Tx",
            81,
            vec![Value::m1_unsigned(scaled_handle)],
            0.0,
        )
        .unwrap()
        .event
        .unwrap();
        assert_eq!(event.bytes, vec![0, 0, 2, 0]);

        let overlap_handle = open(&mut can, 82);
        project_invoke(
            &mut can,
            message,
            "TxInitialise",
            83,
            vec![Value::m1_unsigned(overlap_handle)],
            0.0,
        )
        .unwrap();
        project_invoke(
            &mut can,
            "Vehicle Network.Command Frame.Low Nibble",
            "SetUnsignedInteger",
            84,
            vec![
                Value::m1_unsigned(overlap_handle),
                Value::m1_integer(0b1010),
            ],
            0.0,
        )
        .unwrap();
        project_invoke(
            &mut can,
            "Vehicle Network.Command Frame.Overlapping Nibble",
            "SetUnsignedInteger",
            85,
            vec![
                Value::m1_unsigned(overlap_handle),
                Value::m1_integer(0b0011),
            ],
            0.0,
        )
        .unwrap();
        let event = project_invoke(
            &mut can,
            message,
            "Tx",
            86,
            vec![Value::m1_unsigned(overlap_handle)],
            0.0,
        )
        .unwrap()
        .event
        .unwrap();
        assert_eq!(
            event.bytes,
            vec![0x0e, 0, 0, 0],
            "overlap is last-write-wins while bits outside the written fields remain unchanged"
        );
    }

    #[test]
    fn scaled_integer_encoding_rejects_large_non_grid_values() {
        let definition = SignalDef {
            message: "M".to_string(),
            raw_kind: ValueType::Unsigned,
            signed: false,
            float: false,
            endian: CanEndian::Little,
            start_bit: 0,
            width: 32,
            scale: 1.0,
            offset: 0.0,
        };
        let call = project_call("M.S", "SetScaled", 90, vec![], 0.0);
        for raw in [1_000_000.25, 1_000_000_000.5] {
            let error = encode_scaled(&call, "M.S", &definition, raw)
                .expect_err("large non-grid raw values fail loud");
            assert!(error.to_string().contains("non-integral"), "{error}");
        }
        assert_eq!(
            encode_scaled(&call, "M.S", &definition, 1_000_000_000.0).unwrap(),
            1_000_000_000
        );
    }

    #[test]
    fn scenario_and_dbc_layout_bounds_fail_loud() {
        let bad_scenario = CanScenario {
            rx: vec![CanRx {
                time_s: 0.0,
                bus: 0,
                id: 0x800,
                extended: false,
                bytes: vec![],
            }],
        };
        let error = VirtualCan::new(
            &CanRuntimeModel {
                modules: Vec::new(),
                skipped_scripts: Vec::new(),
            },
            &bad_scenario,
        )
        .err()
        .expect("invalid standard ID fails");
        assert!(error.to_string().contains("0x800"), "{error}");

        let mut width_model = dbc_model();
        width_model.modules[0].messages[0].dlc = 8;
        width_model.modules[0].messages[0].signals[0].width = 32;
        VirtualCan::new(&width_model, &CanScenario::default())
            .expect("a 32-bit DBC integer is in the supported subset");
        width_model.modules[0].messages[0].signals[0].width = 33;
        let error = VirtualCan::new(&width_model, &CanScenario::default())
            .err()
            .expect("a 33-bit DBC integer is outside the supported subset");
        assert!(error.to_string().contains("1..=32-bit"), "{error}");

        width_model.modules[0].messages[0].signals[0].raw_kind = ValueType::Unknown;
        let error = VirtualCan::new(&width_model, &CanScenario::default())
            .err()
            .expect("an unknown 33-bit non-float layout is also rejected");
        assert!(error.to_string().contains("1..=32-bit"), "{error}");

        let mut float_model = dbc_model();
        float_model.modules[0].messages[1].signals[2].width = 31;
        let error = VirtualCan::new(&float_model, &CanScenario::default())
            .err()
            .expect("a DBC float must be exactly 32 bits");
        assert!(error.to_string().contains("32-bit IEEE-754"), "{error}");

        let mut boolean_model = dbc_model();
        let boolean = &mut boolean_model.modules[0].messages[0].signals[0];
        boolean.raw_kind = ValueType::Boolean;
        boolean.signed = false;
        boolean.float = false;
        boolean.width = 2;
        let error = VirtualCan::new(&boolean_model, &CanScenario::default())
            .err()
            .expect("a DBC Boolean must be exactly one bit");
        assert!(error.to_string().contains("exactly 1 bit"), "{error}");
    }

    #[test]
    fn project_method_classification_matches_directional_runtime_routing() {
        let model = dbc_model();
        let rx_message = "Vehicle Network.Status Frame";
        let rx_signal = "Vehicle Network.Status Frame.Signed Count";
        let tx_message = "Vehicle Network.Command Frame";
        let tx_signal = "Vehicle Network.Command Frame.Command";

        for alias_prefix in ["", "DBC."] {
            let rx_message = format!("{alias_prefix}{rx_message}");
            let rx_signal = format!("{alias_prefix}{rx_signal}");
            let tx_message = format!("{alias_prefix}{tx_message}");
            let tx_signal = format!("{alias_prefix}{tx_signal}");

            assert_eq!(
                model_project_receiver(&model, &rx_message),
                Some("DBC.Vehicle Network.Status Frame")
            );
            assert_eq!(
                model_project_receiver(&model, &rx_signal),
                Some("DBC.Vehicle Network.Status Frame.Signed Count")
            );
            assert_eq!(
                model_project_receiver(&model, &tx_message),
                Some("DBC.Vehicle Network.Command Frame")
            );
            assert_eq!(
                model_project_receiver(&model, &tx_signal),
                Some("DBC.Vehicle Network.Command Frame.Command")
            );
            assert!(model_handles_project_call(&model, &rx_message, "Receive"));
            assert!(!model_handles_project_call(&model, &rx_message, "Tx"));
            assert!(model_handles_project_call(&model, &rx_signal, "GetInteger"));
            assert!(!model_handles_project_call(
                &model,
                &rx_signal,
                "SetInteger"
            ));
            assert!(model_handles_project_call(&model, &tx_message, "TxOpen"));
            assert!(!model_handles_project_call(&model, &tx_message, "Receive"));
            assert!(model_handles_project_call(&model, &tx_signal, "SetInteger"));
            assert!(!model_handles_project_call(
                &model,
                &tx_signal,
                "GetInteger"
            ));
        }

        let mut can = VirtualCan::new(&model, &CanScenario::default()).unwrap();
        for (receiver, method, arguments) in [
            (rx_message, "Tx", vec![Value::m1_unsigned(1)]),
            (
                rx_signal,
                "SetInteger",
                vec![Value::m1_unsigned(1), Value::m1_integer(1)],
            ),
            (tx_message, "Receive", vec![]),
            (tx_signal, "GetInteger", vec![]),
        ] {
            let error = project_invoke(&mut can, receiver, method, 99, arguments, 0.0)
                .expect_err("directionally impossible DBC method fails loud");
            assert!(matches!(error, EvalError::UnsupportedBuiltin { .. }));
        }
    }
}
