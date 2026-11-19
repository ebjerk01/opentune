# Diagnostics log

OpenTune keeps a small local log of what went wrong on your machine: crashes,
errors in the app window, commands that failed, and the steps of every start.
It is **on by default and stays on your computer**: OpenTune never sends it
anywhere. It is there so that a bug report can say exactly what happened.

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
