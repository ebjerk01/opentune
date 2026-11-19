# Drive view

A full-window view of a few large tiles, made to be read at arm's length on a
small laptop or tablet: engine speed, vehicle speed, coolant, intake air,
boost and load, plus GPS speed and the speedometer error when a GPS source is
present. It shows **live** values while driving or **replays** a recorded log
offline, in exactly the same layout.

## What you need

- **Replay**: a CSV log. Everything the **Log viewer** opens works: a Live-log
  CSV, a Dashboard recording, an `opentune log` CSV from the command line, or
  any wide CSV with one column per channel. No adapter, no car.
- **Live** (alpha / bench builds only, like the Dashboard): an adapter with a
  diagnostic link, detected in the sidebar, and the ignition on.

## Replay step by step

1. Open **Drive view**, choose **Replay** and press **Open log…** — or, in the
   **Log viewer**, open a log and press **▶ Drive view**.
2. Press **▶** (or Space). Pick **1×**, **2×**, **4×** or **8×**.
3. Drag the scrub bar to jump anywhere in the log.

A log without a time column is replayed at 5 samples per second at 1×. A very
long log is thinned so each frame holds the latest value of a few samples; the
note under the bar says so.

## Live step by step

1. Detect the adapter in the sidebar.
2. Open **Drive view**, choose **Live** and press **Start**. This starts the
   **Dashboard's** polling (the same requests, the same adapter path) and asks
   it to poll the Drive view's channels too. **Stop** stops that polling.
3. A running **Live log** session shows up here as well; whichever source is
   delivering values is named in the status strip.

Only one thing polls the adapter: the Drive view never opens a connection of
its own.

## Reading the tiles

- **Big number** — the latest value, in the unit it was logged in. Pressures
  follow the **bar / kPa** switch; nothing else is converted.
- **Bar** — where the value sits in the channel's range, with the warning and
  critical marks and a **peak** tick (the highest value since the last reset).
- **Sparkline** — the last 60 seconds.
- **min / max** — since the last reset. **R** or **Reset peaks** clears them.
- **Colours** — an amber frame past the warning threshold, a solid red tile
  past the critical one. The thresholds are the Dashboard's generic defaults
  (coolant, intake air, oil, EGT, DPF). They are applied only when the
  channel's unit matches the table's; a log in other units shows no colour
  rather than a wrong one.

## When values are not fresh

A tile never shows an old number as if it were live:

- **STALE** — this channel has had no new value for a while (three update
  intervals, at least 2.5 s). The big number is replaced by a dash and the
  line below says what the last value was and how old it is.
- **NO DATA** — this channel has not had a value yet.
- **LINK LOST** — the source stopped answering (failed polls, or nothing new
  arrived). The status strip turns red and every tile is greyed out.
- **Paused** (Space in live mode) — the display is frozen on purpose and the
  status strip says so.

## Choosing channels

**Channels…** lists every channel of the current source or log. Tick the ones
you want as tiles; they appear in the order ticked, and the first two are
large. **Default set** goes back to the automatic choice (found by channel id,
or by header name and unit in a log). Your choice is remembered per source.

## Keys

- **F** — fullscreen (hides the sidebar and controls). **Escape** or **Exit
  fullscreen** leaves it.
- **R** — reset min / max / peak.
- **Space** — pause / play (replay), freeze the display (live).

## Safety

- Read-only. Nothing is written to the ECU.
- Do not watch the screen while driving. Mount the screen where a glance is
  enough, let a passenger operate it, or use it on a dyno or a closed road.
- Validation state: live mode is **synthetic-tested** (Bench tier until it has
  been used in a car); replay is offline.
