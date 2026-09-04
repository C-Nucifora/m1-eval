# Virtual CAN adapter

Virtual CAN is a deterministic, run-owned classic-CAN model for supported
`CanComms.*`, `J1939.*`, and loaded M1 DBC objects. It does not open a host CAN
device, model arbitration, reproduce ECU electrical timing, or send live
traffic.

## Scenario input

Declare one-shot receive frames in TOML with `[[can.rx]]`:

```toml
[[can.rx]]
time_s = 0.125
bus = 0
id = 0x123
extended = false
bytes = [0x12, 0x34]
```

JSON accepts the same shape under `{"can":{"rx":[...]}}`. `bus` is restricted
to the captured catalogue range `0..=2`. Standard identifiers must fit 11 bits,
extended identifiers must fit 29 bits, and payloads contain zero through eight
wire-order bytes. Declarations are sorted by time while equal-time source order
is retained.

Each raw receive handle has an independent cursor. A due frame is consumed only
when `RxMessage(handle)` runs. A DBC RX message instead consumes through
`Message.Receive()` and exposes that message's current frame to all child signal
getters. Scenario arrival time is converted to the first observable base tick;
an off-grid arrival rounds up. `GetTicks` reports that arrival tick, not the later
polling tick.

## Lifecycle and state ownership

Every `Engine` run creates new bus configuration, handles, receive cursors,
current frames, and transmit buffers. Handles are stable, nonzero `u32` tokens
for a call site within one run. They are invalid in another run and cannot cross
raw/DBC or message ownership boundaries.

`CanComms.Init(bus, kbaud)` initializes a raw bus. A DBC module uses
`Module.Init(bus)`. Repeating the same configuration is valid. Conflicting
bitrates, a DBC module initialized on two buses, and a script bus that disagrees
with the loaded binding fail with the exact bus and prior configuration.

Whole-project mode shares one adapter between startup and periodic functions.
Function and cone modes do not run startup, so their selected call chain must
initialize the bus or module. Counterfactual replay starts with fresh CAN state,
has no startup pass, and receives no scenario CAN frames. CAN calls inside an
override expression still produce ordered trace events.

## Supported raw CanComms calls

The virtual route implements:

- bus and handles: `Init`, `RxOpenStandard`, `RxOpenExtended`, `RxMessage`,
  `TxOpen`, and `TxInitialise`;
- frame operations: `TxStandard`, `TxExtended`, `GetID`, `GetLength`,
  `GetTicks`, and `XOR8`;
- fields: `GetBit`/`SetBit`, `GetInteger`/`SetInteger`,
  `GetUnsignedInteger`/`SetUnsignedInteger`, `GetFloat`/`SetFloat`, and
  `GetFixed7DP`/`SetFixed7DP`.

`TxInitialise(handle, 0)` is a valid initialized zero-length frame. Transmitting
before initialization is a distinct error. `GetLength` reports the current RX
frame length or an initialized TX buffer length; an uninitialized TX buffer has
no specified length and fails loud. Integer fields use widths `1..=32`.
Float and fixed fields use exactly 32 bits. Raw float operations preserve every
IEEE-754 binary32 bit pattern, including signed zero, infinity, and NaN payloads.
Fixed operations require an exact `FixedPoint7dps` value and preserve its raw
signed `i32` bits.

M1 allows signed and unsigned 32-bit storage at integral call boundaries. The
adapter applies that bit-preserving normalization before range checks.
`SetUnsignedInteger` accepts all 32 bits at width 32 and rejects an out-of-range
value at narrower widths. Floating parameters accept M1 FloatingPoint, Integer,
or UnsignedInteger widening. They do not implicitly convert FixedPoint7dps.

Known CanComms methods outside this list remain available to a scenario value or
external adapter first. If neither owns the call, they fail loud rather than
using the old generic zero stub.

## J1939 over virtual CAN

The `J1939` library uses the same buses, timed scenario frames, handle allocator,
and ordered CAN trace as `CanComms` and DBC objects. `Open` validates the SAE
J1939 NAME field widths, establishes the node address, and initializes its bus.
`Address` and `State` then report the run-owned node state.

