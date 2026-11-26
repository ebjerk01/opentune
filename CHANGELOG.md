# Changelog

All notable changes to OpenTune. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

OpenTune is **early alpha**. Unless an entry says otherwise, a feature is tested
offline only (unit tests, simulators, synthetic fixtures) and is **not yet proven
on a real car or adapter**. Car-facing panels stay in the desktop app's Bench
section (alpha/bench builds) until they are vehicle-tested.

## [Unreleased]

### Added
- **OpenPort 2.0 K-line: request length form and five-baud W4.** New
  `length=auto|separate|format` option: `separate` always sends the additional
  length byte (`80 tgt src len …`), an override for an ECU that wants one
  particular form. A five-baud init now writes W4 = 40 ms
  unless `w4_ms` is given: the cable's 50 ms sits on the late edge of the
  ISO 14230 25–50 ms window, where a real ECU stayed silent.
- **EDC15C6 K-line address is `0x12`** (`ecudefs/bosch-edc15c6.json`, was an
  unsourced `0x01`): own-unit identify (`1A 86`) + own-dump analysis show a
  five-baud wake at `0x12` (key bytes `EF 8F`) and the W4 window; the ECU
  accepts both request length forms, and identify is keyless (validated on
  hardware with this driver). Documented in docs/DRIVERS.md §3l.
- **OpenPort 2.0 K-line defaults to host checksums** (`checksum=host`,
  `ISO9141_NO_CHECKSUM`): on a real cable the echo showed the appended sum on
  the wire, while cable-checksum mode (`checksum=cable`) is not yet confirmed
  there. In ISO 9141-2 OBD mode every responder's source address is logged
  (the first answer is used).
- **OpenPort 2.0 K-line request addressing and keyword decoding
  (offline-tested):** after the init the key bytes decide what the link
  speaks. KB2 `8F` is KWP2000: requests go to the `target` option, else the
  five-baud address after a five-baud init, else `--tx-id`, functional
  (`0xC0|len`) for the OBD address `33` and physical otherwise
  (`addressing=physical|functional|none` overrides), and KB1's header bits
  (length in format byte / length byte, with / without address) pick the
  header form or refuse one it rules out. Key bytes `08 08` / `94 94` mean
  the ECU woke as ISO 9141-2 OBD: only OBD read modes go out, with the
  `68 6A F1` header, KWP2000 (identify's `1A`) is refused unsent, and the
  ECU address from the first `48 6B` answer is reported. Other key bytes are
  refused unless a header is forced. New `session=<hex>` option sends
  startDiagnosticSession right after the init (programming sessions
  refused); `opentune obd` takes `--tx-id` and `--opt` for K-line. The
  forms are listed on the firmware line (docs/DRIVERS.md §3l).
- **CLI diagnostic logging** (off by default): `OPENTUNE_LOG` (or `RUST_LOG`)
  turns on stderr logging, e.g. `OPENTUNE_LOG=openport2=trace`. The OpenPort
  2.0 driver logs every command line it sends and every reply and record it
  receives at `trace`, for bringing up a new ECU or cable.
- **OpenPort 2.0 K-line (read-only, offline-tested):** `--driver openport2
  --bus kline` (or `iso9141`, or an ECU definition with a K-line protocol)
  opens the cable's ISO 14230 / ISO 9141 channel, wakes the ECU with the
  cable's fast or five-baud init (`init=fast|5baud|none`, key bytes
  reported), sets P1/P3/P4, W0–W5, TIdle, TiniL, TWUP and FIVE_BAUD_MOD on
  request (range-checked), and runs KWP2000 framing as a `DiagTransport`, so
  `identify` and the core KWP2000 client work over it as over `kkl`. A
  read-only allow-list refuses every non-read service before it is sent;
  writes are refused. Tested against a synthetic KWP2000 ECU in the offline
  fake cable; hardware validation pending (docs/DRIVERS.md §3l).
- **`opentune candump --driver openport2`:** receive-only raw-CAN capture
  (pass-all filter, 11- and 29-bit ids); the monitor has no transmit path and
  its session refuses every bus-writing command unsent.
- **Checksums: Subaru / Mitsubishi TCU balance-word sums** —
  `subaru-tcu-m32r` (Hitachi M32R), `subaru-tcu-sh7055` (Denso SH7055) and
  `subaru-tcu-mh8104` (Mitsubishi MH8104 CVT), reimplemented from FastECU
  (GPL-3.0). Synthetic-tested only: reported *consistent*, and an edited image
  is corrected only against your own validating stock dump.
- **Checksums: VAG Simos ECM3 calibration-monitor sum** (`ecm3`, alias
  `simos-ecm3`), reimplemented from VW_Flash (BSD-2-Clause) for extracted
  Simos 8/10/12/16/18 CAL blocks. Verify-first: reported *consistent*,
  correction only against a validating stock CAL block of the same software;
  full reads are refused.
