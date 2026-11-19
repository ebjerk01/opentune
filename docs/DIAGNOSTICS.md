# Diagnostics log

OpenTune keeps a small local log of what went wrong on your machine: crashes,
errors in the app window, commands that failed, and the steps of every start.
It is **on by default and stays on your computer**: the public OpenTune app
has no upload address and never sends it anywhere. It is there so that a bug
report can say exactly what happened.

A build that embeds OpenTune (a plugin build), or you yourself through an
environment variable, can configure an **upload** of a redacted copy; see
[Uploading diagnostics](#uploading-diagnostics) below.

## Where it lives

The file is `logs/opentune.log` in the app's data directory:

| OS | Path |
| --- | --- |
| Linux | `~/.local/share/org.opentune.desktop/logs/opentune.log` (or under `$XDG_DATA_HOME`) |
| macOS | `~/Library/Application Support/org.opentune.desktop/logs/opentune.log` |
| Windows | `%APPDATA%\org.opentune.desktop\logs\opentune.log` |

A plugin build that embeds OpenTune uses its own app identifier in place of
`org.opentune.desktop`.

Each file holds at most 1 MiB. When it is full it is renamed to
`opentune.log.1` (the older ones move up to `.2` … `.5`, and the oldest is
deleted), so the log never takes more than 6 MiB.

In alpha builds the **Debug** panel has a **Diagnostics log** card: the last
100 records with a level filter, **Open folder**, **Copy last 200 lines** and
**Clear…** (deletes the file contents and the rotated files, after a
confirmation).

## Format

One JSON object per line:

```json
{"ts":"2026-11-19T15:04:05.123Z","level":"warn","src":"cmd","msg":"image_info failed: …","ctx":{"cmd":"image_info"}}
```

- `ts`: UTC time, RFC 3339 with milliseconds.
- `level`: `debug`, `info`, `warn` or `error`.
- `src`: `rust` (a crash, or a line of the Debug panel's session log), `js`
  (an error in the app window), `cmd` (a command the window called failed) or
  `startup` (a startup step, or the session header).
- `msg`: the message; `ctx` (optional): details such as the source location
  of a crash or a short stack.

Every start begins with a **session header** (`"src":"startup"`,
`"msg":"session start"`) with the app version and build channel, the OS and
architecture (and the Linux distribution name), the WebKit version, and the
render decision: whether the WebKitGTK DMA-BUF renderer is on, which rule
turned it off, the GPU driver of the render node and what OpenTune set before
WebKit started (see [TROUBLESHOOTING-STARTUP.md](TROUBLESHOOTING-STARTUP.md)).

## What is recorded

- **Crashes** (Rust panics): the message, the source location and the thread;
  with `RUST_BACKTRACE=1` also the backtrace.
- **Errors in the app window**: uncaught errors and unhandled promise
  rejections, with the script location and a stack cut to 2,000 characters.
- **Failed commands**: the command's name and its error text. Its arguments
  are never recorded.
- **The session log** of the Debug panel (raw requests, adapter probes, Render
  check results), line by line, after masking (below).
- **Startup steps** (the same milestones `OPENTUNE_TRACE_STARTUP=1` prints),
  with the time since start, on every start.

Limits: a message is cut to 2,000 characters, each detail to 4,000; at most 5
reports from the app window per second (the rest are counted and the count is
recorded), and the same window error repeated within 10 seconds is counted
instead of written again.

## What is not recorded, and what is masked

Masking happens **before** a record is written, so the file never holds:

- **SecurityAccess seeds and keys.** In hex that is a SecurityAccess request
  (`27 …`) or its positive response (`67 …`), also behind an ISO-TP or
  KWP2000 header: everything after the sub-function becomes
  `<masked: N bytes>`, e.g. `67 01 <masked: 4 bytes>`. A hex value right after
  a `seed` or `key` label is masked the same way.
- **Long payloads.** Hex longer than 64 bytes keeps its first 16 bytes and its
  length: `<first 16 bytes> … <258 bytes>`.
- **File contents.** Nothing reads a flash image, a datalog, a definition or a
  CAN capture into the log; commands are logged by name and error text only.

The log can still contain things that identify your car or computer: a VIN or
a part number in a session-log exchange or an error message, and file paths
(which can include your user name). **Review the file before you share it.**

If the log cannot be written (no data directory, disk full, permissions), it
turns itself off for the rest of the session and says so once in the Debug
panel's session log; the app carries on as normal.

## For plugin builds

An embedder can add its own records through the same write path,
`opentune_desktop::diaglog::record(level, src, msg, ctx)`, which applies the
same masking and limits. The read-back API (`read_tail`, `read_since_session`,
`read_all`, `sessions`, `log_dir`) returns the records as structs.

## Uploading diagnostics

The public app ships **no endpoint and no token**: uploading is a capability
that stays unused until something configures a target. Nothing is ever sent
from a public build unless you set `OPENTUNE_DIAG_ENDPOINT` yourself *and*
allow it *and* click Send.

### Where a target can come from

| Source | Mode it can give |
| --- | --- |
| A plugin build's `DesktopConfig.diagnostics` (compiled in by the embedder) | manual or automatic |
| `OPENTUNE_DIAG_ENDPOINT` (+ `OPENTUNE_DIAG_TOKEN`) in the environment | **manual only**; the environment can never turn on automatic sending |
| neither (the public app) | none: nothing is sent, the Diagnostics card is hidden |

`OPENTUNE_DIAG=off` (also `0`, `false`, `no`) turns **every** upload off,
including a plugin build's automatic mode. The local log is unaffected.

`OPENTUNE_DIAG_INSECURE=1` is a developer flag, honoured on alpha / bench
channels only: it allows a plain-HTTP endpoint and a certificate that does not
verify (a local test server). Without it the endpoint must be HTTPS with a
valid certificate.

### Manual mode

Something is sent only when **all three** hold:

1. an endpoint is configured (above);
2. **Allow sending diagnostics** is on (About & updates → Diagnostics, or the
   Debug panel; saved per user, **off by default**);
3. you press **Send diagnostics…**, optionally write a note, press
   **Preview** and then **Send**. The preview shows the *exact* redacted text
   that will be sent; Send transmits exactly those bytes. Changing the note
   needs a new preview.

After a crash (the previous session panicked, or never drew its first frame)
the next start only **offers** to send: *Review and send…*, *Not now* or
*Don't offer again*. Nothing goes out without the steps above.

### Automatic mode (plugin builds only)

A plugin build may compile in automatic mode. It then sends without a click:

- **a crash report** at the next start, when the previous session in the log
  panicked or never painted its first frame (once per crashed session);
- **error reports**: new errors of this session, all of them coalesced into
  one report per allowed slot;
- optionally **a periodic session summary** (every *n* minutes, only when
  something happened): version, channel, OS / WebKit, render decision, error
  and warning counts, how often each panel was opened — no records.

Guards:

- On the first start the app shows a notice: *"This build sends redacted
  diagnostics automatically to <host>."* with **Turn off** and **What is
  sent**. Automatic sending starts only after this notice was shown.
- The sidebar footer shows **Diagnostics: auto → <host>** for as long as it
  is on; clicking it opens the settings.
- **Send diagnostics automatically** (About & updates → Diagnostics) is a
  global off switch, saved per user; off means nothing at all is sent
  automatically, the crash report included.
- Rate limits: at most **one upload per 10 minutes** and **20 per day**
  (manual uploads count towards the daily limit; failed attempts count too,
  so a dead network never causes a retry storm).
- **Offline queue**: an upload that fails for a network reason or a server
  error waits on disk (at most 2 MiB; the oldest is dropped first) and is
  retried at the next allowed slot. A refusal by the server (4xx) is not
  retried.
- **Upload history**: the last 20 uploads (time, trigger, size, status) are
  listed in the Diagnostics card; **Show payload** shows exactly what was sent.

### What is sent

One JSON document (`"schema": "opentune-diagnostics/1"`), at most **2 MiB**:

- `trigger` (`manual`, `crash`, `error`, `periodic`), `created`, the build's
  `label` if it set one;
- `app`: version, core version, channel, OS, architecture, distribution,
  WebKit version and the render decision (from the session header);
- `summary`: error / warning / panic counts of the session, the number of
  sessions and crashed sessions in the log, panels opened;
- `records`: records of the diagnostics log — for a manual send the current
  and the previous session, for a crash report the crashed session, for an
  error report the records since the last report (the newest are kept when
  the cap is reached; `omitted_records` says how many were dropped);
- `crash` (session and reason) for a crash report, and your `note`.

**Never sent, in any mode:** flash images, datalogs, definitions, CAN
captures, projects or any other file you opened. The upload reads nothing but
the diagnostics log.

### Redaction

The whole payload is redacted **before** the preview and **before** it is sent
(`opentune_core::redact`). Markers replace:

| What | Marker |
| --- | --- |
| VINs (also synthetic-looking ones, also as ASCII inside a hex dump, e.g. a `22 F190` response) | `<VIN>` |
| ECU / part numbers: VAG `03L 906 018 AJ`, Bosch `0 281 0xx xxx` and software `1037…`, Siemens / Continental `5WP…`, Mercedes `A xxx xxx xx xx` (also inside hex) | `<PART>` |
| labelled serial numbers (`S/N:`, `serial number`, …) | `<SERIAL>` |
| e-mail addresses | `<EMAIL>` |
| absolute paths (your home directory and user name with them) | `<PATH>/basename` |
| your user name and the computer's host name | `<USER>`, `<HOST>` |
| IPv4 / IPv6 addresses, MAC addresses | `<IP>`, `<MAC>` |
| SecurityAccess seeds and keys | `27 01 <masked: N bytes>` |
| hex payloads over 64 bytes | first 16 bytes `… <N bytes>` |
| the upload token and endpoint, should a message repeat them | `<SECRET>` |

Redaction is a heuristic: it errs on the side of removing too much. The
preview is there so you can check.

### Transport and privacy

- `POST` to the endpoint with `Content-Type: application/json` and, when a
  token is configured, `Authorization: Bearer <token>`; HTTPS only (see the
  developer flag above), 30 s timeout, no redirects followed.
- The token never appears in the log, the preview, the history, an error
  message or `Debug` output.
- Settings, the history (with the payloads sent) and the queue live in
  `<app data dir>/diagnostics/`; no token or endpoint is stored there.
- Privacy statement: OpenTune itself collects nothing. What a plugin build
  does with the reports it receives is up to whoever runs its endpoint; its
  notice names the host the reports go to.

### For plugin builds

```rust
use opentune_desktop::{AutoPolicy, DesktopConfig, DiagMode, DiagUpload, SecretString};

let config = DesktopConfig {
    diagnostics: Some(DiagUpload {
        endpoint: opentune_desktop::diagupload::Url::parse("https://diag.example.invalid/upload")?,
        token: Some(SecretString::new(my_token())),
        label: Some("my build".into()),
        mode: DiagMode::Auto(AutoPolicy {
            crash_on_next_start: true,
            on_error: true,
            periodic_min: Some(60),
        }),
    }),
    ..DesktopConfig::default()
};
```

`DiagMode::Manual` gives the manual flow with your endpoint. Keep the real
endpoint and token out of public source.