Receive parameter groups match an 18-bit PGN and either one source address or
the `-1` wildcard. `RxRegister` binds the group to a node. `RxTicks`,
`GetLength`, and `GetUnsignedInteger` consume due extended frames on that node's
bus and expose their arrival tick, payload length, and little-endian fields.
PDU1 PGNs require a zero destination byte; the destination comes from the
29-bit identifier. PDU2 PGNs retain their group-extension byte.

Transmit parameter groups validate priority, PGN, and length, then bind to a
node through `TxRegister`. `TxClear`, `TxSetUnsignedInteger`, and `TxTransmit`
own one zero-initialized, little-endian payload buffer. `Request` emits the
standard request PGN (`0x0EA00`) with the requested PGN in three wire-order
bytes. This classic-CAN model deliberately restricts parameter groups to one
`1..=8` byte frame; J1939 transport-protocol segmentation is not inferred.

`DTC`, `DTCRegister`, `DTCSetActive`, `DTCSetInactive`, `DTCActive`, and
`DTCCount` provide deterministic run-local diagnostic-handle state. SPN, FMI,
lamp, address, PGN, priority, field, registration, and ownership errors fail
with the exact offending value or handle. Coverage reports every captured
`J1939` method as adapter-backed because it is handled by this virtual adapter.

## Raw bit addressing

CanComms uses a fixed 64-bit internal buffer with MSB-first bit numbering. For
field bit `k`, where `0 <= k < width`:

```text
internal_bit  = bitoff + k
internal_byte = internal_bit / 8
wire_byte     = bigendian ? internal_byte : 7 - internal_byte
wire_mask     = 0x80 >> (internal_bit % 8)
numeric_bit   = width - 1 - k
```

Big-endian fields therefore use the low canonical offsets. Little-endian fields
reverse the eight bytes, so a two-byte little-endian payload occupies canonical
bits `48..64`, not `0..16`. Bits within each byte remain MSB-first. Arbitrary
sub-byte big-endian fields are supported, and writes preserve neighboring bits.

Examples:

| Operation | Wire bytes |
| --- | --- |
| big `(offset=0, width=16, value=0x1234)` | `12 34` |
| little DLC 2 `(offset=48, width=16, value=0x1234)` | `34 12` |
| big `(offset=4, width=12, value=0xABC)` | `0A BC` |
| little `(offset=32, width=16, value=0x1234)` | `00 00 34 12 00 00 00 00` |

This is separate from DBC Intel/Motorola start-bit traversal.

## Loaded DBC objects

The loader reads every nested `.m1dbc` once and passes those caller-owned bytes,
the loaded `Project`, and the already parsed scripts to m1-can's runtime-model
API. The resulting model preserves the exact relative source path, module,
message, signal, aliases, ID/format/DLC/direction, endian, raw type, bit layout,
scale, offset, and script-derived bus binding. Duplicate, ambiguous, mismatched,
and out-of-range identities fail during load.

The supported DBC call shapes are:

- module: `Init(bus)`;
- receive message: `Receive()`;
- transmit message: `TxOpen()`, `TxInitialise(handle)`, and `Tx(handle)`;
- receive signal: `GetBit`, `GetInteger`, `GetUnsignedInteger`, `GetFloat`, and
  `GetScaled`;
- transmit signal: the matching `SetBit`, `SetInteger`,
  `SetUnsignedInteger`, `SetFloat`, and `SetScaled`, each with
  `(message_handle, value)`.

An RX-only parent rejects setters and transmit methods. A TX-only parent rejects
getters and `Receive`. Unknown direction permits both. One DBC message receive
updates the current frame shared by its child getters. Transmit handles own
independent buffers, so signal writes and `Tx` must use the handle returned by
that exact message's `TxOpen` call.

Classic CAN limits DBC DLC to eight. Boolean layout is exactly one bit, float
layout exactly 32 bits, and every other scaled raw layout at most 32 bits. Intel
signals use increasing LSB0 positions. Motorola signals use DBC sawtooth
traversal. Overlapping signal writes preserve unrelated bits and use ordinary
last-write-wins behavior for shared bits.