- **Checksums: Mercedes EGS52 / EGS53 (NAG 722.6 TCU)** (`egs52`, `egs53`):
  per-module descriptor table (`(N << 16) | crc16`, N inclusive 24-bit
  ranges, `0xFFFFFFFF` terminator, descriptor carved out of its ranges) with
  a reflected CRC-16/ARC (poly `0x8005`) seeded `0x7878` (EGS52) or `0xFFFF`
  (EGS53). Clean-room, derived from and validated on the owner's own images
  (34/34 modules over 17 images). Only documented layouts are accepted, and
  only when every descriptor validates structurally; a module is reported
  *recognised* when at least one module in the image matches. Match-first:
  `correct` writes only the low 16 bits (never N) while one module still
  matches; otherwise correct against the untouched stock read.
- Checksum vectors: the public DQ250-MQB trailer vector (`"123456789"` +
  `D9 C6 0B 34`) and the Haldex Gen5 header-window exclusion, per published
  block offset.

### Changed
- **One read-only allow-list for every read path.** The list of services a
  read-only path may send now lives in `opentune_core::protocol::readonly`;
  the desktop/native `ReadOnlyDiag` uses it directly and the OpenPort 2.0
  K-line guard derives its narrower K-line set from it (no upload, nothing in
  the KWP2000 manufacturer range, plus Start/StopCommunication). Tests pin
  both sets, so the safety rule cannot drift between them. No behaviour
  change.
- **`kkl` speaks KWP2000 by default.** Without `--opt layer=uds|kwp2000`
  the K-line driver now uses KWP2000 (as `openport2` on K-line does) unless an
  ECU definition with a K-line protocol names its own layer. Before,
  `opentune identify --driver kkl` inherited the CLI's generic UDS default and
  sent UDS requests down the K-line, which a KWP2000 ECU (EDC15, ME7, …) does
  not answer.
- **Checksums: an algorithm not yet validated on a real dump is never reported
  as "supported", and only writes against a proven stored value.** One rule
  for every algorithm, old and new:
  - `haldex` (Haldex Gen5), `dq250` (DQ250-MQB) and `zf8hp` (ZF 8HP CAL) now
    report *consistent* instead of *supported*, even when a stored value
    matches.
  - `haldex` corrects match-first, like `dq250`: standalone correction writes
    only when at least one located block's stored checksum already matches in
    that image. An image whose every block was edited (for example a single
    extracted block) is refused; correct it against your own untouched stock
    dump (correct with a reference), whose blocks must all validate.
  - `tcu-ccitt` no longer rewrites the last two bytes of any image, and
    `crc32-tail` no longer rewrites the last four: standalone correction
    refuses a mismatch, and correcting needs a same-length stock dump whose
    trailer validates. A genuine crc32-tail image that still matches keeps
    working; a normal ECU read is left untouched.
  - No user-selectable algorithm can now overwrite an image blindly: each is
    either validated on real dumps or gated as above. (Tests use a separate,
    test-only algorithm.)
  Each is promoted back once a real dump validates it.

## [1.0.0-beta.1] - 2026-11-20

The first beta of the **native desktop app**: every panel of the desktop app
rebuilt as a native egui/wgpu application (`native/`), over the same Rust
core. It is a pre-release: the on-car paths are still unproven on a vehicle
(see the warning in the README), and GitHub pre-releases are not offered by the
in-app updater, so install it by hand.

### Added
- **Native desktop app** (Linux AppImage, Windows zip; macOS builds from
  source with `native/packaging/build-macos.sh`). One window, no web view:
  - Calibration: flash dump (hex, maps, checksums), Analyze bin, map editor,
    compare, project, vault, log viewer with a map-hits overlay, Drive view
    (live tiles and log replay).
  - Vehicle (Bench section of alpha/bench builds until vehicle-tested):
    OBD dashboard, OBD inspection, live log, auto-scan, coding, adaptation,
    actuator tests, service functions, Haldex AWD (OpenHaldex, Gen4 read, CAN
    log decode), CAN capture (candump / CSV / ASC), aftermarket ECUs
    (TunerStudio INI / `.msq`; writing a tune stays in `opentune ts write`)
    and the read-only **Check my car** wizard.
  - Reference: ECU database, interface drivers with a probe, controller
    definitions with link checks and CAN-log decode, utilities (VIN, part
    number, DTC lookup, generic seed/key transforms), SA2 script viewer, and
    the in-app guide.
  - Every change to a module goes through a typed confirmation, a backup that
    must succeed first, and either a guarded write or an action that always
    restores the previous state; reads stay on an allow-list of read-only
    services.
  - Try every live panel offline with the built-in **virtual car**: pick
    "Virtual car (simulator)" as the cable and press **Connect**.
- **Desktop: "What's new" after an update.** The first start after an update
  shows that version's highlights once, from the changelog built into the app
  (nothing is fetched); a fresh install shows nothing. Updates from 0.7.x are
  recognised by the app's own saved UI state.
- Control-unit database: boot points accept `action: "connect"` (attach a
  BDM / JTAG probe to a header or pads), allowed only with `entry: bdm-jtag`
  and never as a pin `strap`.
- Control-unit database: `access.boot.possible` and `access.bench.possible`
  accept `"unknown"` (no public source documents a route) next to `true` /
  `false` (a source says there is none); plain booleans are still accepted.
  The five profiles that said `false` for "not documented" (EDC16U34,
  EDC17CP14, PCR2.1, DQ250 02E, Haldex Gen4) now say `"unknown"`, and the ECU
  database panel shows "unknown (not documented)".
