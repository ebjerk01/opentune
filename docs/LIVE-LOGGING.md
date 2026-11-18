# Live logging

`opentune-logdef` turns a RomRaider-style `logger.xml` (or EcuFlash/FastECU
`logprotocols`) definition into live samples. Each channel says how it is read:

| Addressing | Request | Positive response |
|---|---|---|
| UDS DID (`mode="22"`) | `22 <DID>` | `62 <DID> <record>` |
| OBD-II PID (`mode="1"`) | `01 <PID>` | `41 <PID> <data>` |
| Memory (`<address>`) | `23 14 <addr:4> <len:1>` | `63 <data>` |

The raw bytes are decoded in the channel's endian and passed through its linear
conversion. A signed `storagetype` (`int8`/`int16`/`int32`) is sign-extended
before the conversion; the CSV `raw` column keeps the unsigned bit pattern.
Non-linear conversions, failed reads and unknown addressing are noted per
sample, never guessed.

```sh
opentune log --def logger.xml --driver <id> --rounds 500 --out run.csv [IDS…]
```

The CSV columns are `round,param_id,name,raw,value,units,note,text`.

## Text channels

Identification records (ISO 14229-1 `F187` part number, `F18C` serial, `F190`
VIN, `F199` programming date, …) are not numbers. A channel's `value_kind`
says how to present its bytes; anything but `number` decodes the **whole**
record into `Sample::text` and leaves `raw`/`value` empty (no conversion):

| `value_kind` | Decoding | Set by |
|---|---|---|
| `number` (default) | first 8 bytes as one integer, scaled | numeric `storagetype` or none |
| `text` (alias `ascii`) | printable ASCII; trailing `00`/`FF`/space padding dropped; other bytes shown as `.` and noted | `storagetype="string"`/`"ascii"`/`"text"` |
| `bcd` | two decimal digits per byte, trailing `F` fillers dropped; a nibble above 9 is noted | `storagetype="bcd"` |
| `hex` | upper-case hex, two digits per byte | `storagetype="hex"`/`"bytes"` |

In a pack's logger JSON set `"value_kind": "text"` (or `bcd`/`hex`) on the
parameter; `number` is the default and is not written. Text channels work in
both polling and fast mode (the record is sliced exactly as for numbers). The
CSV writers put the text in the last column, `text`; the desktop Live-log CSV
(`ts,param_id,name,raw,value,units,note,text`) does the same, and the Log viewer
lists text channels in its notes instead of plotting an empty trace. Older
seven-column files still open.

## Polling

The default mode sends **one request per channel per sample**
(`opentune_logdef::poll_once`, or a `LiveSession` without fast mode). Simple and
universally supported, but on a real bus every request is a round trip of
typically 5–50 ms, so 20 channels cap the sample rate at a few samples per
second.

## Fast mode: UDS 0x2C dynamically defined DIDs

```sh
opentune log --def logger.xml --driver <id> --rounds 500 --fast \
    [--dynamic-did 0xF200] [--max-record 32] [--fast-session 0x03]
```

ISO 14229-1 §10.6 *DynamicallyDefineDataIdentifier* (service `0x2C`) lets the
tester ask the ECU to compose a new identifier — a dynamically defined DID
(DDDID) in the reserved range `0xF200..=0xF3FF` — from slices of existing DIDs
(`defineByIdentifier`, sub-function `0x01`) and from memory ranges
(`defineByMemoryAddress`, `0x02`). A single `22 <DDDID>` then returns all of
those bytes concatenated. `--fast` uses this to read many channels with **one
request per record** instead of one per channel:

1. **Plan.** UDS-DID channels are probed once with `0x22` to learn their record
   length, so each slice is exactly what polling would decode (at most the first
   8 bytes). Memory channels use their declared length. Channels are packed in
   definition order into records of at most `--max-record` bytes; a full record
   opens the next DDDID (`0xF200`, `0xF201`, … up to 4 by default). Inside a
   record the identifier sources come first, then the memory sources.
2. **Define.** Each DDDID is cleared (`2C 03 <DDDID>`) and then defined:
   `2C 01 <DDDID> {<sourceDID> 01 <size>}…` for DID channels and
   `2C 02 <DDDID> 14 {<addr:4> <size:1>}…` for memory channels, at most 8
   elements per request. Successive defines of one DDDID append in order.
3. **Sample.** One `22 <DDDID>` per record. The response length is checked
   against the plan and the record is sliced back into per-channel values with
   the same decoding and conversions as polling.
4. **Recover.** ISO 14229-1 drops DDDIDs when the session changes, and an ECU
   reset or S3 timeout does the same. A DDDID read answered
   `requestOutOfRange` (`0x31`) or with the wrong length is treated as a lost
   definition: the session re-enters `--fast-session` (when given), re-defines
   once and re-reads. If that fails, the log continues in polling mode.
