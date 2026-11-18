# Diagnostic protocol layer — notes

Hardening pass over `crates/opentune-core/src/protocol/` plus a new VW TP2.0
transport, informed by mature open-source references. Everything below was
**reimplemented from the observed behaviour** — no reference source was copied
verbatim. The transports are driven by a scripted `CanLink` in unit tests, so
the behaviour claimed here is covered by `cargo test -p opentune-core`.

## References studied

| Project | Licence | What it taught us |
|---|---|---|
| [`rnd-ash/ecu_diagnostics`](https://github.com/rnd-ash/ecu_diagnostics) | GPL-3.0 | Closest prior art (UDS/KWP in Rust). The authoritative source for the **VW TP2.0** wire protocol: `hardware/vwtp.rs` gave the broadcast channel-setup opcodes, the dynamic CAN-id assignment, the `A0/A1` parameter handshake, the timing-byte encoding and the data/ACK PCI nibbles. |
| [`pylessard/python-udsoncan`](https://github.com/pylessard/python-udsoncan) | MIT | UDS service semantics: security-access attempt/delay behaviour, the all-zero "already unlocked" seed convention, `transferData` block-sequence-counter rules. |
| [`pylessard/python-can-isotp`](https://github.com/pylessard/python-can-isotp) | MIT | ISO-TP flow control: `FlowStatus` (ContinueToSend/Wait/Overflow), the exact **STmin byte encoding**, block-size handling, and the `wftmax` bound on consecutive WAIT frames. |
| [`fenugrec/freediag`](https://github.com/fenugrec/freediag) | GPL-3.0 | KWP2000 / ISO 14230 K-line timing (P1–P4, P2min/P2max, P3min/P3max), `busyRepeatRequest` (0x21) and `responsePending` (0x78) handling, 5-baud and fast init. |
| [`fenugrec/nisprog`](https://github.com/fenugrec/nisprog) | GPL-3.0 | Practical KWP2000 read/download sequencing on real ECUs (cross-check for our service clients). |

### Licence notes

OpenTune is GPL-3.0. `ecu_diagnostics`, `freediag` and `nisprog` are GPL-3.0 and
therefore **reuse-compatible**; `udsoncan` and `python-can-isotp` are MIT and
also reusable. Regardless, none of them was copied — the Rust here is an
independent implementation that learns the *behaviour* (opcodes, state machines,
encodings) from the references. The `ecu_diagnostics` TP2.0 implementation is
thread/`mpsc`-based; ours is a blocking request/response transport with the same
observable wire behaviour.

## `isotp.rs` — ISO 15765-2 hardening

Changes (public `CanLink` / `IsoTp` / `DiagTransport` API unchanged; the two
original tests still pass):

- **Flow-control BS/STmin honored on transmit.** `send()` now parses the FC
  frame, obeys the ECU's **Block Size** (waits for a fresh FC after every `BS`
  consecutive frames; `BS == 0` = send all) and sleeps the ECU's **STmin**
  between frames.
- **STmin decoding** per ISO 15765-2 §6.5.5.5: `0x00..=0x7F` → milliseconds,
  `0xF1..=0xF9` → 100–900 µs, reserved values → 127 ms (max). Sub-millisecond
  separations are not slept on (tests/fast links), matching the reference's
  pragmatic behaviour. Unit-tested against the spec table.
- **FC Wait (0x31) / Overflow (0x32).** `WAIT` loops for the next FC, bounded by
  the new `wft_max` field (default 16, ISO `N_WFTmax`); `OVERFLOW` aborts with a
  descriptive protocol error. Reserved flow statuses are rejected.
- **Consecutive-frame sequence validation with correct wrap.** On receive, CF
  sequence numbers are checked to run `1,2,…,0xF,0x0,0x1,…`; a gap is a protocol
  error (previously SNs were not checked at all). The transmit side already
  wrapped correctly; now there is a test (`recv_sequence_number_wrap`, 118-byte
  payload → 16 CFs so the SN wraps 0xF→0x0).
- **29-bit identifiers.** Nothing masks the `u32` CAN id to 11 bits; added a test
  that runs a full exchange on `0x18DAF110`/`0x18DA10F1`.
- **Malformed-frame guards.** Empty frames, zero-length single frames, single
  frames whose declared length exceeds the data, truncated first frames,
  truncated FC frames, and over-long CFs are all rejected instead of panicking
  or reading out of bounds. Receive also emits a fresh CTS per block when our own
  `block_size` is set.
- **Bounded response-pending loop** made configurable (`max_response_pending`,
  default 32) instead of a hard-coded `32`.

New tests: `stmin_decode_matches_spec`, `send_honors_block_size_and_waits_for_second_fc`,
`send_overflow_flow_control_errors`, `send_wait_flow_control_then_continue`,
`recv_sequence_number_wrap`, `recv_sequence_number_gap_is_error`,
`extended_29bit_ids_work`.

## `uds.rs`, `kwp2000.rs`, `diag.rs` — review

- **Security access attempts.** `send_key` now maps both `invalidKey` (0x35) and
  `exceededNumberOfAttempts` (0x36) to `Error::SecurityDenied` in both the UDS
  and KWP clients; `requiredTimeDelayNotExpired` (0x37) stays a raw negative
  response so a caller can honor the ECU's delay timer before retrying.
- **Already-unlocked seed.** Added `uds::seed_is_unlocked()` for the ISO 14229
  all-zero-seed convention so callers can skip the key exchange.
- **Transfer sequence counters.** Documented the `blockSequenceCounter` rules
  (start at 0x01, wrap 0xFF→0x00, NRC 0x73 on mismatch, re-send to retransmit)
  and added `uds::next_block_seq()` plus tests. `parse_max_block_len` is now
  unit-tested (valid, short, and `n == 0`).
- **Response-pending loop bounds.** The `0x78` keep-waiting loop lives in each
  `DiagTransport::request` implementation and is now bounded by a configurable
  counter (ISO-TP and TP2.0 both).
- **NRC table** extended with `responseTooLong` (0x14), `busyRepeatRequest`
  (0x21) and `noResponseFromSubnetComponent` (0x25).
- **KWP2000 P2/P3 timing** documented at the top of `kwp2000.rs`
  (freediag-derived): P1 ≤ 20 ms (inter-byte in a response), **P2min 25 ms /
  P2max 50 ms** (request → response start; longer needs NRC 0x78), **P3min 55 ms
  / P3max 5000 ms** (response → next request; `TesterPresent` must fall inside
  P3max), P4min 5 ms (inter-byte in a request). On CAN the byte-level P1/P4 do
  not apply, but P2/P3 still gate request/response and keep-alive.

## `vwtp20.rs` — VW TP2.0 (new)

The VAG CAN diagnostic transport used by older VW/Audi/Seat/Škoda ECUs
(EDC15/EDC16, ME7, …) before ISO 15765-4 UDS. It carries **KWP2000** application
data, so `Kwp2000Client` runs directly on top. Built over a `CanLink` and
implements `DiagTransport`, exactly like `IsoTp`.

### Design

TP2.0 is connection-oriented. `VwTp20::open()` (called lazily on the first
`request`, or eagerly) does:

1. **Channel setup (broadcast).** Send to CAN id `0x200`:
   `[ecu_id, 0xC0, 0x00, 0x10, rx_lo, rx_hi&0x0F, app_proto(0x01)]`, where
   `rx_id` is the CAN id the tester asks the ECU to answer on. The ECU replies on
   `0x200 + ecu_id`. A positive reply has `data[1] == 0xD0` and carries the
   **dynamically assigned transmit id** in `data[4]` (low byte) and `data[5]`
   (high nibble): `tx_id = ((data[5] & 0x0F) << 8) | data[4]`. `0xD6/0xD7/0xD8`
   are the application-not-supported / temporarily-unavailable / no-resources
   rejects.
2. **Parameter handshake (`A0` → `A1`).** The tester sends
   `[0xA0, BS, T1, 0xFF, T3, 0xFF]` (block size, ACK-timeout, separation time);
   the ECU answers `[0xA1, BS', T1', 0xFF, T3', 0xFF]`. We **store the ECU's `BS'`
   and `T3'`** and honor them when transmitting. Timing bytes use the TP2.0
   encoding (top two bits = unit 100 µs / 1 ms / 10 ms / 100 ms, low six bits =
   count); `encode_timing`/`decode_timing` round-trip it.

### Segmented data

Byte 0 of each data frame is `op_nibble | seq` (4-bit sequence, wraps 0xF→0x0):

| Op | Meaning |
|---|---|
| `0x0` | data, more to follow, **ACK required now** (block boundary) |
| `0x1` | data, **last** frame of the telegram, ACK required |
| `0x2` | data, more to follow, no ACK yet |
| `0x3` | data, **last** frame, no ACK expected |
| `0x9` | ACK — "received, not ready for next" (a `0xB_` follows) |
| `0xB` | ACK — "ready for next block" (low nibble = next expected seq) |

The application telegram is `[len_hi, len_lo, payload…]` (12-bit length) split
across the frames' bytes `1..`. On transmit we emit `0x2_` frames until the
negotiated block size is reached, then a `0x0_` frame and wait for the ECU's
`0xB_` ACK; the final frame is `0x1_` and also awaits an ACK. On receive we
accumulate, validate the sequence runs contiguously, send a `0xB_` ACK at each
block boundary and after the last frame, then strip and length-check the telegram
(trailing DLC padding is tolerated).

### Keep-alive / management

- `keep_alive()` sends `A3` (channel test) and expects the ECU's `A1`; call it
  within P3max while the channel is idle. `tp20channel::Tp20Channel` (what the
  raw-CAN drivers open with `transport=tp20`) does this from a background
  thread after an idle interval, serialised with requests, and sends `A8` on
  close.
- Every wait is bounded by a deadline, so a busy bus (frames on other ids
  arriving continuously) cannot stretch a timeout.
- While receiving, an ECU-initiated `A3` is answered transparently with an `A1`,
  stray `A1`s are absorbed, and an ECU `A8` (disconnect) surfaces as an error.
- `disconnect()` sends `A8` to tear the channel down.

### Tests (scripted `CanLink`)

`channel_setup_assigns_tx_id` (broadcast + dynamic id + `A0` emission),
`single_frame_request_response`, `multiframe_send_honors_block_size` (PCIs
`2,0,2,1` with `BS=2`), `multiframe_receive_acks_blocks` (mid-stream + final
ACKs), `setup_rejection_is_error` (`0xD6`), `response_pending_is_retried`
(`0x78` loop), `timing_byte_roundtrips`.

## TP2.0 hardening: loss recovery, bounds, fuzzing

A second pass over `vwtp20.rs`, `tp20channel.rs`, `tp20sim.rs` and the
KWP2000 client, driven by a fault-injecting simulator and a seeded fuzzer.

### Sources for the recovery rules

The public protocol description "VW Transport Protocol 2.0 (TP 2.0) for CAN
bus" (jazdw.net) defines the pieces used here: the data opcodes `0x0`–`0x3`
(ACK expected / not, more / last), the ACKs `0xB` (ready) and `0x9` (not
ready), the 4-bit sequence that "increments up to 0xF then back to 0x0", the
block size ("number of packets to send before expecting an ACK"), `T1`
("time to wait for ACK"), `T3` ("interval between two packets"), `A3`
(channel test, answered like the parameter response, keeps the channel
alive), **`A4` ("break: receiver discards all data since last ACK")** and
`A8` (disconnect). It says nothing about *when* to retransmit; the rule
below is ours, built from those primitives, and is what the simulator
implements on the module side as well.

### Rules implemented

| Situation | Behaviour |
|---|---|
| Sequence numbering | Starts at 0 in both directions on every new channel and runs continuously across telegrams. The receiver never synchronises on "whatever comes first": a lost first frame would otherwise make the second one the start of the telegram (the fuzzer found exactly that). |
| Block size | We honour the module's `A1` block size, but **at most 15 frames per ACK; 0 and values above 15 count as 15** (`vwtp20::effective_block_size`). With a 4-bit sequence a block of 16+ frames cannot be retransmitted unambiguously: its replay restarts on the very sequence number the receiver expects next and is taken as new data (the fuzzer found a whole request executed twice that way). The simulator applies the same limit to its replies. |
| Our block not ACKed (transmit) | Wait `T1` from the module's `A1` (at least `ACK_WAIT_FLOOR_MS` = 100 ms, since USB adapters add latency; at most the request timeout). No matching `0xB_`: send `A4` and re-send the whole block from its first frame with the **same** sequence numbers, at most `max_block_retries` times (default 2, `tp20_ack_retries`), then fail with a protocol error naming the missing ACK. A `0x9_` for the block stops retransmission (the module has it) and waits up to the full timeout for the `0xB_`. An ACK with another sequence is ignored (duplicate or wrong) and counted. |
| Reply starts while we wait for the ACK of our *last* frame | Taken as the implicit ACK (the module has the whole request); the frame is kept for the receive path. Only a frame with the expected sequence counts; anything else is stale and ignored. |
| Receive: expected sequence | Accepted. A `0x0_`/`0x1_` frame is ACKed with the next sequence; that ACK is the rollback point. |
| Receive: exact repeat of the previous frame, or (right after an ACK) a frame of the block we last ACKed | Discarded as a duplicate / replay (the module missed our ACK); at the replayed block's last frame the same ACK is sent again. Also handled outside a telegram (while we send or keep alive). |
| Receive: any other sequence (a frame was lost) | Nothing is accepted and nothing ACKed until the module's `A4`, which rolls the telegram back to our last ACK; the module then re-sends. In an unacknowledged (`0x3`) tail no retransmission will come, so that is an immediate sequence error. At most 8 `A4`s per telegram. |
| Module `A3` | Answered with our `A1` at any point (also mid-telegram). |
| Module `A8`, or a request that fails mid-exchange | The channel is torn down (we send `A8` unless the module did) and the **next request sets up a new channel** (`VwTp20::request` opens lazily; `Tp20Stats::reopens`). A late reply can therefore never be taken for the next request's answer. The module's KWP session is gone: callers restart it. `VwTp20::last_break` keeps the reason. |
| Keep-alive (`Tp20Channel`) | `A3` after `tp20_keepalive_ms` idle, serialised with requests; one retry; two unanswered mark the channel lost until `Tp20Channel::reopen`. The thread keeps running across request-level breaks. |
| `T3` separation | Honoured, clamped to `MAX_SEPARATION_US` = 100 ms (the timing byte can encode 6.3 s). |
| responsePending (0x78) | Waited through only when it echoes the requested service (`diag::is_response_pending`); each wait uses `response_pending_timeout_ms` (P2*, 5000 ms, at least the request timeout); more than `max_response_pending` (32) is a protocol error naming the count. A 0x78 for another service is handed to the client, which reports it as mis-correlated. |
| Telegram length | Request payload 1..=4095 bytes (12-bit length), checked before the bus is touched. A received telegram must declare a non-zero length, carry at least that much, and at most one frame's padding more. |
| Setup | A module that assigns the tester its own receive id or an id in `0x200..=0x2FF` is refused. |

Every wait is a wall-clock deadline that only restarts on progress (an
accepted frame or an `A4`), so neither a busy bus nor a stream of
duplicates can stretch it, and every loop is bounded by a count.

### Simulator (`tp20sim`)

The module side is now scriptable without breaking its old API:

- a **virtual clock**: `Tp20SimLink::recv_frame` with nothing queued lets
  `Tp20Ecu::advance` run up to the wait (delayed replies, retransmissions)
  instead of sleeping, so timing tests cost no wall-clock time;
- **faults** (`Tp20EcuConfig::faults`): drop, duplicate, delay, reorder
  (swap with the next frame), wrong sequence, or module disconnect, on the
  n-th frame of a class (data, ACK, params, `A3`, `A4`, `A8`, setup) in
  either direction;
- module behaviour: configurable `T1`/`T3` in its `A1`, its own ACK timeout
  (`A4` + resend, `max_retransmits`, then `A8`), responsePending chains with
  intervals, reply delays, a channel test before each reply, and a
  disconnect instead of the n-th reply;
- counters for everything (`retransmissions`, `breaks_received`,
  `duplicates`, `gaps`, `channel_test_replies`, `keep_alive_times`, …) and an
  opt-in wire trace (`record_wire`).

Without faults the module stays strict (every gap or stray ACK from the
tester lands in `errors`), as before.

Two fault shapes are outside what any TP2.0 receiver can detect, and the
fuzzer keeps them out: a *delayed data frame* released more than 15 frames
late (use `Reorder`), and a *rewritten data sequence* next to another
data-frame fault (frame k+1's data labelled k forges the lost frame k). Both
are corruptions the CAN CRC and in-order delivery rule out on a real bus.

### Tests

- `tests/tp20_faults.rs`: one test per rule above (lost ACK each way, lost
  data each way, duplicates/reorders, wrong sequence then re-open, sequence
  wrap with long telegrams in both directions and several block sizes, block
  size clamping, `T1`/`T3` variants staying bounded, responsePending chains
  and their bound, module disconnects, module channel tests, late replies,
  retry exhaustion, lost setup/`A1`, payload size limits, keep-alive retry,
  `Tp20Channel` re-open, the `tp20_ack_retries` option).
- `tests/tp20_fuzz.rs`: a deterministic seeded fuzzer (splitmix64). Each case
  draws a module configuration, client block size / retries and up to five
  faults, then runs up to four uniquely tagged requests and asserts: no
  panic; each request returns within 5 s (in practice microseconds); the
  result is exactly that request's reply or a `Protocol`/`Timeout` error;
  the module executed each request at most once, in order; and cases
  without faults or module disruptions always succeed with no protocol
  violation. It also asserts that every recovery path was reached.
  `cargo test` runs 3000 cases (~0.5 s); `OPENTUNE_TP20_FUZZ_CASES=2000000`
  runs a soak, `OPENTUNE_TP20_FUZZ_SEED=<seed>` replays one case with a wire
  trace.

### KWP2000 client

- `kwp_nrc_name`: the ISO 14230-3 negative-response table. Shared codes keep
  the UDS name; KWP-specific meanings win (0x40–0x43 download, 0x50–0x53
  upload — not the UDS certificate codes —, 0x71–0x79 block transfer, 0x80,
  `reservedByDocument` 0x81–0x8F/0xFF, `manufacturerSpecific` 0x90–0xF9,
  `systemSupplierSpecific` 0xFA–0xFE). `Kwp2000Client` reports these names.
- Echo checks: `startDiagnosticSession` (an echo-less `50` is tolerated),
  `readEcuIdentification` option, `securityAccess` level on both seed and
  key; a seed response without a seed is an error.
- `readMemoryByAddress`: empty or overlong blocks and addresses beyond 24
  bits are errors; a short block is still returned (the K-line drivers
  advance by its length); `read_memory_exact` refuses it too.
- A 0x78 that reaches the client is a `NegativeResponse`, never success.
