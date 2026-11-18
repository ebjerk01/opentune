# Aftermarket ECUs (Speeduino, rusEFI, MegaSquirt)

Open aftermarket engine controllers describe themselves with a
**TunerStudio-format INI definition** and keep their calibration in **pages**
that a tuning tool reads, edits and burns over a serial link. OpenTune speaks
that format and those protocols directly, so the same tune files (`.msq`) and
INIs the firmware projects publish work here.

| Piece | Crate | State |
|---|---|---|
| INI parser, constant ↔ page-byte mapping, realtime decoding | `opentune-tsini` | **real-image-tested** (82 public INIs, 3 public `.msq` tunes, see below) |
| `.msq` read/write | `opentune-tsini::msq` | **real-image-tested** (lossless round trip of public tunes) |
| Serial protocol, verified write, burn | `opentune-tsproto` | **synthetic-tested** (simulated ECU only; no bench test yet) |
| CLI `opentune ts …` | `opentune-cli` | **synthetic-tested** |
| Desktop panel | — | not started; backend API proposed [below](#backend-api-for-a-desktop-panel) |

The validation words are defined in [DEVELOPMENT.md](DEVELOPMENT.md#7-validation-state-vocabulary).
Nothing here has talked to a real ECU yet: treat the online commands as
experimental until someone bench-tests them.

## Safety first

These ECUs run engines. A wrong fuel, ignition or boost value can destroy an
engine in seconds. OpenTune therefore:

- **never writes unless asked**: `opentune ts write` is a dry run that lists
  the changes; it writes only with `--confirm`. In the library, every method
  that changes the ECU needs a `WriteAuth`, which a front end creates only
  after the user confirmed;
- **reads and saves the current tune first**: before any write, every page is
  read and saved as a `.msq` backup *and* a raw `.pages.bin` (lossless even
  for constants a `.msq` cannot express);
- **refuses a mismatch**: the ECU's signature must match the INI's, and the
  tune's signature must match the INI's (`--force-signature` overrides, for
  people who know why);
- **writes only what changed**, in `blockingFactor`-sized chunks;
- **verifies before burning**: every written page is read back and compared;
  any difference stops the write *before anything is burned* (the change is
  then only in RAM and is gone at the next power cycle);
- **burns, then checks**: pages are burned only after all verified, and the
  ECU's page CRC32 is compared with the target when the INI has a
  `crc32CheckCommand`;
- refuses values that do not fit the storage type instead of clamping them,
  and refuses a tune whose entries could not all be applied unless
  `--allow-partial`.

Practical rules: write with the engine **off** and ignition on, on a stable
power supply; keep the backup; start the engine only after checking the
changed values; know how to restore (write the backup `.msq`).

## What is supported

### INI definitions (`opentune-tsini`)

- `[MegaTune]` / `[TunerStudio]`: `signature`, `queryCommand`, `versionInfo`,
  `MTversion`, `iniSpecVersion`, `messageEnvelopeFormat`.
- `[Constants]`: `endianness`, `nPages`, `pageSize`, `pageIdentifier`,
  `pageReadCommand`, `pageValueWrite`, `pageChunkWrite`, `burnCommand`,
  `crc32CheckCommand` (per-page lists; a single value applies to every page;
  `""` means none), `blockingFactor`, `tableBlockingFactor`,
  `pageActivationDelay`, `interWriteDelay`, `blockReadTimeout`,
  `delayAfterPortOpen`, `writeBlocks`, `tsWriteBlocks`; `page = n`; constants
  of class `scalar`, `array` (`[n]`, `[cols x rows]`, row-major), `bits`
  (`[lo:hi]`, option lists with `$define` references, `INVALID`, synthesised
  labels, the older `[4:7+1]` form) and `string, ASCII`; types
  `U08 S08 U16 S16 U32 S32 F32 F64`; `lastOffset` (the previous constant's
  offset — an alias of the same bytes); `{ expression }` scales, translates
  and limits; `noMsqSave`.
- `[PcVariables]` (same forms, no offset).
- `[OutputChannels]`: `ochGetCommand`, `ochBlockSize`, `scalar` and `bits`
  channels, `{ expression }` channels with optional units; `array` channels
  are listed but not decoded.
- `[TableEditor]` (`table`, `xBins`/`yBins` with their operating-point
  channel, `zBins`, labels, help), `[CurveEditor]` (`curve`, `xBins`, one or
  more `yBins`, axes, labels), `[Menu]` (`menuDialog`, `menu`, `subMenu`,
  `groupMenu`/`groupChildMenu`, conditions), `[SettingGroups]`.
- Preprocessor: `#if`/`#ifdef`/`#ifndef`/`#elif`/`#else`/`#endif` (nested,
  `!`, `&&`, `||`), `#set`/`#unset`, `#define`. Conditions come from the file,
  from a tune's saved `<settings>`, and from `--set`/`--unset` (which win).
- Robustness: `;` comments (quotes respected), `\` line continuation,
  UTF-8 or ISO-8859-1, and the hand-editing slips TunerStudio tolerates
  (a missing comma after a type, offset, bracket or quoted string, an empty
  field before an offset, an offset fused with its bit range). Nothing is
  fatal: unparseable lines become warnings with line numbers, classified as
  `invalid` (a modelled line that could not be parsed), `reference`
  (a missing or inconsistent name) or `ignored` (a section or key OpenTune
  does not model, e.g. `[GaugeConfigurations]`, `[Datalog]`, `[UserDefined]`).

Scale and value conversion is `user = (raw + translate) × scale` and back,
rounded to the nearest raw value for integer types. Expressions in scales may
use PC variables, constants and expression output channels that depend only
on constants (Speeduino's `idleRes`). The expression evaluator handles
arithmetic, comparison, logic, bit operators, `?:` and `abs round floor ceil
sqrt exp log log10 sin cos tan pow min max isNaN`; anything else (table
lookups, `smoothBasic`, `bitStringValue`, string functions, …) is reported
as *unsupported* for that channel or constant, never guessed.

**Real-file results** (files kept outside the repo; reproduce with
`cargo run -p opentune-tsini --example ts_survey -- <dir> --ini <fallback.ini>`):
82 of 82 INIs parse with no invalid line — 8 Speeduino (201911 … 202504-dev),
6 rusEFI board INIs (2026-11 master), 68 MegaSquirt (2 MS1, 2 MS2,
29 MS2/Extra, 35 MS3, from the msqur project's collection). Three public
Speeduino tunes load through the INI of their own firmware version:
813/814 and 847/866 entries applied (the rest are names the INI no longer
has, and one option label that the 202501 INI itself spells with a stray
period); every file survives `.msq` → XML → `.msq` unchanged and
pages → `.msq` → pages byte-identical.

### `.msq` tunes

Read and written as XML (`bibliography`, `versionInfo`, `page` with
`constant`/`pcVariable`, `settings`; other elements such as `userComments`
are kept verbatim). Values are written with as many decimals as needed for
the raw value to read back exactly. A bit field whose label is ambiguous (two
options with the same label) is written as its number. Loading through an INI
reports unknown names, entries it could not apply (with the reason) and
values outside the INI's limits.

### Protocols (`opentune-tsproto`)

The command bytes always come from the INI templates: `\xNN` / octal escapes,
`$tsCanId` (the `--can-id`, default 0), `%2i` page identifier, `%2o`/`%2c`
offset/count in the INI's endianness, `%v` value bytes.

| Framing | Used by | Wire format |
|---|---|---|
| **msEnvelope_1.0** (CRC32) | Speeduino (new comms, 2021+), rusEFI, MS3, MS2/Extra 3.3+ | request `[len u16 BE][payload][CRC32(payload) u32 BE]`; response `[len u16 BE][status][data][CRC32(status+data) u32 BE]` |
| **plain** | MS1/MS2 legacy, Speeduino before msEnvelope / compatibility mode | raw command bytes; raw response of the length the request implies; writes and burns are not acknowledged |

Status bytes (msEnvelope): `0x00` OK, `0x01` realtime OK, `0x02` page OK,
`0x04` burn OK; `0x80` underrun/timeout, `0x81` overrun, `0x82` CRC failure,
`0x83` unrecognised command, `0x84` out of range, `0x85` busy, `0x8D`
framing error. CRC is CRC-32/IEEE (zlib). The page CRC32 reply is big-endian.

| Operation | INI key | Notes |
|---|---|---|
| signature | `queryCommand` (`Q`, rusEFI `S`) | trailing NUL / spaces ignored |
| read page | `pageReadCommand` | chunks of `blockingFactor` when it has `%o`/`%c`, else the whole page |
| write | `pageChunkWrite`, else `pageValueWrite` | each request ≤ `blockingFactor` bytes; value writes without a count send one byte each |
| burn | `burnCommand` | waits `pageActivationDelay`; an empty command means a RAM-only page |
| page CRC32 | `crc32CheckCommand` | used after burning |
| realtime | `ochGetCommand`, `ochBlockSize` | chunked like reads; decoded with `[OutputChannels]` |

Timeouts come from `blockReadTimeout` (override `--timeout-ms`). A timeout,
short read, response CRC error or a `0x80`/`0x82`/`0x85`/`0x8D` status is
retried twice after flushing input; every request OpenTune sends is
idempotent (a read, or the same bytes written to the same place), and any
write is verified by read-back before a burn. Other statuses fail at once.
`delayAfterPortOpen` is honoured after opening a real port (Arduino-based
Speeduino boards reset when the port opens).

The protocol facts come from the open firmware sources: Speeduino
`speeduino/comms.cpp` (envelope, status codes, `p`/`M`/`b`/`d`/`r`
commands, big-endian length and CRC, little-endian offsets) and rusEFI
`firmware/console/binary/tunerstudio*.cpp` plus its generated constants
(`S`/`R`/`C`/`B`/`k`/`O`, response codes), and the INIs themselves.

Not implemented yet: rusEFI's scattered high-speed output read, the
`tableCrcCommand`, tooth/composite loggers, SD-card log download, CAN
pass-through to other `tsCanId`s (the id is sent but nothing routes),
MS1's single-byte numeric signatures, the legacy `pageActivate` command, and
`[ControllerCommands]` buttons.

## CLI

```sh
# Offline
opentune ts ini info   speeduino.ini [--set LAMBDA] [--warnings] [--json]
opentune ts ini tables speeduino.ini
opentune ts msq show   tune.msq --ini speeduino.ini [--name veTable]
opentune ts msq diff   old.msq new.msq --ini speeduino.ini

# Online (read-only)
opentune ts read --port /dev/ttyACM0 --ini speeduino.ini --out current.msq
opentune ts log  --port /dev/ttyACM0 --ini speeduino.ini --channels rpm,map,coolant \
                 --rounds 600 --interval-ms 100 --out run.csv

# Write: dry run first, then --confirm
opentune ts write --port /dev/ttyACM0 --ini speeduino.ini new.msq
opentune ts write --port /dev/ttyACM0 --ini speeduino.ini new.msq --confirm

# No hardware: the simulated ECU answers the INI's own commands
opentune ts read --port sim --ini samples/ts/opentune-demo.ini --out demo.msq
opentune ts write --port sim --sim-tune demo.msq --ini samples/ts/opentune-demo.ini edited.msq --confirm
```

`ts log` writes the Live-log CSV (`ts,param_id,name,raw,value,units,note`,
one row per channel per sample, `ts` in Unix ms) that the desktop log viewer
opens; a channel whose expression is unsupported has an empty value and the
reason in `note`. `samples/ts/` holds two synthetic INIs (a Speeduino-like
envelope one and a MegaSquirt-like plain one) for trying everything offline.

## Backend API for a desktop panel

The desktop panel comes after the desktop module split. It should be a thin
shell over the two crates; the CLI (`crates/opentune-cli/src/ts_cmd.rs`) is
the reference use. Proposed Tauri commands (all serial I/O as `async fn`,
see CLAUDE.md; tier **`bench`** until vehicle-tested, the offline INI/`.msq`
viewer may be `public`):

| Command | Backend | Returns |
|---|---|---|
| `ts_load_ini(path, set, unset)` | `opentune_tsini::load_ini` | the `Ini` (serde JSON: pages, constants, channels, tables, curves, menus, warnings) |
| `ts_load_msq(path)` / `ts_save_msq(path)` | `Msq::load` + `Msq::to_tune` / `Msq::from_tune` + `to_xml` | `MsqReport` (unknown, skipped, out-of-range) |
| `ts_get(name)` / `ts_set(name, value)` | `Tune::get` / `Tune::set` on the panel's working tune | `Value` (scalar, bits index, row-major array, text) |
| `ts_table(id)` | `Ini::table` + `Tune::get` of x/y/z | axes, cells, units, operating-point channels |
| `ts_connect(port \| "sim", baud)` | `SerialPort::open` / `SimEcu::new` + `Client::check_signature` | `SignatureCheck` |
| `ts_read()` | `Client::read_tune` (progress events) | the ECU tune (becomes backup + working tune) |
| `ts_plan_write()` | `plan_write(current, working)` + per-constant diff | changed pages/bytes/constants for the confirm dialog |
| `ts_write(confirmed, burn)` | save backup, `WriteAuth::user_confirmed()`, `write_tune` (progress by `WriteStage`) | `WriteReport` |
| `ts_poll()` | `Client::read_realtime` + `decode_channels` | `Vec<ChannelSample>` for gauges, table cursors and the Live log |

State: keep the `Ini` in an `Arc` and the port as `Box<dyn Port>` in the
panel's state; a `Client` borrows the `Ini`, so build one per operation
(`Client::new(port, &ini, opts)` … `client.into_port()`), and serialise
access so a poll never overlaps a write. Show the signature check, the
diff and the backup path in the confirm dialog; never call `ts_write` without
an explicit user action.
