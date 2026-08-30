// SPDX-License-Identifier: GPL-3.0-or-later
//! Deterministic RS232 byte-buffer model for the `Serial` library.
//!
//! The model implements RS232 operations grounded in the pinned intrinsic
//! catalogue and the M1 Development Manual's examples, plus the explicit
//! evaluator contracts documented for behavior those sources do not settle. LIN
//! and nonzero protocol configurations fail clearly instead of inheriting a
//! generic zero stub. Handles are stable per source call site, nonzero M1 unsigned
//! values, and own independent receive cursors plus separate receive/transmit
//! buffers.

use crate::env::CallSite;
use crate::error::EvalError;
use crate::hardware::HardwareCall;
use crate::scenario::SerialScenario;
use crate::trace::{SerialDirection, SerialEvent};
use crate::value::{M1Scalar, Value};
use std::collections::BTreeMap;

const BUFFER_BYTES: usize = 256;

/// Methods implemented by the deterministic RS232 model.
pub(crate) const ADAPTER_BACKED_METHODS: &[&str] = &[
    "GetFloat",
    "GetHandle",
    "GetInteger",
    "GetTransmitHandle",
    "GetUnsignedInteger",
    "PortDiagnostic",
    "PortInit",
    "Receive",
    "SetFloat",
    "SetInteger",
    "SetString",
    "SetUnsignedInteger",
    "Sum8",
    "Transmit",
    "XOR8",
];

/// Catalogue methods deliberately rejected by the virtual adapter.
pub(crate) const UNSUPPORTED_METHODS: &[&str] = &["GetLinOffset", "LinDump", "SetLinHeader"];

pub(crate) fn is_adapter_backed(method: &str) -> bool {
    ADAPTER_BACKED_METHODS.contains(&method)
}

pub(crate) fn is_explicitly_unsupported(method: &str) -> bool {
    UNSUPPORTED_METHODS.contains(&method)
}

/// Result of one virtual serial call. Only byte transfers carry an event.
#[derive(Debug)]
pub(crate) struct SerialReply {
    pub(crate) value: Value,
    pub(crate) event: Option<SerialEvent>,
    pub(crate) external: bool,
}

impl SerialReply {
    fn value(value: Value) -> Self {
        SerialReply {
            value,
            event: None,
            external: false,
        }
    }

    fn external_value(value: Value) -> Self {
        SerialReply {
            value,
            event: None,
            external: true,
        }
    }

    fn event(value: Value, event: SerialEvent, external: bool) -> Self {
        SerialReply {
            value,
            event: Some(event),
            external,
        }
    }
}

