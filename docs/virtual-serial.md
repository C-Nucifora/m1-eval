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
`SerialDirection`. These additions leave the public `EvalCtx` shape unchanged;
direct `eval` and `exec_script` calls use a fresh virtual adapter without
scenario RX data.