`GetScaled` applies the loaded multiplier and offset. `SetScaled` inverts them
only when the result is on an integer raw grid within a small ULP-bounded check.
It does not guess an M1 rounding rule. Non-finite physical values, non-invertible
metadata, off-grid values, and raw range overflow fail loud. Raw `Get*` accessor
choice determines signed or unsigned interpretation; scaled decode continues to
use the DBC signal's declared signedness.

## Routing and whole-call ownership

Hardware calls use this order:

1. exact call-site `[[io]]` value;
2. wildcard `[[io]]` value;
3. external `HardwareAdapter`;
4. run-owned virtual CAN, including J1939;
5. later built-in models or documented fallbacks where applicable;
6. fail loud.

A returned value means that route owns the entire call. Lower routes do not also
mutate state. For example, an adapter-handled signal setter does not update the
virtual transmit buffer, and an adapter-handled `Receive` does not populate the
virtual DBC current frame. Keep a stateful call chain on one route unless this
separation is deliberate.

Successful Void calls normalize to evaluator unit `Bool(true)` regardless of an
adapter's placeholder scalar. Scenario and adapter replies are normalized to
captured catalogue families before returning to script. Virtual CAN performs its
own argument normalization only after higher-priority routes decline, so the
external adapter continues to receive the original evaluated arguments.

## Trace and public API

JSON traces include an ordered top-level `can` array. Each event records
direction, evaluator seconds and phase, base tick, bus, numeric ID, standard or
extended format, wire bytes, optional handle, optional exact source DBC message,
script, and call offset. CSV remains channel-only.

Scenario-backed frame reads use `HardwareValueSource::VirtualCanRx` and mark the
call external. Deterministic setup, handle, TX-buffer, and transmit operations
use `HardwareValueSource::VirtualCan`. Reading a script-owned TX buffer also uses
`VirtualCan`; only a consumed scenario RX buffer is external.

Rust callers that build public structs or exhaustively match provenance must
account for:

```rust
let scenario = Scenario {
    // existing fields
    serial: Default::default(),
    can: Default::default(),
    // existing fields
};

let trace = Trace::new(); // preferred over a struct literal
```

`Loaded` struct literals must also supply the new `can` field. Prefer
`m1_eval::loader::load`, which builds it from the exact loader snapshot. A
custom loader can use the re-exported API without adding a direct m1-can
dependency:

```rust
use m1_eval::{CanDbcSource, runtime_model_loaded};

let sources = [CanDbcSource {
    path: "dbc/Vehicle.m1dbc",
    bytes: dbc_bytes.as_slice(),
}];
let can = runtime_model_loaded(&project, &scripts, &sources)?;
// Supply `can` alongside the existing fields in the `Loaded` literal.
```

`Scenario::can` exports `CanScenario` and `CanRx`. `Trace::can` exports
`CanEvent` and `CanTransferDirection`. `HardwareValueSource` adds `VirtualCan`
and `VirtualCanRx`. JSON consumers must accept the new top-level `can` array.
`CanEvent::format` uses the re-exported `m1_eval::CanFrameFormat`.
`Loaded::can` exposes the re-exported, read-only `m1_eval::CanRuntimeModel`
retained by the loader.

## Evidence boundary and release gate

The raw byte-reversal and start-bit convention comes from the pinned M1 help
catalogue plus MoTeC's staff-posted CAN bit-numbering note. DBC layout comes from
the exact supplied source bytes. Synthetic vectors cover raw endian mapping,
sub-byte fields, signedness, floats, fixed storage, DBC Intel/Motorola decode,
overlap, lifecycle, timing, routing, and provenance.

State lifecycle, scaling-grid rejection, and offline scheduling remain Assumed
evaluator contracts. There is no captured M1 output establishing rounding,
arbitration, errors, or runtime timing parity.

The runtime model is supplied by the released m1-can v0.2.4 API and pinned by
tag in `Cargo.toml` and `Cargo.lock`.