- **Diagnostics log** (desktop, local only, on by default): crashes (Rust
  panics, with location; backtrace with `RUST_BACKTRACE`), uncaught errors in
  the app window, every command that fails (name + error text, never its
  arguments), the Debug session log and the startup steps go to
  `<app data dir>/logs/opentune.log` as JSON lines, 1 MiB per file with five
  rotated files. Each start writes a session header (version, channel, OS,
  WebKit version, DMA-BUF render decision). SecurityAccess seeds/keys are
  masked and hex payloads over 64 bytes cut before anything is written; file
  contents are never recorded; window reports are rate-limited. Debug panel
  card **Diagnostics log**: tail with a level filter, Open folder, Copy last
  200 lines, Clear. Nothing is sent anywhere. Docs:
  [DIAGNOSTICS.md](docs/DIAGNOSTICS.md).
- **Diagnostics upload** (desktop, capability only: the public app ships no
  endpoint and no token and sends nothing). A target comes from a plugin
  build's `DesktopConfig.diagnostics` or from `OPENTUNE_DIAG_ENDPOINT` /
  `OPENTUNE_DIAG_TOKEN` (manual mode only); `OPENTUNE_DIAG=off` turns every
  upload off. **Manual**: needs the per-user setting "Allow sending
  diagnostics" (off by default) and a click on Send after a preview of the
  exact redacted payload; after a crash the next start only offers to send.
  **Automatic** (plugin builds only): crash report on the next start,
  coalesced error reports, optional periodic session summary; one-time notice
  with Turn off, sidebar indicator "Diagnostics: auto → host", a global off
  switch, at most 1 upload per 10 min and 20 a day, a 2 MiB offline queue
  retried at the next slot. Payloads hold only diagnostics-log records, a
  session summary and the note (never images, datalogs, definitions, CAN
  captures or opened files), are redacted as a whole and capped at 2 MiB.
  HTTPS only (reqwest/rustls already used by the updater), Bearer token, 30 s
  timeout, no redirects; the token never appears in logs, previews, history
  or errors. Diagnostics card in About & updates and the Debug panel with the
  upload history (exact payload per upload). Docs:
  [DIAGNOSTICS.md](docs/DIAGNOSTICS.md#uploading-diagnostics).
- **`opentune_core::redact`**: replaces VINs (also inside hex dumps), part
  numbers (VAG, Bosch, Siemens, Mercedes), labelled serials, e-mail, paths,
  user / host names, IP and MAC addresses and SecurityAccess seeds/keys with
  markers (`<VIN>`, `<PART>`, `<PATH>/basename`, …).
- **ECU database access profiles**: each control unit can say how it is
  reached — OBD, bench, boot mode (entry, strap points, whether the case must
  be opened, reset timing) and the security access a read or write needs —
  each part with its own confidence and cited public sources. `read_methods.obd`
  gains `read`, boot pins can carry a `strap`, units a `flash_note`. The
  validator checks the profile against `read_methods`, requires sources,
  boot points and an "OpenTune ships no OEM key" statement; there is no field
  for key material. `opentune db validate --require-access` (CI) fails on any
  new unit without a profile (`database/units/.access-pending` lists the older
  units, and only shrinks). Filled: Bosch EDC15C6 (worked example), EDC15
  (VAG), EDC16U34, EDC16C3x, EDC17CP14, ME7.5, ME7.5.10, Siemens MS41/MS42/MS43,
  PCR2.1, Mercedes EGS52, DQ250 02E, DQ250 MQB, DQ200, DQ381, DL382, ZF 8HP Gen1 and Haldex
  Gen4/Gen5, each from public sources with stated licences; where nobody
  documents a route the profile says "unknown". The ECU database panel shows an
  **Access** card. Docs: [UNIT-ACCESS-PROFILE.md](docs/UNIT-ACCESS-PROFILE.md).
- **Bench wiring diagrams**, drawn from the ECU database: every unit with a
  pinout gets an inline diagram (pins wired to +12 V permanent / ignition,
  ground, K-line, CAN) in the ECU database panel, and guide pages can embed one
  with a ```` ```pinout <unit-id> ```` block. Disputed pins are dashed and marked
  "confirm by continuity"; pins where two sources agree are marked. Design in
  [UNIT-ACCESS-PROFILE.md](docs/UNIT-ACCESS-PROFILE.md).
- Guide: **EDC15C6 on the bench** (Mercedes CDI): wiring diagram and steps,
  with both disputed pins (ground S1-5/S1-6, second terminal 15 S1-7/S1-8)
  called out.
- **BMW PSdZData SWE flash containers (read-only)**: `container unpack` decodes a
  BTLD / SWFL / SWFK / IBAD `.bin` + `.xml` pair (the sibling is found by name, or
  `--pair`): the `FLASH-SEGMENT` table, stored, UCL NRV2B and gzip segments, and
  the Ident / SignatureTable / Checksum metadata. Signature and CRC16 are reported
  **unverified**; CAFD `.caf`, undocumented compression states and ambiguous
  segment lengths are refused, not guessed. New `container identify` and
  `container unpack --flat` (flat image for any container format). Built from
  public sources only ([BMW-CONTAINERS.md](docs/BMW-CONTAINERS.md)); tested on
  synthetic fixtures only, not yet on a real BMW file.
- Clean-room UCL NRV2B decompressor (`opentune_container::nrv2b`), checked
  against the public `uclcli` test vector.
- Evidence level **`example`** (below `implemented`) for placeholder definitions:
  never used to identify a real unit, labelled "Example — placeholder data".
- Load/validate tests for the three imported BMW MS4x definitions (`bmw-ms4x-ms41`,
  `-ms42`, `-ms43`): id, family, size, map count, GPL-3.0-only provenance and
  `implemented` evidence.
- **First community definition pack, `community-drivetrain`**: three drivetrain
  controller definitions not in `ctrldefs/`, transcribed from opendbc (MIT) DBCs
  and firmware-query fingerprints: Hyundai / Kia TCU (`TCU11`-`TCU13`, UDS
  `0x7E1`), Hyundai / Kia 4WD coupling (`_4WD11`-`_4WD13` plus ABS wheel speeds)
  and the Subaru Global-platform transmission (`Transmission`, `CVT`, wheel
  speeds, UDS `0x7E1`). **synthetic-tested** only: every frame is decoded from
  synthetic payloads built from the DBC layouts; the UDS channels are untested
  and nothing has been checked against a real car or capture.
- **[PACK-AUTHORING.md](docs/PACK-AUTHORING.md)**, a step-by-step "write and
  submit a pack" guide worked through on that pack (scope, sources, scaffold,
  DBC transcription, validate, privacy scan, synthetic tests, evidence, pull
  request), linked from CONTRIBUTING.md; the PR template's pack checklist now
  asks for the controller validator and a synthetic test.
- **Brand module coding** ([CODING.md](docs/CODING.md)): pack `vag-mib2-coding`,
  the long coding of the VAG MIB2 High infotainment unit (module 5F, DID
  `0x0600`, 118 parameters), converted from the MIT-licensed mib2-toolbox;
  Ford As-Built format support in `opentune-coding` (line checksum, `.ab` XML
  reader, block = DID `0xDE00 + n - 1`) with `opentune coding asbuilt` (check
  checksums, export a module as a mock coding store); and an importer for a
  user-supplied CCF/CCC parameter descriptor (`opentune coding import-ccf`).
  No Ford labels ship: no openly licensed Ford label source was found.
  Synthetic-tested only (udssim, vcar and mock-store round trips with invented
  coding bytes); never run against a real module.
- **RomRaider logger definitions import / export**: `import-romraider-logger
  <logger.xml> [--protocol SSM|OBD] [--ecu <ROM id>] -o <logdef.json>` turns a
  RomRaider `logger.xml` into an OpenTune JSON logger definition with a
  per-parameter report (linear / expression / calculated / switch / text /
  refused); anything that cannot be logged correctly is disabled with the reason
  (unsupported expression, input not imported — e.g. a per-ECU parameter without
  `--ecu` —, missing unit conversion, cycle, address out of range). DS2 and
  Consult protocols are refused. `export-romraider-logger` writes the
  representable subset back (round-trip tested). Independent implementation of
  the published format; no RomRaider definition files are committed (they carry
  no licence) — validated at development time against four public files and on
  demand with `OPENTUNE_ROMRAIDER_LOGGER`. Synthetic-tested only.
- **Expression conversions and calculated live-log channels**: conversions that
  are not affine (`14.7/(1+x)`) are evaluated per sample by a safe expression
  evaluator (`opentune_logdef::expr`, no `eval`); RomRaider `<depends>`
  parameters (`(P12*60)/P8`, `[P7:psi]-[P24:psi]`, EcuFlash `GetLogParam`) are
  computed each round from the other channels, which are logged automatically
  (polling, `--fast` and SSM). New `ConversionFit::Expression`.
- `LogDef::validate` (duplicate ids, protocol/addressing mismatches, bits outside
  the read, unresolved or cyclic references); `packs validate` now runs it on
  pack loggers. `opentune log --def` reads JSON logger definitions as well as XML.
- **Seed/key definitions loader (UnlockECU format), off by default**: new crate
  `opentune-seedkey-loader` reads the *user's own* UnlockECU-format definitions
  file and answers `--seedkey unlockecu:<EcuName or alias>` through the existing
  `SeedKey` hook, choosing the definition by the requested access level. Loads
  only when both a file and a provider are configured (CLI global flags
  `--seedkey-defs` + `--seedkey-provider`, `CliConfig::seedkey_loader`, or
  `DesktopConfig::seedkey_loader` on alpha / bench channels only). Providers:
  the user's **external provider program** (absolute path, no shell, one JSON
  request on stdin / one JSON response on stdout, 5 s timeout, 64 KiB output
  cap) or `builtin:sa2` (`VolkswagenSA2` entries on the SA2 VM over their own
  `InstructionTape`). Strict validation (file size, lengths, level range,
  parameter types; bad entries rejected and reported), and a hard
  **immobiliser / anti-theft refuse-list** (EZS, ESL, ELV, WFS, immo/immobili*,
  keyless/KESSY, entriegeln, unlock, Wegfahrsperre, theft) applied even to
  user-supplied files. Keys and parameter values are never printed. New
  `opentune seedkey defs <FILE>` checks a file without running anything.
  OpenTune ships **no** provider and **no** definition
  ([SEEDKEY-LOADER.md](docs/SEEDKEY-LOADER.md)); synthetic-tested only (a
  synthetic provider program and the UDS simulator's SecurityAccess).
- **Offline fake OpenPort 2.0** (`opentune_driver_openport2::fake`): a scripted
  cable speaking the documented `at…` protocol with a simulated CAN bus and an
  ISO-TP ECU behind it (the `opentune-udssim` server in the tests), with fault
  injection (`are` replies, silence, stale replies, frames ahead of the ack, no
  bus ACK, background traffic, garbage, loopback echoes) and split/merged read
  chunking. `opentune info` / `identify` / `read --driver openport2` now run end
  to end against it over a pty. Offline only.

- Desktop **Render check** (Debug panel): the WebKit/GTK environment the app
  sees (a fixed whitelist: `WEBKIT_DISABLE_COMPOSITING_MODE`,
  `WEBKIT_DISABLE_DMABUF_RENDERER`, `GDK_BACKEND`, `XDG_SESSION_TYPE`,
  Wayland/X11 display and AppImage presence, `/dev/dri` render nodes), the
  WebGL renderer, and scroll frame timing per panel (fps, p50/p95 frame time,
  long frames) measured while you scroll for ~5 s. Results go to the session
  log; `OPENTUNE_PERF=1` logs the environment at startup and measures every
  panel's first scroll (also on stderr). New commands `render_env` and
  `render_note`.
- Desktop: a one-time, dismissible warning when `WEBKIT_DISABLE_COMPOSITING_MODE`
  is set (everything is CPU-painted, scrolling will be slow).

### Changed
- Internal: the live-log request building (logger definition, SSM or UDS
  addressing with the engine assumed when no ids are given, fast / SSM poll
  options, idle stop, reconnect policy) moved from the desktop's `log_start`
  into `opentune_hub::livelog::live_request`, so the native app starts the
  same sessions. No behaviour change.
- Internal: the desktop's live-log session engine (the `0x2C` fast / SSM
  live session held on the adapter hub, link-loss detection and reconnect,
  the stateless poll and the logger-definition loader) moved from
  `feat_logging.rs` into `opentune_hub::livelog`, so a second front end can
  share it. No behaviour change; the `log_*` commands are thin glue over it.
- Internal: the desktop's local diagnostics log (records, write-time masking
  of SecurityAccess seeds/keys and long hex payloads, size caps and rotation,
  the process-wide log, the chained panic hook, rate-limited frontend reports
  and read-back) moved into the new workspace crate `opentune-diaglog`, so a
  second front end can share it. No behaviour change; the desktop keeps its
  `init` and `diag_log*` commands and re-exports the rest under the same
  `opentune_desktop::diaglog` paths.
- Internal: more pure desktop helpers moved into the workspace crates, so the
  native app can call them. No behaviour change; the desktop's Tauri
  commands call the moved code and the moved unit tests went with it.
  - The Controllers panel's catalog (built-in definitions plus definition
    packs), link-validation context, list / detail rows and the CAN-log
    decode → `opentune_hub::controllers` (not `opentune-ctrldef`:
    `opentune-packs` depends on it).
  - The Log viewer's map overlay (axis breakpoints, unit checks, the binned
    payload) → `opentune_hub::logoverlay` (it reads axes and cells with the
    hub's map codecs).
  - The Haldex panel's chart series, their decimation, the decoded-log
    payload and the Live-log CSV export → `opentune_haldex::series`.
  - The aftermarket-ECU panel's value formatting, per-constant diffs, UI
    value parsing and INI / constant / axis / `.msq` report JSON →
    `opentune_tsini::present`.
- Internal: the desktop's adapter/session layer (adapter hub, sessions,
  module addressing, driver registry and probe, seed/key lookup, build
  channel, SA2 script decoding, map cell codecs, vault listing) moved into
  the new workspace crate `opentune-hub`, so a second front end can share it.
  No behaviour change; the desktop re-exports it under the same public paths.
- Internal: the desktop's long-running adapter operations moved into
  `opentune-hub` as `opentune_hub::ops`: the CAN capture job with its
  counters and stop handling (`ops::candump`), and the actuator-test gates,
  live run, exchange log, STOP and automatic return of control
  (`ops::actuator`), so a second front end runs the same code. The desktop
  keeps only the Tauri commands; its session log plugs in through a small
  `ExchangeLog` trait. No behaviour change.
- Internal: more desktop helpers moved into shared crates, no behaviour
  change: the stock/tuned diff clustering, 16-bit shape guess and map cell
  deltas into `opentune_mapfind::diff` (the CLI's `findmaps` / `compare` and
  the desktop now share one copy), the Find maps / Compare payloads into
  `opentune_hub::compare`, the seed/key calculator's generic transforms into
  `opentune_hub::seedkeys::generic`, and the "What's new" changelog parsing,
  semver order and seen-version check into `opentune_hub::whatsnew`.
- CI and `scripts/check.sh` build, lint and test with `--locked` (the
  workspace, `desktop/src-tauri` and the release builds), so a stale
  `Cargo.lock` fails the run instead of being silently updated.
- **Breaking for embedders: `DesktopConfig` gained a `diagnostics` field**
  (the diagnostics upload target). A plugin that builds `DesktopConfig` with a
  struct literal adds `diagnostics: None` (or its own `DiagUpload`); one that
  uses `..DesktopConfig::default()` needs no change.
- Desktop (Linux): **GPU rendering on by default; DMA-BUF disabled only on
  detected known-bad setups.** OpenTune no longer sets
  `WEBKIT_DISABLE_DMABUF_RENDERER=1` unconditionally: it does so only when
  there is no usable render node, with the NVIDIA proprietary driver, on a
  virtual GPU, with forced software GL, on remote X, or when the last start
  never drew a frame (self-heal: fallback, one retry after 2 clean starts,
  then off for good). Overrides: `OPENTUNE_KEEP_WEBKIT_ENV=1`, a user-set
  `WEBKIT_DISABLE_DMABUF_RENDERER`, and the new `OPENTUNE_DMABUF=auto|on|off`.
  The startup trace, `render_env` and the Render check show the decision and
  the rule. See docs/TROUBLESHOOTING-STARTUP.md.
- Desktop repaint costs: Drive view tiles and the map heat grids use
  `contain: content` (a tile's update repaints only that tile); paused Drive
  view tiles are dimmed with opacity and muted colours instead of a
  `grayscale` filter; the Dashboard gauge arc no longer animates its path on
  every sample.
- **Breaking: ECU definition id renamed** `siemens-pcr21-03l907425c` → `siemens-pcr21-vag16tdi-ident`, so
  the id no longer carries a hardware part number. Identification is unchanged (the
  part number stays a match key); anything referencing the old id (scripts, packs,
  saved sessions) must use the new one. Its `vehicle-tested` evidence now states
  that it covers only the live identity read.
- Evidence levels corrected after review: `bosch-edc16-toyota` is now `example`
  (placeholder addresses and maps); the Haldex Gen4 ECU definition is
  `synthetic-tested` (its decode is checked against a public CAN capture, not a
  flash image).
- **KWP2000 basic settings try without a login first** (engine, `adaptation
  kwp-basic`, desktop KWP card). Many VAG modules allow basic settings without a
  login (as the public kw1281test tool does), so the seed/key handler is no longer
  required up front. It is used only if the module refuses the start with NRC
  0x33 (securityAccessDenied). Without a handler, that refusal reports "this module
  needs a login for basic settings" and starts nothing. NRC 0x22 is not treated as
  a login request. The confirmation gate and Bench tier are unchanged, and
  adaptation test / save still need a login. **Untested on a real car.**
- **Host API: `CliConfig` and `DesktopConfig` gained a `seedkey_loader` field**
  (default `None`). Embedders that build either with a full struct literal
  must add `seedkey_loader: None` (or use `..Default::default()`).

### Fixed
- **Backup vault: two backups in the same second no longer overwrite each other.**
  Backup files were named by ECU, address and Unix second, so a second backup
  of the same ECU within one second replaced the first (e.g. the original dump
  before a quick re-save). A new backup now gets a `_2`, `_3`, … suffix and is
  created exclusively, so no backup is ever replaced.
- ECU identification no longer lets a generic prefix claim an ECU. Every
  Bosch software number starts with `1037`, and `bosch-edc16u34`, `bosch-me7`
  and the EDC16 example carried a bare `1037` software key, so an EDC15C6
  with an unlisted software number, any ME7 and most other Bosch dumps were
  identified as EDC16U34 (and map write-back with checksum fix took that
  definition's checksum). The keys are removed (definitions, the Android
  copies and the demo sample keep their part numbers). The matcher now picks
  the definition whose longest identify key hits instead of the first one in
  file order, ignores keys shorter than 6 characters, and `ecudefs validate`
  rejects such keys as an error (`identify.key_short`, was a warning below
  3). Synthetic tests cover the EDC15C6 case in both load orders and check
  that every shipped key still resolves to its own definition. Pack authors:
  a community definition pack whose identify key is shorter than 6
  characters is now refused when the pack loads (`identify.key_short`); use
  the full part or software number, or a family prefix of at least 6
  characters (e.g. `03G906`).
- ECU identification: the four reference Mercedes EDC15C6 calibrations
  (SW 1037351686 / 351637 / 351736 / 351737) now match the `bosch-edc15c6`
  definition instead of `bosch-edc16u34`, whose broad `1037` software key
  matches any Bosch number. Scanning every owner fixture before and after
  shows no other match changed.
- EDC15C6: `checksum verify` reports a real EDC15C6 (or VAG TDI v4.1v2) image
  as a recognised family instead of "mathematical match only"; the
  `bosch-edc15c6` definition now names the `edc15c6` checksum (map write-back
  can fix it directly) and the 1 MiB flash size the bootstrap read and the
  checksum use.
- Desktop "What's new": versions compare with semver pre-release precedence,
  so an update from a pre-release to its final release (e.g. `1.0.0-rc.1` to
  `1.0.0`) shows the popup instead of being taken for a downgrade.
- Desktop: the sidebar adapter row no longer runs about 20 px past the pane;
  the port field and the HS/MS picker now shrink to fit.
- Desktop: the sidebar's adapter **driver** and **CAN bus** pickers no longer
  open their menu past the screen edge in fullscreen (WebKitGTK placed the
  native `<select>` popup itself). They are now an in-page listbox
  (`listbox.js`) that opens down when it fits, otherwise up, stays inside the
  window and scrolls inside when long; full keyboard support (arrows,
  Home/End, Page Up/Down, type-ahead, Enter/Space, Esc) and combobox/listbox
  ARIA roles. The hidden `<select>` keeps the value, so nothing else changed.
- **`openport2` (Tactrix OpenPort 2.0) driver audited against the public
  protocol notes and rewritten at the wire level** (bisak/openport-j2534,
  Mackanized/op2j2534, roffe/libj2534_openport2; facts only). The old code could
  not have worked on a cable: lines ended in a bare CR (ignored by the device),
  `ato`/`atc`/`att`/`atf` were malformed (spaced protocol digit, missing `ato`
  field, a non-existent channel handle, inline ASCII transmit/filter payloads),
  received-frame lengths were misread by 9 bytes, transmit indications were
  delivered as ECU data, frames arriving before an ack were dropped, replies
  were not matched by sequence number and `atr` was parsed wrongly. Now: CRLF,
  documented command shapes with binary payloads in one write, sequence-matched
  replies with stale-reply dropping, queued interleaved frames, indications
  dropped, J2534 error names, a resync preamble, a CAN bit-rate allow-list and
  a configurable transmit budget (`tx_timeout_ms`). `programming_voltage` is no
  longer advertised (nothing implements it). Every deviation and its source is
  listed in [DRIVERS.md §3l](docs/DRIVERS.md#3l-openport2-tactrix-openport-20-hardware-validation-pending),
  with a hardware-validation checklist. **Hardware validation pending**: tested
  against the offline fake only.
- `j2534` driver: the ISO 15765 flow-control filter now sets
  `ISO15765_FRAME_PAD`, so the interface pads its flow-control frames; the
  Windows FFI no longer reports a real `PassThruReadMsgs` error (e.g. device
  unplugged) as a timeout. Hardware-unproven.
- The placeholder `bosch-edc16-toyota` definition could identify a real Bosch
  EDC16U34: its keys (`0281011…`, software `1037…`) overlap and it sorted first.
  Example definitions are now skipped by identification.
- Logger definitions: a real RomRaider `logger.xml` (with its `<!DOCTYPE>`
  header) failed to parse; an `x` inside a function name or parameter id was
  upper-cased by the equation normalizer; a bit on an OBD PID / UDS DID was
  silently dropped (now left unaddressed with a warning); an OBD `<address
  length>` is now the value width, so one byte of a two-value PID is no longer
  read as both; `<ecuparam>` addresses now honour `bit` and one-address-per-byte
  fragments; DS2 / Consult parameters are no longer read as plain memory.

## [0.7.0] - 2026-11-19

### Added
- **Guide: dumping a Mercedes 722.6 TCU (EGS51/52/53)** — how to tell the three
  generations apart and read your own unit (bench only); states plainly that OpenTune
  has no EGS flash checksum yet.
- **"Check my car" guided wizard** (desktop, Bench): a read-only walk through
  identify → scan → fault codes → live data, tested end to end on the virtual
  car and an OBD simulator.
- **Logging triggers**: start, stop and mark conditions for datalogging
  (`log --trigger`, `--split`, also on `obd read --out`), a `triggers/` pack kind
  and a generic-OBD2 wide-open-throttle pull trigger set.
- **Desktop Live log**: reconnect through the adapter hub with link-lost markers,
  Subaru SSM transports (K-line / CAN) and an offline SSM simulator; a negative
  response now counts as a live link in polling and on the Dashboard.
- Offline EGS52 coding CRC verification and opt-in recomputation. CRC-16/ARC
  is implemented and remains unconfirmed on a real EGS.
- EGS52 C167 flash read preset for the 512 KiB emulator window at `0x80000`.
  Plans name missing board registers. Reads require all values. Simulator coverage only.
- Standalone multithreaded checksum search and stock/modified diff tool under
  `tools/checksum-search/`, with generated synthetic test cases.

### Changed
- **Desktop: the sidebar and the content pane scroll separately.** The window is
  a fixed-height shell; scrolling a long panel no longer moves the sidebar and
  vice versa, and each panel keeps its own scroll position. Sidebar groups
  (Calibration, Vehicle, Reference, Help, Bench) collapse from their header
  button; the choice is remembered, and the open panel's entry stays visible.

### Fixed
- **Desktop: smooth scrolling in WebKitGTK.** Long panels (Maps, Log viewer,
  Inspection, Haldex, Controllers, ECU database) scrolled at 30–48 fps because
  every card's blurred shadow was repainted per frame; they now hold 60 fps.
  On alpha/bench builds the whole page scrolled as one (the sidebar stretched
  the layout to ~1800 px). The panel fade is opacity-only, the dashboard
  paints only while visible, and the dashboard no longer touches the GPS
  card's tiles.

## [0.6.0] - 2026-11-19

### Added
- **Read-only DSG diagnostics** (`opentune dsg identify|dtcs|groups`) for the VAG
  DQ250 02E over KWP2000 / VW TP2.0: identification, trouble codes and measuring
  groups. Group meanings are not assigned yet (raw, marked unconfirmed) until a
  real capture backs them. Never run against a real gearbox yet.
- **VAG KWP2000 adaptation** in the adaptation engine, over TP2.0 and K-line:
  measuring blocks, basic settings, adaptation read / test / save with read-back
  (`opentune adaptation kwp-read|kwp-basic|kwp-test|kwp-save`, desktop KWP card).
  Writes need a session, a caller-supplied login and an explicit confirmation;
  OpenTune ships no VAG login algorithm.
- **Actuator tests** (UDS `0x2F`): timed output tests with tester-present,
  automatic return of control to the ECU (also on stop, error, panel close or
  disconnect), desktop **Actuator tests** panel (Bench) and
  `opentune adaptation actuator list|run` (plan only unless `--ack`).
- **Controller definitions** (`opentune-ctrldef`, `ctrldefs/`): one data-defined
  format for gearbox and AWD units (DQ250 02E/MQB, DQ381, Haldex Gen4/Gen5,
  OpenHaldex, ZF 8HP, Mercedes 722.6 EGS51 CAN decode), `opentune controller
  list|show|validate|match|decode`, a `controllers/` definition-pack kind and a
  desktop **Controllers** panel with offline CAN-log decode (candump, CSV, Vector
  ASC).
- **SA2 seed/key scripts**: the interpreter now runs the real VAG SA2 instruction
  set (differential-tested against a public reference implementation), lists the
  scripts inside your own FRF/ODX/SGO files, `opentune seedkey sa2`, and a desktop
  **SA2 script** panel (disassembly, key preview). No scripts are bundled.
- **DoIP** (ISO 13400) as an interface: `--driver doip` with vehicle discovery
  (UDP identification by VIN/EID, announcements, entity status, power mode),
  routing activation, every engine over Ethernet, and a desktop **Discover**
  button (Bench). Tested against a loopback simulator only.
- **GPS logging**: an NMEA 0183 source (`opentune-gps`, RMC/GGA/VTG) for the
  datalogger (`log --gps <port|file>`) and the desktop Live log / Dashboard,
  with position, true ground speed and the speedometer error.
- **Resilient logging over any transport**: the log/OBD pipeline handles a quiet
  gateway bus and an adapter that drops and reopens mid-log (`--reconnect`,
  `--duration`).
- **Drive view**: an at-a-glance gauge view with peak hold, sparklines and offline
  replay of logs.
- **Subaru SSM2** transport, simulator and logging.
- **New interface drivers** (all unverified on hardware): EFI EFP02C USB-CAN
  (1 Mbit/s, 11-bit, writes disabled), the open ESP32 ISO-TP BLE bridge firmware
  (`isotp-ble`; Bluetooth backend behind an optional feature), DoIP, and the
  ESP32-S3 OpenTune interface now registered in the CLI and (Bench) desktop.
- **C167/ST10 bootstrap-loader client** for EDC15 boards (`opentune bsl
  presets|plan|read`): dry-run plan, read behind `--i-understand`, simulator with
  fault injection. You supply the loader files.
- **Contribution workflow**: `CONTRIBUTING.md`, issue and pull-request templates,
  source / licence / evidence metadata on every ECU definition and pack, a
  privacy scanner (`opentune privacy-scan`), `opentune packs validate` and a CI
  data gate.
- Desktop: a shared definition-pack picker in the engine panels; preview test data
  for every panel.
- An identification-only definition for a VW 1.6 TDI CR engine ECU (likely
  Continental Simos PCR2.1; no maps or checksum).

### Changed
- **VW TP2.0 hardened**: retransmit after a lost acknowledgement, receive-side
  recovery, automatic re-open after a channel break (the module's KWP session
  must then be restarted), bounded waits; a seeded fuzzer runs in every test run.
  It found and fixed two real bugs (sequence numbering after a lost first frame,
  a request executed twice with block size 0).
- KWP2000: full negative-response name table and echo checks on session,
  identification and security replies.
- "Response pending" (`0x78`) is accepted only when it echoes the request's
  service, in all drivers and in the ELM327 reply parser.
- DoIP requests honour the caller's timeout instead of waiting up to 60 s for a
  silent ECU.
- CLI log files gain a `ts` column, so they open in the desktop Log viewer.
- The ELM327 driver's first real-car read (identity over UDS) passed; its clone
  quirks (a receive filter that only `ATZ`/`ATAR`/`ATD` clear) are documented and
  pinned by a test.

### Fixed
- Block reads no longer return short images or shift later addresses when a
  module answers with fewer bytes than asked (slcan, ESP32, K-line bridge, KKL,
  ELM327 K-line drivers).
- A DSG measuring-group answer with no whole value is reported as an error
  instead of an empty group.

## [0.5.0] - 2026-11-18

First signed public release: desktop app (Linux AppImage/.deb, Windows installers)
with signed auto-update, and the `opentune` CLI for Linux, Windows and macOS. See
the [README](README.md) for the feature set at that point.

[Unreleased]: https://github.com/ebjerk01/opentune/compare/v1.0.0-beta.1...HEAD
[1.0.0-beta.1]: https://github.com/ebjerk01/opentune/compare/v0.7.0...v1.0.0-beta.1
[0.7.0]: https://github.com/ebjerk01/opentune/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/ebjerk01/opentune/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/ebjerk01/opentune/releases/tag/v0.5.0
