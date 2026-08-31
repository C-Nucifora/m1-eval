<!-- SPDX-License-Identifier: GPL-3.0-or-later -->
# Virtual serial adapter

`m1-eval` includes a deterministic offline RS232 byte-buffer adapter. It does
not open host serial devices or attempt to reproduce electrical timing. The
adapter is fresh for every scenario run and uses evaluator time only.

This behavior has synthetic tests but no captured M1 or M1 Sim comparison. It
is **Assumed** under the repository maturity contract.

## Scenario input

Declare one-shot receive chunks in TOML with `[[serial.rx]]`:

```toml
[[serial.rx]]
time_s = 0.025
port = 0
bytes = [0x1b, 0x30, 0x35, 0x0d]
```

`time_s` must be finite and non-negative, `port` must fit a non-negative M1
Integer, and each byte must be in `0..=255`. Source declarations are ordered by
time and retain declaration order when timestamps match. A public
`SerialScenario` constructed directly receives the same stable ordering inside
the adapter.

`Serial.Receive(handle, port)` sees all not-yet-seen chunks whose timestamp is
at or before that call's evaluator time. It replaces and flushes that handle's
receive buffer. It returns `true` when at least one byte arrived and `false`
otherwise. A receive buffer may hold at most 256 bytes.

Each handle has its own cursor for each port. Two handles receiving from the
same virtual port therefore observe the same injected stream independently.
This fan-out is an explicit evaluator contract requested for repeatable tests;
it is not a claim about a physical port's destructive read behavior.

## Run-mode boundaries

Whole-project mode creates one virtual adapter before the `On Startup` pass and
shares it with every periodic function. A startup `Serial.PortInit` therefore
configures the port used by later `Receive` and `Transmit` calls.

Function and cone modes do not run the project's startup functions. The selected
function or cone must call `PortInit` before it reaches virtual `Receive` or
`Transmit`. A `[[serial.rx]]` declaration supplies bytes; it does not initialize
a port. A higher-priority `[[io]]` value or user adapter may replace the serial
sequence, but it must own every state-dependent call in that sequence.

Counterfactual replay has no `Scenario`, so it has no `[[serial.rx]]` stream. It
also skips project startup and begins with an empty virtual adapter. A number
loaded from a log's handle channel is data, not a handle allocated by that
adapter. Counterfactual cone code must initialize its ports and create its
handles in the recomputed call chain. A user adapter can instead own the full
serial sequence. Missing setup fails with the same uninitialized-port or
invalid-handle error as any other run.

## Routing

Hardware calls use this precedence:

1. Exact-site `[[io]]` value.
2. Wildcard `[[io]]` value.
3. User `HardwareAdapter`.
4. Virtual serial adapter.
5. Deterministic `System` model.
6. Existing documented stubs.
7. Fail loud.

An external adapter can therefore replace any serial method. Scenario `[[io]]`
can also replace one method result, while `[[serial.rx]]` supplies the byte
stream to the built-in virtual adapter.

The first route that returns a value owns the whole call. Lower routes do not
run for side effects. For example, a `[[io]]` value for `Serial.PortInit` returns
success without configuring the virtual port, so a later virtual `Receive`
fails as uninitialized. A supplied `GetHandle` value is likewise unknown to the
virtual adapter and fails if a later getter falls through to it. A user adapter
that returns `AdapterReply::Unhandled` lets that call fall through to virtual
serial. Stateful serial calls should stay on one route unless the higher route
deliberately implements the entire chain.

## Handles, ports, and buffers

- `GetHandle` and deprecated `GetTransmitHandle` return a nonzero M1
  `UnsignedInteger`. A source call site gets the same handle on every execution.
  Different call sites get different handles. A fresh run repeats the same
  deterministic allocation order.
- Reopening one call site with a different endian flag fails.
- Handles own separate receive and transmit buffers. Getters read RX. Setters
  write TX. `Transmit` snapshots TX, so later writes do not alter old events.
- Buffers are 256 bytes. Negative offsets or lengths, integer widths outside
  `1..=4`, checked range overflow, and reads beyond delivered RX bytes fail with
  a method-specific diagnostic.
- `PortInit` accepts non-negative ports, protocol `0` (RS232), and raw baud
  values `1200`, `1800`, `2400`, `4800`, `9600`, `19200`, `38400`, `57600`, or
  `115200`. Repeating the same configuration succeeds. Conflicting
  reconfiguration, an unknown baud, or another protocol fails clearly.
