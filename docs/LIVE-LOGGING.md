# Live logging

`opentune-logdef` turns a RomRaider-style `logger.xml` (or EcuFlash/FastECU
`logprotocols`) definition into live samples. Each channel says how it is read:

| Addressing | Request | Positive response |
|---|---|---|
| UDS DID (`mode="22"`) | `22 <DID>` | `62 <DID> <record>` |
| OBD-II PID (`mode="1"`) | `01 <PID>` | `41 <PID> <data>` |
| Memory (`<address>`) | `23 14 <addr:4> <len:1>` | `63 <data>` |
| Subaru SSM (`<protocol id="SSM">`, `--transport ssm`) | `A8 00 <addr:3>…` (batched) | `E8 <byte per address>` |

The raw bytes are decoded in the channel's endian and passed through its
conversion: linear (`x*0.25`, `(x-128)*100/128`) or an expression in the safe
grammar below (`14.7/(1+x)`). A signed `storagetype` (`int8`/`int16`/`int32`)
is sign-extended and a `float` one is read as an IEEE-754 single before the
conversion; the CSV `raw` column keeps the unsigned bit pattern. A
**calculated** channel (RomRaider `<depends>`, e.g. `(P12*60)/P8`) is computed
from the other channels of the same round (see
[RomRaider logger definitions](#romraider-logger-definitions)).
Unsupported conversions, failed reads and unknown addressing are noted per
sample, never guessed.

```sh
opentune log --def logger.xml --driver <id> --rounds 500 --out run.csv [IDS…]
# or an OpenTune JSON logger definition (import-romraider-logger -o, pack loggers/*.json)
opentune log --def ssm.json --driver <id> --rounds 500 --out run.csv [IDS…]
```

The CSV columns are `round,param_id,name,raw,value,units,note,text,ts`:
`round` counts polls from 0, and `ts` is the wall-clock time each round
started, in Unix milliseconds (every row of a round shares it). The Log viewer
opens the file as an OpenTune Live log, like the desktop's; a file written
before the `ts` column existed still opens, with the round number as the time
axis.

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

## Subaru SSM2

Subaru Select Monitor 2 reads ECU RAM by address. `log --transport ssm` speaks
it over a K-line, `--transport ssm-can` over CAN; the `logger.xml` is a
RomRaider one (`<protocol id="SSM">`).

```sh
# K-line: a KKL cable or the OpenTune K-line bridge (port from OPENTUNE_PORT)
OPENTUNE_PORT=/dev/ttyUSB0 opentune log --def logger.xml --driver kkl --transport ssm --rounds 500 --out run.csv
# CAN (ISO 15765, 7E0/7E8), e.g. a slcan/socketcan adapter
opentune log --def logger.xml --driver socketcan --transport ssm-can --out run.csv
# Offline against the simulated module
opentune log --def crates/opentune-logdef/tests/fixtures/ssm_demo_logger.xml --driver ssmsim --transport ssm --rounds 10
```

**The wire** (`opentune_core::protocol::ssm`). On K-line every packet is
`80 <dst> <src> <len> <cmd> <data…> <checksum>` at 4800 baud 8N1: the tester
is `F0`, the engine ECU `10`, the TCU `18` (`--ssm-target ecu|tcu`), `<len>`
counts the command and data bytes, and the checksum is the 8-bit sum of
everything before it. There is no wake-up, so the K-line driver is opened with
`init=none`. The line is half duplex: a plain KKL cable hears its own packet
first, and the transport reads that echo back and requires it to match byte
for byte before it reads the reply (an adapter that strips the echo itself,
the K-line bridge, says so). The reply is read by its length byte, and its
checksum and addresses are checked. Over CAN the same `<cmd> <data…>` payload
rides ISO-TP on `7E0`/`7E8` (`7E1`/`7E9` for the TCU) with no header, length
or checksum.

| Command | Reply | Use |
|---|---|---|
| `BF` | `FF <SSM ID:3> <ROM ID:5> <capability bytes…>` | identify the module (once, before logging) |
| `A8 00 <addr:3>…` | `E8 <one byte per address>` | the logging read |
| `A0 00 <addr:3> <count-1>` | `E0 <count bytes>` | block read (`SsmClient::read_block`; not used by `log`) |
| `B8 <addr:3> <value>` | `F8 <value>` | **write**: `SsmClient` refuses it until `arm_writes()`; logging never arms it |

A reply starting `7F <cmd> <code>` is reported as a negative response with
the ISO 14229 NRC name.

**Which parameters** (`opentune_logdef::ssm::select`). The `BF` reply's data
is indexed the way RomRaider's `ecubyteindex`/`ecubit` attributes count it:
bytes 0–2 are the SSM ID, 3–7 the ROM ID, and the capability bitmap starts at
index 8. A standard `<parameter>` or `<switch>` is logged only when its
capability bit is set; an `<ecuparam>` only for the `<ecu id>` equal to the ROM
ID (comma-separated id lists match too); and only parameters whose `target`
(1 = ECU, 2 = TCU, 3 = both) includes the selected module. A calculated
parameter (`<depends>`, no address of its own) is kept when every parameter
its expression references is kept, and computed after each read; one whose
inputs this module does not log, or whose conversion does not reference its
dependencies, is left out. Everything left out
is listed on stderr with the reason, and a parameter id given on the command
line that is not logged gets a warning. Older definitions that give a
multi-byte value as one `<address>` per byte are read as one value when the
addresses are consecutive.

**Batching.** Every byte address the selected channels cover is read once per
sample, in `A8` requests that keep each request and reply packet within
`--ssm-max-packet` bytes (default 80: 24 addresses per read, which fits the
K-line bridge's buffers; the protocol allows up to 84 addresses). A channel's
bytes stay in one request so a multi-byte value is not assembled from two
moments, unless the channel alone is larger than one request (noted). A
failed read (timeout, bad checksum, echo mismatch, NRC) is noted on the
channels of that request only, and the next sample tries again. `--fast` does
not apply (it is UDS `0x2C`).

The session is a `LiveSession` in mode `ssm-a8`, set by
`FastLogConfig { ssm: Some(SsmPollConfig { max_packet_len }), .. }`, so the CSV
is the same as for every other path.

**Shared setup.** The wire setup and the identify/select step live in
`opentune_logdef::ssm` (`SsmWire::configure` / `prepare` / `with_transport`,
`ssm::prepare` → `SsmPrepared { init, def, cfg, notes, not_logged }`), so the
CLI and the desktop **Live log** panel (*Transport* SSM K-line / SSM CAN,
*Module* ECU / TCU, *Simulated module*) run the same code.

**Validation state: synthetic-tested.** The SSM simulator
(`opentune_core::protocol::ssmsim`: synthetic ROM ID, capability bitmap and
RAM addresses that change on every read, optional echo, fault injection) and
pseudo-terminal tests of the `kkl` and `kline-bridge` raw K-line. Not yet run
against a Subaru, and the K-line bridge's 60 ms reply gap is not verified
against SSM reply latency. The ELM327 drivers are not wired for SSM (see
[DRIVERS.md](DRIVERS.md#3k-optional-raw-k-line-kline-for-ssm2)).

## RomRaider logger definitions

`opentune import-romraider-logger` turns a RomRaider logger definition
(`logger.xml`, the open format of RomRaider's `logger.dtd`) into an OpenTune
logger definition: the JSON `LogDef` a pack keeps in `loggers/*.json` and that
`log --def` and the desktop **Live log** read. It imports **one protocol** (SSM
or OBD; default the first in the file) with its standard `<parameters>` and
`<switches>`, plus, with `--ecu <ROM id>`, that ECU's `<ecuparam>` addresses
(comma-separated `<ecu id>` lists match too).

```sh
opentune import-romraider-logger logger.xml --ecu 2F12785206 -o ssm-2f12785206.json --report report.json
opentune log --def ssm-2f12785206.json --driver kkl --transport ssm --rounds 500 --out run.csv
# Back to RomRaider's format (the representable subset; omissions are listed)
opentune export-romraider-logger ssm-2f12785206.json -o logger-subset.xml
```

Every parameter gets a status in the summary (and in `--report`):

| Status | Meaning |
|---|---|
| `linear` | `<address>` read, affine conversion (`x/4`, `(x-128)*100/128`) |
| `expression` | `<address>` read, non-affine conversion evaluated per sample (`14.7/(1+x)`, `1/(x*.0004882812)`) |
| `calculated` | computed from other imported parameters each round (`(P8*P21)/1200`, `[P7:psi]-[P24:psi]`) |
| `switch` | one bit (`<switch byte bit>`, or `<address bit>`) |
| `text` | `storagetype` string / bcd / hex, decoded as text |
| `refused` | kept in the file but **disabled** (`"enabled": false`) with the reason; its conversion is marked unusable, so even a forced read shows only the raw value with a note |

A parameter is refused, never silently wrong, when its expression is outside
the grammar, it references a parameter that is not imported or was refused
(for a per-ECU input the report says to pass `--ecu`), it asks for a unit
conversion the referenced parameter does not have (`[P9:knots]`), the
references form a cycle, it has `<depends>` but its conversion does not use
them (`x*2` with no address), or its address is outside the 3-byte SSM range.
An *alternative* conversion that cannot be evaluated is dropped from the
parameter and named in its detail. The output passes `LogDef::validate`.

**Expression grammar** (`opentune_logdef::expr`, parsed to a tree and
evaluated; nothing is handed to an interpreter): numbers (`12`, `.004`, `1e-3`,
`0x1F`), the raw value `x` (EcuFlash `[value]`), parameter references (a bare
id `P8` = that parameter in its primary, first, conversion; `[P9:mph]` = in the
conversion with those units, which may contain `/` or `µ`; EcuFlash
`GetLogParam("…")` = a bare id), `+ - * /`, `^` (power, right-associative),
unary minus, parentheses, and `abs sqrt exp ln log10 floor ceil round min max
pow`. Comparisons, conditionals and other functions are refused with the
construct named. Evaluation fails, leaving the sample empty with a note, on a
division by zero, a non-finite result or a missing input.

**Calculated parameters at run time** (`opentune_logdef::derived`). Selecting
a calculated parameter also logs the parameters it references (placed before
it in the CSV), on every path: polling, `--fast` (they are computed, not
polled) and SSM. After each round the calculated channels are evaluated from
that round's values; an input that did not answer gives `calculated: P8 has no
value this round` instead of a stale number.

**Also converted**: one-`<address>`-per-byte fragments (consecutive = one
value), `storagetype`/`endian` (signed, `float`, little-endian), `gauge_min` /
`gauge_max` / `gauge_step`, `format`, SSM `ecubyteindex`/`ecubit`/`target`,
and OBD `<address length>` as the value width (so one byte of a two-value PID
such as `14` is read from its own byte).

**Not modeled (reported in the notes)**: `<replace>` value labels (the number
is logged), `<dtcodes>`, `<transports>`/`<module>` (OpenTune picks the module
with `--ssm-target`), OBD supported-PID capability bits (an unsupported PID
answers with a negative response, noted per sample), a bit inside an OBD PID
(refused), and the **DS2** (BMW, `group`/`subgroup` telegrams) and **NCS**
(Nissan Consult) protocols: OpenTune has no live transport for them, so
`--protocol DS2` is an error and their parameters are never read as plain
memory. A real RomRaider file starts with `<!DOCTYPE logger SYSTEM
"logger.dtd">`; the declaration is accepted (the external DTD is not fetched
and entity expansion is bounded).

**Export** (`export-romraider-logger`, `opentune_logdef::export_romraider_logger`)
writes memory, PID and DID parameters, switches, per-ECU parameters,
`<depends>` and conversions, and lists what it leaves out (`<alt>` addresses,
EcuFlash unit-conversion groups, protocols without addressing). UDS DID and
mode-23 sections are written with a `mode` attribute, an OpenTune/FastECU
extension RomRaider itself does not read. Importing an export reproduces the
definition (tested).

**Licences and validation.** RomRaider is GPL-2.0-or-later; this is an
independent implementation of the published format, no RomRaider code is used.
The community definition files (RomRaider/SubaruDefs and forks) carry no
licence, so **none are committed**: the tests use a synthetic fixture
(`crates/opentune-logdef/tests/fixtures/romraider_logger_synthetic.xml`, every
construct above, addresses served by the SSM simulator) and log the imported
definition end to end against `ssmsim` and a scripted OBD-II ECU. The importer
was also run, at development time only, against four public `logger.xml` files
(RomRaider/SubaruDefs `standard` and `metric`, a fork's `STD_EN` and
`METRIC_EN`; 160–440 parameters each): all import and validate, with and
without an `--ecu`, every ECU id in one file imports, and export → re-import
reproduces the definition. To repeat that check on your own copy:

```sh
OPENTUNE_ROMRAIDER_LOGGER=/path/to/logger.xml cargo test -p opentune-logdef --test romraider real_romraider -- --nocapture
```

**Validation state: synthetic-tested.** Not yet logged on a Subaru.

## Long runs, dropped links and reconnect

`opentune log` and `opentune obd read` run on the same loop
(`opentune_core::resilient::run_resilient`) and share these flags (the
desktop Live log session runs on it too, with a *Reconnect* box: see
[Backend API for the desktop panel](#backend-api-for-the-desktop-panel)):

| Flag | Default | Meaning |
|---|---|---|
| `--rounds N` | 1 | rounds to log; `0` = until Ctrl-C or `--duration` |
| `--interval-ms MS` | 0 | pause between live rounds |
| `--duration SECONDS` | none | stop after this long |
| `--reconnect` | off | when the link is lost, close the driver, re-open it with the same settings and resume |
| `--dead-after N` | 3 | rounds in a row with no response at all before the link counts as lost |
| `--reconnect-backoff-ms MS` | 500 | first backoff; doubles per attempt |
| `--reconnect-max-backoff-ms MS` | 10000 | backoff ceiling |
| `--reconnect-max-attempts N` | never | give up after N failed re-opens |
| `--reconnect-give-up SECONDS` | never | give up once the link has been down this long |

```sh
opentune log --def logger.xml --driver <id> --fast --rounds 0 --interval-ms 100 \
  --reconnect --out drive.csv            # until Ctrl-C, surviving an unplug
opentune obd read --driver elm327obd --rounds 0 --duration 600 --interval-ms 250 \
  --reconnect --out drive.csv 0C 0D 05 0B   # Live-log CSV, as the desktop Dashboard records
```

**Dead rounds and lost links.** A round in which requests were sent and
*none* got any response is *dead* (a negative response is an answer: the link
is alive). Its rows are still written, with empty values and the transport
error in `note` — never a repeated or invented value. After `--dead-after`
dead rounds in a row the link is **lost**; an error that says so outright
(`Error::AdapterReset`, e.g. an adapter that power-cycled and printed its
banner, or `Error::NotConnected`) counts as lost at once. Then:

- with `--reconnect` the driver is closed and re-opened after a backoff
  (500 ms, 1 s, 2 s, … up to 10 s), until it opens or a give-up limit is hit.
  After a re-open the session starts again: `--fast` re-defines its dynamic
  DIDs, `obd read` forgets what the old connection taught it (multi-PID
  support, PIDs that never answered) and re-reads the support bitmaps;
- without it the same link keeps being polled, with the same growing pause
  between dead rounds, in case it comes back on its own.

The loop never spins: every dead round and every re-open attempt is preceded
by a sleep, even when the transport fails instantly. A clean end (rounds,
`--duration`, Ctrl-C) still clears the dynamic DIDs while the link is alive;
while it is lost nothing could answer, so nothing is sent.

**Marker rows.** Each link event is one row with `param_id` `link` (name
`Link status`), an empty `value`, the event in `note` and its state in `text`:
`lost`, `reopen-failed`, `reconnected`, `recovered` (answered again without a
re-open) or `gave-up`. The Log viewer lists `Link status` as a text channel
and shows the outage as a gap. stderr gets a summary:

```text
link: 4 dead round(s), 1 drop(s), 1 reconnect(s) in 3 attempt(s), 0 recovered without reopen, 6.2 s down
```

**OBD-II recording.** `obd read --out FILE.csv` writes the desktop
Dashboard's Live-log layout, `ts,param_id,name,raw,value,units,note,text`
(one row per channel per poll; a multi-sensor PID becomes `key#i` rows named
`… S1`, `… S2`; a channel without a value keeps its error in `note`). Without
`--out` it prints as before (`--json` for one readout per line). A poll in
which nothing answered teaches `ObdSession` nothing, so an unplugged adapter
cannot mark every PID "not supported by ECU" for the rest of the run.

**GPS.** A serial receiver that ends mid-log (USB unplug) leaves the GPS
channels empty with a note (`stale: no NMEA for 3.0 s (source ended: read
error: …)`); the ECU log carries on. With `--reconnect`, `--gps <port>` also
re-opens the port with the same backoff (`opentune_gps::ReopenFeed`); a
recorded NMEA file is never re-opened.

## Triggers

A **trigger** watches the logged channels and acts on a condition: record
only the wide-open-throttle pulls of a drive, split them into one file each,
or drop a marker row into the log when the coolant passes 105 °C. Triggers
are data: a JSON file (`"format": "opentune-triggers"`), loose or shipped in
a definition pack's `triggers/` folder
([DEFINITION-PACKS.md](DEFINITION-PACKS.md)). The generic-obd2 pack ships
[`triggers/wot-pull.json`](../packs/generic-obd2/triggers/wot-pull.json).

```sh
# Only the pulls, 3 s before to 5 s after, one file each: pulls-001.csv, pulls-002.csv
opentune obd read --driver elm327obd --rounds 0 --interval-ms 100 --reconnect \
  --out pulls.csv --trigger packs/generic-obd2/triggers/wot-pull.json --split 0C 11 05
opentune log --def logger.xml --driver <id> --fast --rounds 0 \
  --out drive.csv --trigger my-triggers.json
```

| Flag | Meaning |
|---|---|
| `--trigger FILE` | apply the trigger file to the CSV (`log`: file or stdout; `obd read`: needs `--out`) |
| `--split` | with `--out run.csv`: one file per recording window, `run-001.csv`, `run-002.csv` and so on (`run.csv` itself is not written) |

```json
{
  "format": "opentune-triggers",
  "version": 1,
  "id": "wot-pull",
  "max_pre_rows": 20000,
  "triggers": [
    {
      "name": "wot-pull",
      "action": "record",
      "when": { "all": [
        { "channel": ["obd01_0c_rpm", "rpm"], "op": ">", "value": 3000, "units": "rpm" },
        { "channel": ["obd01_11_throttle", "throttle_pos"], "op": ">", "value": 90, "units": "%" }
      ] },
      "arm_ms": 300, "release_ms": 1000, "pre_ms": 3000, "post_ms": 5000
    },
    {
      "name": "coolant-hot",
      "action": "mark",
      "when": { "channel": ["obd01_05_coolant_temp", "coolant_temp"], "op": ">", "value": 105, "units": "°C" },
      "arm_ms": 1000, "release_ms": 5000
    }
  ]
}
```

**Conditions.** A comparison is `channel op value [units]` with `op` one of
`>`, `>=`, `<`, `<=`, `==`, `!=`; `{"all": [...]}` and `{"any": [...]}`
combine them (nesting up to 8 levels). `channel` is the row's `param_id`, or
a list of alternatives of which the first one present in the round is used,
so one file works with a logger definition's ids and `obd read`'s keys
(`rpm`, `throttle_pos`, `coolant_temp`; `opentune obd pids` lists them all).
With `units`, a channel logged in other units is converted when the
conversion is known (°C/°F/K, km/h/mph/m/s, kPa/hPa/mbar/bar/psi/Pa) and
spellings of the same unit are folded (`1/min` = `rpm`, `° BTDC` = `deg`); a
channel logged without units is taken to be in the comparison's units.

**Unknown is not false.** A comparison whose channel is missing from the
round, has no value (a failed read, a dead round, a text channel) or is
logged in units that cannot be converted is *unknown*. `all` is false if any
member is false, else unknown if any member is; `any` is true if any member
is true, else unknown if any member is. A link-event round (the `link`
marker rows above) makes everything unknown. Unknown never fires a trigger,
never releases one, and breaks a running hold time: a pull that loses the
link for a few seconds stays one window, and a channel that comes back after
a gap has to be true for the whole `arm_ms` again. A unit mismatch is
reported once on stderr; a channel the log does not carry at all is warned
about before logging starts.

**Hold times (hysteresis).** A trigger *fires* when its condition has been
true for `arm_ms` (observed continuously; 0 = the first true round) and
*releases* when it has been false for `release_ms`. In between it does not
fire again, so a `mark` trigger writes one marker per episode and a noisy
threshold does not chop a pull into pieces.

**Actions.**

- `record` gates the log: only **windows** are written. A window opens when
  a record trigger fires, with up to `pre_ms` of history before the fire
  (from a ring buffer bounded to `max_pre_rows` CSV rows; older history is
  dropped even if `pre_ms` reaches further), and stays open until the
  trigger releases plus `post_ms`. A trigger that fires again inside the
  post window extends the same window. All record/split triggers share one
  gate, so windows never overlap: a window opened by one trigger and
  extended by another is still one window, and each row is written at most
  once.
- `split` does the same and writes every window to its own file (as
  `--split` does for `record`).
- `mark` writes a marker row when it fires and gates nothing (`pre_ms` and
  `post_ms` are not allowed on it). In a gated log a mark row is written
  only if a window covers it, like any other row.

**Marker rows.** Like the link rows, trigger rows carry no value:
`param_id` `trigger`, the trigger name in `name`, the detail in `note`, and
`start`, `end` or `mark` in `text`. Every window begins with a `start` row
(timestamped with the window's first row; the note says which trigger fired
and how far into the window) and ends with an `end` row (duration, row
count, and `log ended` if the log stopped while the window was open). Both
CSV layouts keep their columns; the Log viewer opens a gated file like any
Live log and lists the trigger rows as a text channel.

**Summary.** stderr reports the fires per trigger, each window's duration,
row count and file, the marks and the rows dropped outside windows:

```text
trigger: 2 fire(s) (wot-pull 2, coolant-hot 0); 2 window(s); 0 mark(s); 3412 row(s) outside windows dropped
  window 1: 11.840 s, 312 row(s), wot-pull -> pulls-001.csv
  window 2: 9.705 s, 258 row(s), wot-pull -> pulls-002.csv
```

**Library.** `opentune_logdef::trigger`: `TriggerSet` (the file, with
`validate`), `TriggerEngine` (one round at a time: `push(ts, samples, rows)`
returns rows, markers and window open/close events; pure, no clock, no I/O)
and `TriggerCsvWriter`, a `std::io::Write` layer that runs the engine over
either Live-log CSV layout and writes one stream or one stream per window.
The CLI puts it in front of the file; `run_csv_resilient` and the `obd read`
loop are unchanged. A round is evaluated when the next one starts, so the
writer's `finish` must be called at the end.

## GPS channels

An optional GPS/GNSS receiver adds position and true ground speed to a log
(`opentune-gps`, parsed by OpenTune itself — no gpsd). A u-blox style
receiver enumerates as a plain USB CDC-ACM serial port (`/dev/ttyACM0` on
Linux, `COMx` on Windows) and streams NMEA 0183 at 1–10 Hz:

```sh
opentune gps --list-ports                       # likely GNSS receivers first (u-blox VID 0x1546)
opentune gps /dev/ttyACM0 --count 5             # one CSV row per second + NMEA counters
opentune gps drive.nmea                         # replay a recorded NMEA file, one row per epoch
opentune log --def logger.xml --driver <id> --rounds 500 --out run.csv \
  --gps /dev/ttyACM0 [--gps-baud 9600] [--gps-speed-param vss] [IDS…]
```

`--gps` takes a serial port or a recorded NMEA file. A file is **replayed one
epoch per logged sample** (an epoch is every sentence sharing one UTC time),
so the same ECU log + NMEA recording always produce the same CSV — useful for
tests and offline analysis. A port is read live; the log waits up to 2.5 s
for its first sentence before the first sample. USB CDC-ACM ignores the baud
rate; a UART receiver needs its configured rate (`--gps-baud`, default 9600).

Each round of the CSV gets these rows after the ECU channels (same
`round,param_id,name,raw,value,units,note,text,ts` columns):

| `param_id` | Value | Units |
|---|---|---|
| `gps_lat`, `gps_lon` | signed decimal degrees (north / east positive) | `deg` |
| `gps_alt_m` | altitude above mean sea level (GGA) | `m` |
| `gps_speed_kmh` | speed over ground (VTG km/h, else RMC/VTG knots × 1.852) | `km/h` |
| `gps_course_deg` | course over ground, true | `deg` |
| `gps_sats` | satellites used (GGA) | `count` |
| `gps_fix` | GGA fix quality: 0 none, 1 GPS, 2 DGPS, 4 RTK fixed, 5 RTK float, 6 estimated; 1 from an RMC `A` status when no GGA is sent | |
| `gps_hdop` | horizontal dilution of precision | |
| `speed_error_kmh` | vehicle speed − GPS speed (only when the log has a vehicle-speed channel) | `km/h` |

**Good fix, stale, flagged.** Position, altitude, speed and course are filled
only while the fix is *good*: a position from RMC status `A` / GGA quality
1–5 (estimated, manual and simulated fixes do not count), mode indicator not
`N`/`E`, updated within the staleness limit (2 s: a 1 Hz receiver has missed a
whole epoch). `gps_sats`, `gps_fix` and `gps_hdop` stay filled while the
receiver is talking without a fix. When nothing usable is known the value is
empty and the row's `note` says why — `no NMEA received yet`,
`stale: no NMEA for 3.0 s`, `no fix (GGA quality 0 RMC status V mode N)`,
`replay ended after 20 epoch(s)`. A GPS problem never stops the ECU log.

**Speed error.** The vehicle-speed channel is `--gps-speed-param`, else the
first OBD-II PID `0x0D` channel, else an id such as `vss` / `speed` /
`vehicle_speed`, else a name containing "vehicle speed". Its value is
converted from `km/h`, `mph` or `m/s` (a unit-less PID `0x0D` is km/h by
definition). `speed_error_kmh` is filled only with a good fix and a GPS speed
of at least 5 km/h (below that GPS speed noise dominates); positive means
the vehicle reads high, as speedometers usually do.

**Parser.** `$--RMC`, `$--GGA` and `$--VTG` from any talker (`GP`, `GN`,
`GL`, `GA`, `GB`/`BD`, …), including the NMEA 1.x `VTG` without unit
letters. The `*hh` checksum is required and verified. Other sentences (`GSV`,
`GSA`, `GLL`, `TXT`, proprietary `$P…`) and binary UBX traffic are skipped.
Every rejected line is counted, never fatal; the CLI prints the counters at
the end:

| Counter | Line |
|---|---|
| `bad_checksum` | `*hh` does not match |
| `partial` | no `*hh` (cut off mid-sentence, e.g. at port open) |
| `malformed` | an RMC/GGA/VTG field that does not parse (time, coordinates, status, units, …), a bad address or checksum field |
| `noise` | does not start with `$` |
| `overlong` | longer than 160 bytes (dropped) |
| `ignored` | valid sentence of another type |

Serial reads arrive in arbitrary chunks; lines are reassembled across reads,
and a `$` inside a line starts a new one (the cut-off part counts as
`partial`). The reader runs on a background thread with a 200 ms read
timeout and keeps only the latest merged fix, so the log loop never waits on
the receiver.

**Desktop.** The *Live log* and *Dashboard* panels (Bench tier) carry an
optional GPS card: pick a port (or an NMEA file, replayed in a loop), Open,
and the tiles show position, speed, altitude, course, satellites, fix, HDOP
and the speed error against the panel's vehicle speed (Live log: its
vehicle-speed channel; Dashboard: OBD `speed`). While a GPS source is open
the Live log CSV gets the `gps_*` rows too. Backend: `gps_ports`,
`gps_open`, `gps_fix`, `gps_close` (`desktop/src-tauri/src/feat_gps.rs`).

**Validation state:** synthetic-tested — hand-made NMEA fixtures with valid
checksums (a made-up route in open water; multi-talker, NMEA 1.x VTG, broken
checksums, partial / overlong / noise lines, fix loss and recovery), a
chunked serial-like reader, and replay into `LiveSession` against the UDS
simulator and through `opentune log --gps`. Not yet tested with a real
receiver or in a car.

## Running over your own transport

The logging engines need only one request/response seam,
`opentune_core::RawDiag` (`request(&[u8]) -> Result<Vec<u8>, String>`,
returning negative responses as raw `7F` bytes). An application that brings
its own adapter has two ways in:

- **Raw CAN frames** — implement `opentune_core::protocol::CanLink`
  (`send_frame(id, data)`, `recv_frame(timeout_ms)` returning
  `Error::Timeout` when nothing arrives) and let core `IsoTp` do ISO 15765-2:

  ```rust
  let mut tp = IsoTp::new(Box::new(my_link), 0x7E0, 0x7E8); // tx, rx
  let diag = OwnedDiag::new(Box::new(tp), 1000);            // owning RawDiag
  ```

  For OBD-II functional requests use `tx` `0x7DF` and set
  `tp.fc_tx_id = Some(0x7E0)`: a multi-frame reply needs its flow control
  sent physically (ISO 15765-4); an ECU ignores one on `0x7DF`.
- **A whole driver** — implement `opentune_core::Interface` and return your
  transport from `diag()`; `InterfaceDiag::new(Box::new(driver), timeout_ms)`
  owns it and closes it when dropped.

Then drive any engine over it. A resilient log takes the first link and a
re-open hook that builds a fresh one:

```rust
use opentune_core::resilient::{ReconnectPolicy, RunLimits, RunOptions, SystemClock};

let mut session = LiveSession::new(&def, &ids, FastLogConfig::fast())?;
let mut link: Box<dyn RawDiag> = Box::new(open_my_link()?);
session.start(link.as_mut());
let clock = SystemClock::new();
let opts = RunOptions {
    limits: RunLimits { rounds: None, duration_ms: Some(600_000), interval_ms: 100 },
    policy: ReconnectPolicy::reconnecting(),     // dead_after 3, 500 ms .. 10 s, never give up
    stop: Some(&stop_flag),                      // an AtomicBool, e.g. set by Ctrl-C
    clock: &clock,
};
let mut reopen = || open_my_link().map(|l| Box::new(l) as Box<dyn RawDiag>).map_err(|e| e.to_string());
let report = session.run_csv_resilient(link, &mut reopen, &opts, None, &mut csv_file)?;
```

`report` (`RunReport`) counts rounds, dead rounds, drops, reconnects, attempts
and downtime. For a different output, implement `opentune_core::resilient::LogJob`
(`round`, `event`, `reopened`, `stop`) and call `run_resilient` directly — the
CLI's OBD-II recorder does that around `ObdSession`. A transport error that
means the session is certainly gone should be returned as
`Error::AdapterReset` (the owning wrappers then prefix it with
`resilient::LINK_LOST_PREFIX`, and the loop re-opens without waiting for
more dead rounds); `resilient::is_link_lost` is the classification.

**What to expect on a quiet bus.** The diagnostic CAN behind a gateway is
silent until queried: no broadcast frames, nothing between requests. That is
normal — `recv_frame` timing out while idle is not an error until a request
was sent and its reply timed out, and the loop only counts a round as dead
when *every* request in it got no response. On a busy bus `IsoTp` skips
frames with other ids while waiting for `rx_id`, and the request timeout
bounds the whole wait (foreign traffic does not restart it). Several ECUs may
answer a functional request; each `IsoTp` takes the reply on its `rx_id` and
ignores the rest. An unsupported OBD-II PID on CAN is simply silent, which
looks like a timeout; `ObdSession` learns and skips such PIDs, and a silent
*tick* (nothing answered at all) teaches it nothing. Keep `timeout_ms` above
the ECU's P2 time (50 ms is typical; slow ECUs reply `7F <sid> 78`, which
`IsoTp` waits out up to `max_response_pending` times).

**Testing your integration.** `opentune_core::protocol::faultlink` is a
simulated ISO-TP bus to run all of this against without hardware: `SimNode`s
answer through any closure (or a `RawDiag` simulator via `raw_responder`),
and `FaultBus` injects foreign-id noise, several `0x7DF` responders,
`0x78` delays, and faults — dead for the next N calls or until re-opened,
failing as I/O errors, timeouts or silently, plus refused re-opens. With
`VirtualClock` the loop's sleeps and the link's timeouts take no wall-clock
time and can be counted. `crates/opentune-logdef/tests/resilient.rs` is a
worked example.

**Validation state:** synthetic-tested only — the loop, `IsoTp` and
`ObdSession` over the fault-injecting bus with the UDS and OBD simulators
(drop → reconnect → re-define → resume, recovery without reopen, give-up,
no hot spin, CSVs opened by the Log viewer). Not yet tested by unplugging a
real adapter mid-drive.

## Backend API for the desktop panel

The desktop **Live log** panel's sessions (*Fast (0x2C)*, an SSM transport, or
*Reconnect*) use this API (`desktop/src-tauri/src/feat_logging.rs`:
`log_start` / `log_tick` / `log_stop`, synthetic-tested, thin glue over the
shared engine in `crates/opentune-hub/src/livelog.rs`). It drives
`opentune_logdef::LiveSession` as one long adapter operation on the app's
adapter hub, every request through that operation's own hub `Session`:

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
(`"fast-2c"` | `"polling"` | `"ssm-a8"`), `reason` (why polling), `dynamic_dids`
(`{did, record_len, channels}`), `polled`, `notes`, `redefines`, `ticks`,
`setup_requests` and `tick_requests` (`requests_per_tick()` for display).

**Same link semantics as the CLI.** The session job is a
`resilient::LogJob` on `run_resilient`, one round per `log_tick` (the panel
paces the loop; a clock that serves the command channel while the loop backs
off answers ticks, Stop and the idle check meanwhile). Dead ticks are counted
at the transport (`resilient::Liveness`): a tick of negative responses is
alive. Five dead ticks in a row (`DEAD_TICKS`), or an adapter reset (the error
carries `LINK_LOST_PREFIX`; `CoreDiag` now adds it like the owning wrappers),
lose the link. `reconnect` (opt-in, `live_policy`) then closes and re-opens the
adapter through the hub with the core backoff, runs `LiveSession::start` again
(dynamic DIDs re-defined) and gives up after 5 minutes down or after three
re-opens in a row in which nothing answered; without it the session stops,
as before. Each link event is a marker sample (`link_event_sample`) in the
next reply — the tick that lost the link carries its `lost` marker — and
every reply has a `link` view: `{ state: up|silent|lost|reconnecting|
reconnected|gave-up, reconnect, dead_ticks, dead_after, attempt, retry_in_ms,
error, drops, reconnects, downtime_ms, last_event }`. While the link is down,
ticks get the link view and the markers so far, no samples. The idle stop
(15 s without a tick) is unchanged.

**SSM.** `log_start` takes `transport` (`uds` | `ssm` | `ssm-can`),
`ssm_target` (`ecu` | `tcu`) and `ssm_max_packet`: the key gets the CLI's
driver config (`ssm::SsmWire::configure`: K-line address, 4800 baud,
`init=none`; or the module's ISO-TP ids), and on the worker
`opentune_logdef::ssm::prepare` identifies the module and selects the
parameters — the same code `opentune log --transport ssm|ssm-can` runs. The
reply's `ssm` is `{ target, wire, ssm_id, rom_id, capability_bytes, logged,
notes, not_logged }` and `params` the channels actually logged. The
`ssmsim` pseudo-driver (the synthetic module, `SsmSim` as an `Interface`) is
opened by the desktop's adapter opener without hardware.

**Commands.** `log_start` → `{ status, channels, params, ssm, target, link }`;
`log_tick` → `{ ts, samples, status, rate, running, ended, target, link }`;
`log_stop` → `{ running, status, ended, rate, target, link, events, ssm }`
(`events`: markers no tick picked up). The stateless `log_poll_once` returns
`link: { state: up|silent|lost, error }` and re-opens the adapter on the next
poll when nothing answered or the adapter reset; the Dashboard's `obd_read`
does the same (plus `errors.link` on a reset).