#[derive(Debug, Clone)]
struct Injection {
    time_s: f64,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PortConfig {
    baud: i32,
    protocol: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveBuffer {
    Receive,
    Transmit,
}

#[derive(Debug, Clone)]
struct HandleState {
    big_endian: bool,
    receive: Vec<u8>,
    transmit: Vec<u8>,
    receive_cursors: BTreeMap<i32, usize>,
    active: ActiveBuffer,
}

impl HandleState {
    fn new(big_endian: bool) -> Self {
        HandleState {
            big_endian,
            receive: Vec::new(),
            transmit: vec![0; BUFFER_BYTES],
            receive_cursors: BTreeMap::new(),
            active: ActiveBuffer::Transmit,
        }
    }

    fn active_bytes(&self) -> &[u8] {
        match self.active {
            ActiveBuffer::Receive => &self.receive,
            ActiveBuffer::Transmit => &self.transmit,
        }
    }
}

/// Fresh per-run state for deterministic virtual serial ports.
pub(crate) struct VirtualSerial {
    injections: BTreeMap<i32, Vec<Injection>>,
    ports: BTreeMap<i32, PortConfig>,
    handles: BTreeMap<u32, HandleState>,
    site_handles: BTreeMap<(CallSite, String), u32>,
    next_handle: u32,
}

impl VirtualSerial {
    pub(crate) fn new(scenario: &SerialScenario) -> Result<Self, EvalError> {
        let mut injections: BTreeMap<i32, Vec<Injection>> = BTreeMap::new();
        for (index, entry) in scenario.rx.iter().enumerate() {
            if !entry.time_s.is_finite() || entry.time_s < 0.0 {
                return Err(EvalError::UnsupportedConstruct {
                    kind: format!(
                        "serial rx declaration {} has invalid time_s {} (expected a finite, non-negative time)",
                        index + 1,
                        entry.time_s
                    ),
                    at: 0,
                });
            }
            if entry.port < 0 {
                return Err(EvalError::UnsupportedConstruct {
                    kind: format!(
                        "serial rx declaration {} has negative port {}",
                        index + 1,
                        entry.port
                    ),
                    at: 0,
                });
            }
            if entry.bytes.len() > BUFFER_BYTES {
                return Err(EvalError::UnsupportedConstruct {
                    kind: format!(
                        "serial rx declaration {} has {} bytes, exceeding the {BUFFER_BYTES}-byte receive buffer",
                        index + 1,
                        entry.bytes.len()
                    ),
                    at: 0,
                });
            }
            injections.entry(entry.port).or_default().push(Injection {
                time_s: entry.time_s,
                bytes: entry.bytes.clone(),
            });
        }
        for entries in injections.values_mut() {
            // `SerialScenario` is public and can be built directly, bypassing
            // wire normalization. Stable sorting here keeps that API just as
            // deterministic as TOML/JSON parsing.
            entries.sort_by(|left, right| left.time_s.total_cmp(&right.time_s));
        }
        Ok(VirtualSerial {
            injections,
            ports: BTreeMap::new(),
            handles: BTreeMap::new(),
            site_handles: BTreeMap::new(),
            next_handle: 1,
        })
    }

    pub(crate) fn empty() -> Self {
        VirtualSerial::new(&SerialScenario::default())
            .expect("the empty virtual serial scenario is valid")
    }

    /// Handle one known `Serial` method. Unknown methods return `None` so the
    /// normal fail-loud builtin boundary remains authoritative.
    pub(crate) fn call(
        &mut self,
        method: &str,
        call: &HardwareCall,
    ) -> Result<Option<SerialReply>, EvalError> {
        let reply = match method {
            "GetHandle" | "GetTransmitHandle" => {
                let big_endian = bool_arg(call, 0)?;
                SerialReply::value(Value::m1_unsigned(self.open_handle(
                    call.site.clone(),
                    method,
                    big_endian,
                )?))
            }
            "PortInit" => {
                let port = port_arg(call, 0)?;
                let baud = positive_integer_arg(call, 1, "baud")?;
                if !matches!(
                    baud,
                    1_200 | 1_800 | 2_400 | 4_800 | 9_600 | 19_200 | 38_400 | 57_600 | 115_200
                ) {
                    return Err(serial_error(
                        method,
                        format!(
                            "baud {baud} is unsupported; expected one of 1200, 1800, 2400, 4800, 9600, 19200, 38400, 57600, or 115200"
                        ),
                    ));
                }
                let protocol = integer_arg(call, 2)?;
                if protocol != 0 {
                    return Err(serial_error(
                        method,
                        format!(
                            "protocol {protocol} is unsupported; the virtual adapter implements RS232 protocol 0 only"
                        ),
                    ));
                }
                let requested = PortConfig { baud, protocol };
                if let Some(existing) = self.ports.get(&port) {
                    if *existing != requested {
                        return Err(serial_error(
                            method,
                            format!(
                                "port {port} is already initialized at baud {} with protocol {}; requested baud {baud}, protocol {protocol}",
                                existing.baud, existing.protocol
                            ),
                        ));
                    }
                } else {
                    self.ports.insert(port, requested);
                }
                SerialReply::value(Value::Bool(true))
            }
            "PortDiagnostic" => {
                let port = port_arg(call, 0)?;
                // Both pinned catalogue enums agree that zero means Not in Use
                // and one means OK. Their other diagnostic codes conflict, so
                // the virtual adapter deliberately reports only these two.
                let status = if self.ports.contains_key(&port) { 1 } else { 0 };
                SerialReply::value(Value::m1_integer(status))
            }
            "Receive" => self.receive(call)?,
            "Transmit" => self.transmit(call)?,
            "GetInteger" => {
                let handle = handle_arg(call, 0)?;
                let offset = offset_arg(call, 1)?;
                let length = integer_width(call, 2)?;
                let state = self.handle(method, handle)?;
                let bytes = receive_range(method, state, offset, length)?;
                let raw = decode(bytes, state.big_endian);
                let shift = 32 - length * 8;
                SerialReply::external_value(Value::m1_integer(((raw << shift) as i32) >> shift))
            }
            "GetUnsignedInteger" => {
                let handle = handle_arg(call, 0)?;
                let offset = offset_arg(call, 1)?;
                let length = integer_width(call, 2)?;
                let state = self.handle(method, handle)?;
                let bytes = receive_range(method, state, offset, length)?;
                // The pinned catalogue declares Integer even for this unsigned
                // operation. Preserve all 32 bits in that signed storage family.
                SerialReply::external_value(Value::m1_integer(
                    decode(bytes, state.big_endian) as i32
                ))
            }
            "GetFloat" => {
                let handle = handle_arg(call, 0)?;
                let offset = offset_arg(call, 1)?;
                let state = self.handle(method, handle)?;
                let bytes = receive_range(method, state, offset, 4)?;
                SerialReply::external_value(Value::m1_float(f32::from_bits(decode(
                    bytes,
                    state.big_endian,
                ))))
            }
            "SetInteger" => {
                let handle = handle_arg(call, 0)?;
                let offset = offset_arg(call, 1)?;
                let length = integer_width(call, 2)?;
                let value = integer_arg(call, 3)? as u32;
                let state = self.handle_mut(method, handle)?;
                write_integer(method, state, offset, length, value)?;
                SerialReply::value(Value::m1_integer((offset + length) as i32))
            }
            "SetUnsignedInteger" => {
                let handle = handle_arg(call, 0)?;
                let offset = offset_arg(call, 1)?;
                let length = integer_width(call, 2)?;
                let value = unsigned_bits_arg(call, 3)?;
                let state = self.handle_mut(method, handle)?;
                write_integer(method, state, offset, length, value)?;
                SerialReply::value(Value::m1_integer((offset + length) as i32))
            }
            "SetFloat" => {
                let handle = handle_arg(call, 0)?;
                let offset = offset_arg(call, 1)?;
                let value = float_arg(call, 2)?;
                let state = self.handle_mut(method, handle)?;
                write_integer(method, state, offset, 4, value.to_bits())?;
                SerialReply::value(Value::m1_integer((offset + 4) as i32))
            }
            "SetString" => {
                let handle = handle_arg(call, 0)?;
                let offset = offset_arg(call, 1)?;
                let length = nonnegative_length(call, 2)?;
                let text = string_arg(call, 3)?;
                if !text.is_ascii() {
                    return Err(serial_error(
                        method,
                        "text must be ASCII for the deterministic RS232 byte subset",
                    ));
                }
                if text.len() != length {
                    return Err(serial_error(
                        method,
                        format!(
                            "length {length} does not match the ASCII text length {}",
                            text.len()
                        ),
                    ));
                }
                let state = self.handle_mut(method, handle)?;
                let range = buffer_range(method, offset, length, BUFFER_BYTES)?;
                state.transmit[range].copy_from_slice(text.as_bytes());
                state.active = ActiveBuffer::Transmit;
                SerialReply::value(Value::m1_integer((offset + length) as i32))
            }
            "Sum8" | "XOR8" => {
                let handle = handle_arg(call, 0)?;
                let offset = offset_arg(call, 1)?;
                let length = nonnegative_length(call, 2)?;
                let state = self.handle(method, handle)?;
                let active = state.active_bytes();
                let range = buffer_range(method, offset, length, active.len())?;
                let result = if method == "Sum8" {
                    active[range]
                        .iter()
                        .fold(0_u8, |sum, byte| sum.wrapping_add(*byte))
                } else {
                    active[range].iter().fold(0_u8, |xor, byte| xor ^ byte)
                };
                let value = Value::m1_integer(i32::from(result));
                if state.active == ActiveBuffer::Receive {
                    SerialReply::external_value(value)
                } else {
                    SerialReply::value(value)
                }
            }
            method if is_explicitly_unsupported(method) => {
                return Err(EvalError::UnsupportedBuiltin {
                    object: call.source_receiver.clone(),
                    method: method.to_string(),
                });
            }
            _ => return Ok(None),
        };
        Ok(Some(reply))
    }

    fn open_handle(
        &mut self,
        site: CallSite,
        method: &str,
        big_endian: bool,
    ) -> Result<u32, EvalError> {
        let key = (site, method.to_string());
        if let Some(handle) = self.site_handles.get(&key).copied() {
            let state = self
                .handles
                .get(&handle)
                .expect("site handle always has handle state");
            if state.big_endian != big_endian {
                return Err(serial_error(
                    method,
                    format!(
                        "call site already opened handle {handle} with bigendian={}, then requested bigendian={big_endian}",
                        state.big_endian
                    ),
                ));
            }
            return Ok(handle);
        }
        let handle = self.next_handle;
        self.next_handle = self.next_handle.checked_add(1).ok_or_else(|| {
            serial_error(method, "exhausted the nonzero 32-bit serial handle space")
        })?;
        self.site_handles.insert(key, handle);
        self.handles.insert(handle, HandleState::new(big_endian));
        Ok(handle)
    }

    fn handle(&self, method: &str, handle: u32) -> Result<&HandleState, EvalError> {
        self.handles.get(&handle).ok_or_else(|| {
            serial_error(
                method,
                format!("invalid handle {handle}; handles are nonzero and owned by this run"),
            )
        })
    }

    fn handle_mut(&mut self, method: &str, handle: u32) -> Result<&mut HandleState, EvalError> {
        self.handles.get_mut(&handle).ok_or_else(|| {
            serial_error(
                method,
                format!("invalid handle {handle}; handles are nonzero and owned by this run"),
            )
        })
    }

    fn receive(&mut self, call: &HardwareCall) -> Result<SerialReply, EvalError> {
        let method = "Receive";
        let handle = handle_arg(call, 0)?;
        let port = port_arg(call, 1)?;
        self.require_initialized(method, port)?;
        let cursor = self
            .handle(method, handle)?
            .receive_cursors
            .get(&port)
            .copied()
            .unwrap_or(0);
        let injections = self.injections.get(&port).map(Vec::as_slice).unwrap_or(&[]);
        let end = injections[cursor..]
            .iter()
            .take_while(|entry| entry.time_s <= call.time.elapsed_s)
            .count()
            + cursor;
        let bytes = injections[cursor..end]
            .iter()
            .flat_map(|entry| entry.bytes.iter().copied())
            .collect::<Vec<_>>();
        if bytes.len() > BUFFER_BYTES {
            return Err(serial_error(
                method,
                format!(
                    "{len} pending bytes on port {port} exceed the {BUFFER_BYTES}-byte receive buffer for handle {handle}",
                    len = bytes.len()
                ),
            ));
        }

        let state = self.handle_mut(method, handle)?;
        state.receive.clear();
        state.receive.extend_from_slice(&bytes);
        state.receive_cursors.insert(port, end);
        state.active = ActiveBuffer::Receive;
        if bytes.is_empty() {
            return Ok(SerialReply::external_value(Value::Bool(false)));
        }
        Ok(SerialReply::event(
            Value::Bool(true),
            SerialEvent {
                direction: SerialDirection::Rx,
                time: call.time,
                port,
                handle,
                bytes,
                site: call.site.clone(),
            },
            true,
        ))
    }

    fn transmit(&mut self, call: &HardwareCall) -> Result<SerialReply, EvalError> {
        let method = "Transmit";
        let handle = handle_arg(call, 0)?;
        let port = port_arg(call, 1)?;
        let length = nonnegative_length(call, 2)?;
        self.require_initialized(method, port)?;
        let state = self.handle(method, handle)?;
        let range = buffer_range(method, 0, length, state.transmit.len())?;
        Ok(SerialReply::event(
            Value::Bool(true),
            SerialEvent {
                direction: SerialDirection::Tx,
                time: call.time,
                port,
                handle,
                bytes: state.transmit[range].to_vec(),
                site: call.site.clone(),
            },
            false,
        ))
    }

    fn require_initialized(&self, method: &str, port: i32) -> Result<(), EvalError> {
        if self.ports.contains_key(&port) {
            Ok(())
        } else {
            Err(serial_error(
                method,
                format!("port {port} is not initialized; call Serial.PortInit first"),
            ))
        }
    }
}

fn receive_range<'a>(
    method: &str,
    state: &'a HandleState,
    offset: usize,
    length: usize,
) -> Result<&'a [u8], EvalError> {
    let range = buffer_range(method, offset, length, state.receive.len())?;
    Ok(&state.receive[range])
}