- `PortDiagnostic` returns M1 Integer `0` for an uninitialized port and `1` for
  an initialized, healthy virtual port. Other diagnostic codes are not modeled.
- Receive and transmit require an initialized port.

## Implemented RS232 methods

| Method | Virtual behavior |
| --- | --- |
| `GetHandle`, `GetTransmitHandle` | Stable typed handle creation. |
| `PortInit`, `PortDiagnostic` | Stateful configuration and status. |
| `Receive` | Timed RX delivery and per-handle cursor advance. |
| `GetInteger` | Read 1 to 4 RX bytes and sign-extend. |
| `GetUnsignedInteger` | Read 1 to 4 RX bytes without sign extension, returned in the catalogue's signed Integer family. |
| `GetFloat` | Read four RX bytes as IEEE-754 binary32. |
| `SetInteger`, `SetUnsignedInteger` | Write the selected low bytes to TX and return the first unwritten offset. |
| `SetFloat` | Write binary32 bits to TX and return the first unwritten offset. |
| `SetString` | Write exact-length ASCII to TX and return the first unwritten offset. |
| `Sum8`, `XOR8` | Compute an 8-bit checksum over a checked range. |
| `Transmit` | Record the requested TX prefix as an ordered event. |

The handle's endian flag controls every multibyte integer and float. Big endian
places the most significant selected byte first; little endian places the least
significant byte first.

The pinned catalogue declares `GetUnsignedInteger` and the value argument of
`SetUnsignedInteger` as signed Integer despite their names. The evaluator
preserves all 32 bits at those boundaries. Narrow writes keep the selected low
bytes.

Four details are explicit evaluator assumptions because the available source
material does not settle them:

- `PortInit` interprets its argument as the raw baud rate used in the manual
  examples, not the ordinal value of the separate baud-rate enumeration.
- `SetString` accepts exact-length ASCII only. It does not guess an encoding,
  padding, or truncation rule.
- `Sum8` wraps modulo 256. `Sum8` and `XOR8` select the buffer most recently
  touched by `Receive` or a setter.
- A setter returns `offset + bytes_written`, interpreted as the first unwritten
  byte.

The pinned catalogue has no `Serial.GetString`, so the evaluator does not invent
one.

## Unsupported behavior

`GetLinOffset`, `SetLinHeader`, and `LinDump` remain unsupported after scenario
and user-adapter precedence. LIN frame layout, protected identifiers, checksum
rules, and logging effects are not sufficiently defined for this byte model.
Live host serial IO, framing, parity, timeouts, and hardware faults are also out
of scope.

## Trace output

JSON traces include an ordered `serial` array. Each event records `direction`,
`time_s`, `phase`, `base_tick`, `port`, `handle`, `bytes`, `script`, and
`offset`. RX-derived calls use hardware provenance source
`virtual-serial-rx` and are marked external. Deterministic handle, configuration,
TX-buffer, and transmit operations use `virtual-serial` and are not marked
external. CSV remains the historical time-plus-channel format and contains no
serial event records.

## Library API

`Scenario::serial` contains the exported `SerialScenario` and `SerialRx` types.
`Trace::serial` contains ordered exported `SerialEvent` records with a
`SerialDirection`. `HardwareValueSource` adds `VirtualSerial` for deterministic
adapter results and `VirtualSerialRx` for scenario-derived results.

Rust callers that construct these public structs or match the provenance enum
must update their source:

```rust
let scenario = Scenario {
    // existing fields
    serial: Default::default(),
    // existing fields
};

let trace = Trace::new(); // `Trace::default()` is equivalent
```

Code that still builds a `Trace` literal must add `serial: Vec::new()`. Prefer
`Trace::new()` or `Trace::default()` so metadata fields start empty. Exhaustive
matches on `HardwareValueSource` need arms for `VirtualSerial` and
`VirtualSerialRx`. JSON readers must also accept the new top-level `serial`
array.

These additions leave the public `EvalCtx` shape unchanged. Each direct `eval`
or `exec` call gets a fresh adapter. `exec_script` keeps one fresh adapter for
all statements in that script. None of these direct APIs supplies scenario RX
data. Public builtin-level helpers that receive only an `EvalCtx` also create
fresh per-call serial state, so handles do not persist across separate helper
calls.

The sibling `m1-lsp` crate's `src/eval/engine.rs::offline_scenario` literal needs
`serial: Default::default()` when its m1-eval dependency advances. That is a
release-coordination follow-up; this m1-eval change does not modify the sibling
repository.