5. **Stop.** Every DDDID the session defined is cleared at the end, including
   when writing the CSV fails.

Channels that cannot be packed are **polled alongside** the dynamic reads: OBD-II
PIDs (a different service), unknown addressing, DIDs whose probe failed, DIDs
that are themselves dynamic, slices larger than `--max-record`, and anything
left over once all DDDIDs are full. `status.polled` and `status.notes` list
them.

### Fallback

If the ECU refuses any part of the setup — `serviceNotSupported` (`0x11`),
`subFunctionNotSupported` (`0x12`), `requestOutOfRange` (`0x31`, e.g. the
record exceeds the ECU's own limit or a source is not allowed),
`conditionsNotCorrect` (`0x22`, e.g. wrong session), `securityAccessDenied`
(`0x33`) or anything else — the session clears what it had defined, switches to
polling and reports why. The CLI prints the mode and reason to stderr:

```text
mode: polling (0x2C defineByIdentifier of 0xF200 refused: NRC 0x31 (requestOutOfRange))
```

and, at the end, the achieved rate:

```text
500 sample(s) of 12 channel(s) in 9.812 s: 51.0 samples/s (611.5 channel values/s), 1.00 requests/sample, 15 setup request(s), 0 re-define(s)
```

If the ECU needs a session for `0x2C`, pass `--fast-session 0x03`. An ECU that
needs SecurityAccess for `0x2C` falls back with `NRC 0x33`; the logging engine
does not unlock security itself.

### Limits

- Only DIDs from `0xF200..=0xF3FF` are ever defined; `--dynamic-did` and the
  DDDIDs after it are validated against that range before anything is sent.
- Record size limits are ECU-specific and not discoverable; `--max-record`
  (default 32 bytes) is deliberately conservative. A define past the ECU's
  limit is refused with `0x31` and the log falls back — lower `--max-record`.
- Memory sources use `addressAndLengthFormatIdentifier` `0x14` (4-byte address,
  1-byte size), the same as the polling `0x23` reads.
- Periodic transmission (`0x2A` ReadDataByPeriodicIdentifier) is **out of
  scope**: every read is tester-initiated, so the sample rate is bounded by the
  request round trip, not by an ECU schedule.
- Requests per sample drop from *N channels* to *N records* (usually 1). The
  wall-clock gain depends on the adapter and bus latency.

**Validation state:** synthetic-tested — against the `opentune-udssim`
conformance server (which implements `0x2C` and `0x23`) and the virtual car.
Not yet bench- or vehicle-tested.

### Prior art

The high-speed logger in [VW_Flash](https://github.com/bri3d/VW_Flash)
(`lib/simos_hsl.py`, BSD-2-Clause) informed the approach: register a list of
memory addresses with the ECU **once**, read them all back with **one** request
per sample, and slice the reply by each parameter's known length. VW_Flash
does this through a `0x3E`-based mechanism provided by patched Simos software,
and batches several DIDs into one standard `0x22` request; OpenTune does the
equivalent with the standard ISO 14229-1 `0x2C` service so it works on any
ECU that implements it. No VW_Flash code was used. Multi-DID `0x22` batching
is a possible later addition.

## Backend API for the desktop panel

The desktop **Live log** panel's *Fast (0x2C)* toggle uses this API
(`desktop/src-tauri/src/feat_logging.rs`: `log_start` / `log_tick` /
`log_stop`, synthetic-tested). It drives
`opentune_logdef::LiveSession` over the driver's `CoreDiag`:

```rust
let cfg = FastLogConfig { enabled: fast, first_did, max_record_len, session, ..Default::default() };
let mut s = LiveSession::new(&def, &ids, cfg)?;   // validates the DDDID range
let status: &LiveStatus = s.start(&mut diag);     // plan + define, or fall back
let samples: Vec<Sample> = s.tick(&mut diag);     // one sample per channel, def order
let status = s.status();                          // after each tick
s.stop(&mut diag);                                // clears the DDDIDs
```

`Sample`, `LiveStatus` and `FastLogConfig` are `serde`-serializable, so Tauri
commands can return them directly. `LiveStatus` carries `mode`
(`"fast-2c"` | `"polling"`), `reason` (why polling), `dynamic_dids`
(`{did, record_len, channels}`), `polled`, `notes`, `redefines`, `ticks`,
`setup_requests` and `tick_requests` (`requests_per_tick()` for display).
Suggested commands: `log_start` → `LiveStatus`, `log_tick` →
`{ samples, status }` (show the mode badge and fallback reason from `status`),
`log_stop` → `LiveStatus`. Keep the session in managed state, make the
commands `async` (serial I/O), never let ticks overlap, and call `stop` when
the panel closes or the connection drops.