fn write_integer(
    method: &str,
    state: &mut HandleState,
    offset: usize,
    length: usize,
    value: u32,
) -> Result<(), EvalError> {
    let range = buffer_range(method, offset, length, state.transmit.len())?;
    let bytes = if state.big_endian {
        value.to_be_bytes()[4 - length..].to_vec()
    } else {
        value.to_le_bytes()[..length].to_vec()
    };
    state.transmit[range].copy_from_slice(&bytes);
    state.active = ActiveBuffer::Transmit;
    Ok(())
}

fn decode(bytes: &[u8], big_endian: bool) -> u32 {
    if big_endian {
        bytes
            .iter()
            .fold(0_u32, |value, byte| (value << 8) | u32::from(*byte))
    } else {
        bytes
            .iter()
            .enumerate()
            .fold(0_u32, |value, (shift, byte)| {
                value | (u32::from(*byte) << (shift * 8))
            })
    }
}

fn buffer_range(
    method: &str,
    offset: usize,
    length: usize,
    available: usize,
) -> Result<std::ops::Range<usize>, EvalError> {
    let end = offset.checked_add(length).ok_or_else(|| {
        serial_error(
            method,
            format!("offset {offset} plus length {length} overflows the buffer index"),
        )
    })?;
    if end > available {
        return Err(serial_error(
            method,
            format!("byte range {offset}..{end} exceeds the available buffer length {available}"),
        ));
    }
    Ok(offset..end)
}

fn handle_arg(call: &HardwareCall, index: usize) -> Result<u32, EvalError> {
    match call.arguments.get(index) {
        Some(Value::M1(M1Scalar::UnsignedInteger(handle))) => Ok(*handle),
        Some(value) => Err(serial_error(
            &call.method,
            format!("argument {} must be an M1 Handle, got {value:?}", index + 1),
        )),
        None => Err(missing_arg(call, index)),
    }
}

fn integer_arg(call: &HardwareCall, index: usize) -> Result<i32, EvalError> {
    match call.arguments.get(index) {
        Some(Value::M1(M1Scalar::Integer(value))) => Ok(*value),
        Some(value) => Err(serial_error(
            &call.method,
            format!(
                "argument {} must be an M1 Integer, got {value:?}",
                index + 1
            ),
        )),
        None => Err(missing_arg(call, index)),
    }
}

/// The catalogue declares `SetUnsignedInteger.value` as Integer, while its name
/// and the official hexadecimal example also admit unsigned 32-bit values.
/// Preserve either M1 integer family's bits at this one boundary.
fn unsigned_bits_arg(call: &HardwareCall, index: usize) -> Result<u32, EvalError> {
    match call.arguments.get(index) {
        Some(Value::M1(M1Scalar::Integer(value))) => Ok(*value as u32),
        Some(Value::M1(M1Scalar::UnsignedInteger(value))) => Ok(*value),
        Some(value) => Err(serial_error(
            &call.method,
            format!(
                "argument {} must be an M1 Integer or UnsignedInteger, got {value:?}",
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
        Err(serial_error(
            &call.method,
            format!("{name} must be positive, got {value}"),
        ))
    }
}

fn port_arg(call: &HardwareCall, index: usize) -> Result<i32, EvalError> {
    let port = integer_arg(call, index)?;
    if port >= 0 {
        Ok(port)
    } else {
        Err(serial_error(
            &call.method,
            format!("port must be non-negative, got {port}"),
        ))
    }
}

fn offset_arg(call: &HardwareCall, index: usize) -> Result<usize, EvalError> {
    let offset = integer_arg(call, index)?;
    usize::try_from(offset).map_err(|_| {
        serial_error(
            &call.method,
            format!("offset must be non-negative, got {offset}"),
        )
    })
}

fn nonnegative_length(call: &HardwareCall, index: usize) -> Result<usize, EvalError> {
    let length = integer_arg(call, index)?;
    usize::try_from(length).map_err(|_| {
        serial_error(
            &call.method,
            format!("length must be non-negative, got {length}"),
        )
    })
}

fn integer_width(call: &HardwareCall, index: usize) -> Result<usize, EvalError> {
    let length = nonnegative_length(call, index)?;
    if (1..=4).contains(&length) {
        Ok(length)
    } else {
        Err(serial_error(
            &call.method,
            format!("integer length must be in 1..=4 bytes, got {length}"),
        ))
    }
}

fn bool_arg(call: &HardwareCall, index: usize) -> Result<bool, EvalError> {
    match call.arguments.get(index) {
        Some(Value::Bool(value)) => Ok(*value),
        Some(value) => Err(serial_error(
            &call.method,
            format!("argument {} must be Boolean, got {value:?}", index + 1),
        )),
        None => Err(missing_arg(call, index)),
    }
}

fn float_arg(call: &HardwareCall, index: usize) -> Result<f32, EvalError> {
    match call.arguments.get(index) {
        Some(Value::M1(M1Scalar::FloatingPoint(value))) => Ok(*value),
        Some(value) => Err(serial_error(
            &call.method,
            format!(
                "argument {} must be M1 FloatingPoint, got {value:?}",
                index + 1
            ),
        )),
        None => Err(missing_arg(call, index)),
    }
}

fn string_arg(call: &HardwareCall, index: usize) -> Result<&str, EvalError> {
    match call.arguments.get(index) {
        Some(Value::Str(value)) => Ok(value),
        Some(value) => Err(serial_error(
            &call.method,
            format!("argument {} must be String, got {value:?}", index + 1),
        )),
        None => Err(missing_arg(call, index)),
    }
}

fn missing_arg(call: &HardwareCall, index: usize) -> EvalError {
    serial_error(&call.method, format!("missing argument {}", index + 1))
}

fn serial_error(method: &str, detail: impl Into<String>) -> EvalError {
    EvalError::BadCall {
        detail: format!("Serial.{method}: {}", detail.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{EvalTime, ResolvedReceiver};
    use crate::scenario::SerialRx;

    fn call(method: &str, site: usize, arguments: Vec<Value>, t: f64) -> HardwareCall {
        HardwareCall {
            receiver: ResolvedReceiver::Library {
                object: "Serial".to_string(),
            },
            source_receiver: "Serial".to_string(),
            method: method.to_string(),
            site: CallSite::new("Serial.Test.m1scr", site),
            arguments,
            time: EvalTime::periodic((t * 100.0) as u64, t, 0.01, 0.01),
        }
    }

    fn invoke(
        serial: &mut VirtualSerial,
        method: &str,
        site: usize,
        arguments: Vec<Value>,
        t: f64,
    ) -> Result<SerialReply, EvalError> {
        serial
            .call(method, &call(method, site, arguments, t))?
            .ok_or_else(|| serial_error(method, "test method was not handled"))
    }

    fn open(serial: &mut VirtualSerial, site: usize, big_endian: bool) -> u32 {
        let reply = invoke(
            serial,
            "GetHandle",
            site,
            vec![Value::Bool(big_endian)],
            0.0,
        )
        .expect("handle opens");
        match reply.value {
            Value::M1(M1Scalar::UnsignedInteger(handle)) => handle,
            other => panic!("unexpected handle {other:?}"),
        }
    }

    fn init(serial: &mut VirtualSerial, port: i32) {
        invoke(
            serial,
            "PortInit",
            90,
            vec![
                Value::m1_integer(port),
                Value::m1_integer(115_200),
                Value::m1_integer(0),
            ],
            0.0,
        )
        .expect("port initializes");
    }

    #[test]
    fn handles_are_nonzero_stable_and_independent_per_site() {
        let mut serial = VirtualSerial::empty();
        let first = open(&mut serial, 10, true);
        let repeated = open(&mut serial, 10, true);
        let second = open(&mut serial, 20, true);
        assert_ne!(first, 0);
        assert_eq!(first, repeated);
        assert_ne!(first, second);

        let mut fresh = VirtualSerial::empty();
        assert_eq!(open(&mut fresh, 10, true), first);
        assert_eq!(open(&mut fresh, 20, true), second);
    }

    #[test]
    fn receivers_on_one_port_have_independent_cursors_and_flushes() {
        let scenario = SerialScenario {
            rx: vec![
                SerialRx {
                    time_s: 0.0,
                    port: 0,
                    bytes: vec![1, 2],
                },
                SerialRx {
                    time_s: 0.1,
                    port: 0,
                    bytes: vec![3],
                },
            ],
        };
        let mut serial = VirtualSerial::new(&scenario).expect("valid serial scenario");
        init(&mut serial, 0);
        let a = open(&mut serial, 10, true);
        let b = open(&mut serial, 20, true);
        for handle in [a, b] {
            let reply = invoke(
                &mut serial,
                "Receive",
                30 + handle as usize,
                vec![Value::m1_unsigned(handle), Value::m1_integer(0)],
                0.0,
            )
            .expect("each handle independently receives the first chunk");
            assert_eq!(reply.value, Value::Bool(true));
            assert_eq!(reply.event.expect("rx event").bytes, vec![1, 2]);
        }
        let empty = invoke(
            &mut serial,
            "Receive",
            40,
            vec![Value::m1_unsigned(a), Value::m1_integer(0)],
            0.05,
        )
        .expect("empty receive succeeds");
        assert_eq!(empty.value, Value::Bool(false));
        let later = invoke(
            &mut serial,
            "Receive",
            40,
            vec![Value::m1_unsigned(a), Value::m1_integer(0)],
            0.1,
        )
        .expect("later chunk arrives");
        assert_eq!(later.event.expect("rx event").bytes, vec![3]);
    }

    #[test]
    fn endian_integer_float_and_string_writes_produce_wire_bytes() {
        for (big_endian, expected) in [
            (
                true,
                vec![0x30, 0x35, 0x3f, 0xc0, 0, 0, b'O', b'K', 0xff, 0xfe],
            ),
            (
                false,
                vec![0x35, 0x30, 0, 0, 0xc0, 0x3f, b'O', b'K', 0xfe, 0xff],
            ),
        ] {
            let mut serial = VirtualSerial::empty();
            init(&mut serial, 0);
            let handle = open(&mut serial, 10, big_endian);
            invoke(
                &mut serial,
                "SetUnsignedInteger",
                20,
                vec![
                    Value::m1_unsigned(handle),
                    Value::m1_integer(0),
                    Value::m1_integer(2),
                    Value::m1_integer(0x3035),
                ],
                0.0,
            )
            .expect("integer writes");
            invoke(
                &mut serial,
                "SetFloat",
                30,
                vec![
                    Value::m1_unsigned(handle),
                    Value::m1_integer(2),
                    Value::m1_float(1.5),
                ],
                0.0,
            )
            .expect("float writes");
            invoke(
                &mut serial,
                "SetString",
                40,
                vec![
                    Value::m1_unsigned(handle),
                    Value::m1_integer(6),
                    Value::m1_integer(2),
                    Value::Str("OK".to_string()),
                ],
                0.0,
            )
            .expect("string writes");
            invoke(
                &mut serial,
                "SetInteger",
                45,
                vec![
                    Value::m1_unsigned(handle),
                    Value::m1_integer(8),
                    Value::m1_integer(2),
                    Value::m1_integer(-2),
                ],
                0.0,
            )
            .expect("signed integer writes");
            let tx = invoke(
                &mut serial,
                "Transmit",
                50,
                vec![
                    Value::m1_unsigned(handle),
                    Value::m1_integer(0),
                    Value::m1_integer(10),
                ],
                0.25,
            )
            .expect("transmit succeeds")
            .event
            .expect("tx event");
            assert_eq!(tx.bytes, expected);
            assert_eq!(tx.time.elapsed_s, 0.25);
            assert_eq!(tx.site.offset(), 50);
        }
    }

    #[test]
    fn checksums_follow_the_explicit_active_buffer_contract_and_provenance() {
        let scenario = SerialScenario {
            rx: vec![SerialRx {
                time_s: 0.0,
                port: 0,
                bytes: vec![1, 2, 3],
            }],
        };
        let mut serial = VirtualSerial::new(&scenario).expect("valid serial scenario");
        init(&mut serial, 0);
        let handle = open(&mut serial, 10, true);

        invoke(
            &mut serial,
            "SetUnsignedInteger",
            20,
            vec![
                Value::m1_unsigned(handle),
                Value::m1_integer(0),
                Value::m1_integer(3),
                Value::m1_integer(0x00fa_0a03),
            ],
            0.0,
        )
        .expect("TX bytes are written");
        let tx_sum = invoke(
            &mut serial,
            "Sum8",
            30,
            vec![
                Value::m1_unsigned(handle),
                Value::m1_integer(0),
                Value::m1_integer(3),
            ],
            0.0,
        )
        .expect("TX sum succeeds");
        let tx_xor = invoke(
            &mut serial,
            "XOR8",
            31,
            vec![
                Value::m1_unsigned(handle),
                Value::m1_integer(0),
                Value::m1_integer(3),
            ],
            0.0,
        )
        .expect("TX xor succeeds");
        assert_eq!(tx_sum.value, Value::m1_integer(7));
        assert_eq!(tx_xor.value, Value::m1_integer(0xf3));
        assert!(!tx_sum.external);
        assert!(!tx_xor.external);

        invoke(
            &mut serial,
            "Receive",
            40,
            vec![Value::m1_unsigned(handle), Value::m1_integer(0)],
            0.0,
        )
        .expect("RX bytes are delivered");
        for (method, expected) in [("Sum8", 6), ("XOR8", 0)] {
            let reply = invoke(
                &mut serial,
                method,
                50,
                vec![
                    Value::m1_unsigned(handle),
                    Value::m1_integer(0),
                    Value::m1_integer(3),
                ],
                0.0,
            )
            .expect("RX checksum succeeds");
            assert_eq!(reply.value, Value::m1_integer(expected));
            assert!(reply.external, "RX-derived checksum must be external");
        }
    }

    #[test]
    fn endian_signed_unsigned_and_float_reads_use_receive_buffer() {
        for (big_endian, bytes) in [
            (
                true,
                vec![0xff, 0xfe, 0x89, 0xab, 0xcd, 0xef, 0x3f, 0xc0, 0, 0],
            ),
            (
                false,
                vec![0xfe, 0xff, 0xef, 0xcd, 0xab, 0x89, 0, 0, 0xc0, 0x3f],
            ),
        ] {
            let scenario = SerialScenario {
                rx: vec![SerialRx {
                    time_s: 0.0,
                    port: 0,
                    bytes,
                }],
            };
            let mut serial = VirtualSerial::new(&scenario).expect("valid serial scenario");
            init(&mut serial, 0);
            let handle = open(&mut serial, 10, big_endian);
            invoke(
                &mut serial,
                "Receive",
                20,
                vec![Value::m1_unsigned(handle), Value::m1_integer(0)],
                0.0,
            )
            .expect("receive succeeds");
            assert_eq!(
                invoke(
                    &mut serial,
                    "GetInteger",
                    30,
                    vec![
                        Value::m1_unsigned(handle),
                        Value::m1_integer(0),
                        Value::m1_integer(2),
                    ],
                    0.0,
                )
                .expect("signed read")
                .value,
                Value::m1_integer(-2)
            );
            assert_eq!(
                invoke(
                    &mut serial,
                    "GetUnsignedInteger",
                    40,
                    vec![
                        Value::m1_unsigned(handle),
                        Value::m1_integer(2),
                        Value::m1_integer(4),
                    ],
                    0.0,
                )
                .expect("unsigned read")
                .value,
                Value::m1_integer(0x89ab_cdef_u32 as i32)
            );
            assert_eq!(
                invoke(
                    &mut serial,
                    "GetFloat",
                    50,
                    vec![Value::m1_unsigned(handle), Value::m1_integer(6)],
                    0.0,
                )
                .expect("float read")
                .value,
                Value::m1_float(1.5)
            );
        }
    }

    #[test]
    fn status_and_invalid_configuration_fail_or_report_precisely() {
        let mut serial = VirtualSerial::empty();
        assert_eq!(
            invoke(
                &mut serial,
                "PortDiagnostic",
                1,
                vec![Value::m1_integer(2)],
                0.0,
            )
            .expect("uninitialized diagnostic is a status")
            .value,
            Value::m1_integer(0)
        );
        init(&mut serial, 2);
        assert_eq!(
            invoke(
                &mut serial,
                "PortDiagnostic",
                1,
                vec![Value::m1_integer(2)],
                0.0,
            )
            .expect("initialized port reports OK")
            .value,
            Value::m1_integer(1)
        );
        let unsupported = invoke(
            &mut serial,
            "PortInit",
            2,
            vec![
                Value::m1_integer(2),
                Value::m1_integer(19_200),
                Value::m1_integer(1),
            ],
            0.0,
        )
        .expect_err("LIN config fails");
        assert!(unsupported.to_string().contains("RS232 protocol 0 only"));

        let invalid = invoke(
            &mut serial,
            "GetInteger",
            3,
            vec![
                Value::m1_unsigned(99),
                Value::m1_integer(0),
                Value::m1_integer(1),
            ],
            0.0,
        )
        .expect_err("unknown handle fails");
        assert!(invalid.to_string().contains("invalid handle 99"));
    }

    #[test]
    fn buffer_boundaries_lengths_and_receive_overflow_fail_precisely() {
        let scenario = SerialScenario {
            rx: vec![SerialRx {
                time_s: 0.0,
                port: 0,
                bytes: (0_u16..=255).map(|byte| byte as u8).collect(),
            }],
        };
        let mut serial = VirtualSerial::new(&scenario).expect("valid serial scenario");
        init(&mut serial, 0);
        let handle = open(&mut serial, 10, true);
        invoke(
            &mut serial,
            "Receive",
            20,
            vec![Value::m1_unsigned(handle), Value::m1_integer(0)],
            0.0,
        )
        .expect("full receive buffer fits");
        let boundary = invoke(
            &mut serial,
            "GetFloat",
            30,
            vec![Value::m1_unsigned(handle), Value::m1_integer(252)],
            0.0,
        )
        .expect("four-byte read at 252 fits");
        assert_eq!(boundary.value, Value::m1_float(f32::from_bits(0xfcfd_feff)));

        let too_far = invoke(
            &mut serial,
            "GetFloat",
            31,
            vec![Value::m1_unsigned(handle), Value::m1_integer(253)],
            0.0,
        )
        .expect_err("four-byte read at 253 exceeds 256 bytes");
        assert!(too_far.to_string().contains("253..257"));

        for length in [0, 5] {
            let error = invoke(
                &mut serial,
                "GetInteger",
                32,
                vec![
                    Value::m1_unsigned(handle),
                    Value::m1_integer(0),
                    Value::m1_integer(length),
                ],
                0.0,
            )
            .expect_err("invalid integer width fails");
            assert!(error.to_string().contains("1..=4 bytes"));
        }
        let negative = invoke(
            &mut serial,
            "GetInteger",
            33,
            vec![
                Value::m1_unsigned(handle),
                Value::m1_integer(-1),
                Value::m1_integer(1),
            ],
            0.0,
        )
        .expect_err("negative offset fails");
        assert!(negative.to_string().contains("offset must be non-negative"));

        invoke(
            &mut serial,
            "SetInteger",
            34,
            vec![
                Value::m1_unsigned(handle),
                Value::m1_integer(255),
                Value::m1_integer(1),
                Value::m1_integer(-1),
            ],
            0.0,
        )
        .expect("last TX byte is writable");
        let write_oob = invoke(
            &mut serial,
            "SetInteger",
            35,
            vec![
                Value::m1_unsigned(handle),
                Value::m1_integer(255),
                Value::m1_integer(2),
                Value::m1_integer(1),
            ],
            0.0,
        )
        .expect_err("TX write beyond 256 bytes fails");
        assert!(write_oob.to_string().contains("255..257"));

        let overflow = SerialScenario {
            rx: vec![
                SerialRx {
                    time_s: 0.0,
                    port: 0,
                    bytes: vec![1; 200],
                },
                SerialRx {
                    time_s: 0.0,
                    port: 0,
                    bytes: vec![2; 100],
                },
            ],
        };
        let mut serial = VirtualSerial::new(&overflow).expect("each chunk fits by itself");
        init(&mut serial, 0);
        let handle = open(&mut serial, 10, true);
        let error = invoke(
            &mut serial,
            "Receive",
            20,
            vec![Value::m1_unsigned(handle), Value::m1_integer(0)],
            0.0,
        )
        .expect_err("pending bytes beyond buffer capacity fail");
        assert!(error.to_string().contains("300 pending bytes"));
    }

    #[test]
    fn ports_are_isolated_and_direct_scenarios_are_sorted() {
        let scenario = SerialScenario {
            rx: vec![
                SerialRx {
                    time_s: 0.1,
                    port: 0,
                    bytes: vec![3],
                },
                SerialRx {
                    time_s: 0.0,
                    port: 1,
                    bytes: vec![9],
                },
                SerialRx {
                    time_s: 0.0,
                    port: 0,
                    bytes: vec![1],
                },
                SerialRx {
                    time_s: 0.1,
                    port: 0,
                    bytes: vec![4],
                },
            ],
        };
        let mut serial = VirtualSerial::new(&scenario).expect("valid direct serial scenario");
        init(&mut serial, 0);
        init(&mut serial, 1);
        let handle = open(&mut serial, 10, true);
        let port_one = invoke(
            &mut serial,
            "Receive",
            20,
            vec![Value::m1_unsigned(handle), Value::m1_integer(1)],
            0.0,
        )
        .expect("port one receives")
        .event
        .expect("port one event");
        assert_eq!(port_one.bytes, vec![9]);
        let port_zero = invoke(
            &mut serial,
            "Receive",
            21,
            vec![Value::m1_unsigned(handle), Value::m1_integer(0)],
            0.1,
        )
        .expect("port zero receives independently")
        .event
        .expect("port zero event");
        assert_eq!(port_zero.bytes, vec![1, 3, 4]);
    }

    #[test]
    fn port_init_is_idempotent_and_rejects_unknown_or_conflicting_config() {
        let mut serial = VirtualSerial::empty();
        init(&mut serial, 0);
        init(&mut serial, 0);

        let conflicting = invoke(
            &mut serial,
            "PortInit",
            90,
            vec![
                Value::m1_integer(0),
                Value::m1_integer(9_600),
                Value::m1_integer(0),
            ],
            0.0,
        )
        .expect_err("conflicting reconfiguration fails");
        assert!(conflicting.to_string().contains("already initialized"));

        let unknown_baud = invoke(
            &mut serial,
            "PortInit",
            91,
            vec![
                Value::m1_integer(1),
                Value::m1_integer(12_345),
                Value::m1_integer(0),
            ],
            0.0,
        )
        .expect_err("unknown baud fails");
        assert!(
            unknown_baud
                .to_string()
                .contains("baud 12345 is unsupported")
        );

        let handle = open(&mut serial, 12, true);
        let uninitialized = invoke(
            &mut serial,
            "Transmit",
            92,
            vec![
                Value::m1_unsigned(handle),
                Value::m1_integer(2),
                Value::m1_integer(0),
            ],
            0.0,
        )
        .expect_err("uninitialized port fails");
        assert!(
            uninitialized
                .to_string()
                .contains("port 2 is not initialized")
        );
    }

    #[test]
    fn tx_events_are_snapshots_and_lin_methods_are_unsupported() {
        let mut serial = VirtualSerial::empty();
        init(&mut serial, 0);
        let handle = open(&mut serial, 10, true);
        invoke(
            &mut serial,
            "SetUnsignedInteger",
            20,
            vec![
                Value::m1_unsigned(handle),
                Value::m1_integer(0),
                Value::m1_integer(1),
                Value::m1_integer(1),
            ],
            0.0,
        )
        .expect("first write");
        let first = invoke(
            &mut serial,
            "Transmit",
            30,
            vec![
                Value::m1_unsigned(handle),
                Value::m1_integer(0),
                Value::m1_integer(1),
            ],
            0.0,
        )
        .expect("first transmit")
        .event
        .expect("first tx event");
        invoke(
            &mut serial,
            "SetUnsignedInteger",
            20,
            vec![
                Value::m1_unsigned(handle),
                Value::m1_integer(0),
                Value::m1_integer(1),
                Value::m1_integer(2),
            ],
            0.1,
        )
        .expect("later write");
        assert_eq!(first.bytes, vec![1], "event retained its byte snapshot");

        for method in UNSUPPORTED_METHODS {
            let arguments = match *method {
                "GetLinOffset" => vec![Value::m1_unsigned(handle), Value::m1_integer(1)],
                "LinDump" => vec![Value::m1_unsigned(handle)],
                "SetLinHeader" => vec![
                    Value::m1_unsigned(handle),
                    Value::m1_integer(0),
                    Value::m1_integer(1),
                    Value::m1_integer(1),
                    Value::Bool(false),
                    Value::Bool(false),
                ],
                _ => unreachable!(),
            };
            assert!(matches!(
                invoke(&mut serial, method, 40, arguments, 0.0),
                Err(EvalError::UnsupportedBuiltin { object, method: rejected })
                    if object == "Serial" && rejected == *method
            ));
        }
    }
}
