# Live log

Stream values from the ECU using a RomRaider-style `logger.xml` definition,
watch them in a live table, and save the samples as CSV.

## What you need

- **Adapter: CAN**, connected and detected (a raw K-line adapter for Subaru SSM over K-line).
- A `logger.xml` definition for your ECU, or a definition pack's logger JSON. The
  bundled `generic-obd2/loggers/obd2-mode01.json` works on any OBD-II car on CAN.
- **Ignition on**; engine running for most values.
- The dongle's **HS/MS-CAN switch** on the bus of the module you log.
- To log a module other than the engine, set its **tx / rx** CAN ids in the sidebar. Blank means the engine ECU (7E0 / 7E8).

## Step by step

1. In the **Pack** picker choose a pack and its logger definition (e.g. *Generic OBD-II* → `obd2-mode01.json`), or press **File…** for a `logger.xml`. Then press **Load**. The parameters are listed.
2. Tick the parameters to log, or use **Select all** / **Select none**. With none ticked, every parameter is logged.
3. Set the **Interval** in ms (minimum 50, default 500).
4. Press **Start**. The table updates in place.
5. Press **Stop**, then **Save CSV**.

## Fast mode (UDS 0x2C)

Tick **Fast (0x2C)** before **Start** to read many channels with one request
per sample instead of one per channel. OpenTune asks the ECU to build a
*dynamically defined DID* (ISO 14229-1 service `0x2C`, range `0xF200`–`0xF3FF`)
from the selected UDS DIDs and memory addresses, then reads that one DID each
tick.

- **Dynamic DID** — the first DID to define (default `F200`). Up to four are used.
- **Max record** — bytes per dynamic DID (default 32). If the ECU refuses the size, lower it.
- **Session** — a diagnostic session to enter before defining, e.g. `03` for an
  ECU that only accepts `0x2C` in the extended session. Blank leaves the session alone.

The badge shows the mode the session really runs in:

- **fast-2c** — the ECU accepted the definition. Hover the badge for the dynamic
  DIDs. Channels that cannot be packed (OBD-II PIDs, failed probes) are polled
  alongside and listed under the badge.
- **polling** — the ECU refused part of the setup (for example NRC `0x11`
  *serviceNotSupported*, `0x31` *requestOutOfRange* or `0x33`
  *securityAccessDenied*). The reason is shown and the log continues with one
  request per channel. OpenTune never unlocks security access for logging.

Next to the badge: the achieved **samples/s** and **req/sample** (requests per
sample; 1.00 means one request read every channel).

A fast session holds the adapter for as long as it runs, so other panels report
*adapter busy*. It stops, and clears the dynamic DIDs on the ECU, when you press
**Stop**, switch to another panel or the adapter disconnects. If the app stops
ticking for 15 s the session also stops itself. When the link drops, see
**Dropped links and Reconnect** below.

With the toggle off, the panel polls exactly as before.

## Dropped links and Reconnect

A tick in which nothing answered at all is **dead** (an ECU that refuses a
channel with a negative response still answered: that tick is alive). The
**link pill** next to the options shows *no answer 3/5* while ticks die, then
*link lost* after five dead ticks in a row — or at once when the adapter
resets (an ELM327 that power-cycled and printed its banner).

- **Reconnect if the link drops** off (the default): the session stops on a
  lost link, as before, and frees the adapter.
- On: the adapter is closed and re-opened with a growing pause (0.5 s, 1 s,
  2 s … up to 10 s; the pill says *reconnecting (attempt 2)* and why the last
  try failed). Once it opens, the session starts again — fast mode re-defines
  its dynamic DIDs — and the pill shows *reconnected*. It gives up after five
  minutes down, or after three re-opens in a row in which nothing answered
  (ignition off). Reconnect alone also opens a session (it holds the adapter),
  even without Fast.

Reconnect is off by default because a dropped link is often the ignition
being switched off: re-opening keeps talking to a bus that is going to sleep.

Every link event is a row in the CSV with parameter id `link` (*Link status*):
the event in `note` and `lost`, `reopen-failed`, `reconnected` or `gave-up` in
`text`. No value is invented for the time the link was down. The **Drive
view** shows the link as lost while it is down.

Validation state: **synthetic-tested** (simulated bus with an unplugged
adapter, refused re-opens and an adapter reset). Not yet tested by unplugging
a real adapter mid-drive.

## Subaru SSM

**Transport** chooses what Start speaks: *UDS / OBD-II* (the default),
*Subaru SSM (K-line)* or *Subaru SSM (CAN)*. With SSM:

- **Module** — *ECU (engine)* or *TCU (transmission)*.
- Use a RomRaider `logger.xml` with an SSM protocol. Over K-line you need a
  raw K-line adapter (`kkl` cable or the OpenTune K-line bridge; an ELM327
  cannot do SSM); over CAN any CAN adapter.
- **Simulated module** runs against a synthetic SSM module, no adapter needed.

On Start the module is identified (`BF`): the panel shows its SSM ID, ROM ID
and how many parameters are logged. Only parameters whose capability bit is
set (and per-ROM parameters for this ROM ID) are logged; **Left out** lists the
others with the reason (other module, capability bit clear, derived, …) and
any ticked parameter that is not logged. Values are read in batched `A8`
requests. Fast (0x2C) does not apply. Nothing is written to the module.

Validation state: **synthetic-tested** (SSM simulator). Not yet run against a
Subaru.

Validation state: **synthetic-tested** (UDS simulator). Not yet tested on a car.

## Reading the results

- **Value** and **Units** per parameter. **Note** explains a missing value.
- UDS parameters (service `0x22`, read by identifier), OBD-II mode-01 PIDs and UDS `0x23` memory reads are read; SSM parameters with an SSM **Transport**. Other protocols come back with a note instead of a value.
- **Text channels** — identification data such as the VIN (`F190`), part and
  serial numbers (`F187`, `F18C`) or programming dates (`F199`) is not a number.
  A definition marks such a channel as text, BCD or hex (`value_kind` in a pack's
  logger JSON, `storagetype="string"`/`"bcd"`/`"hex"` in a `logger.xml`), and the
  table shows the decoded text of the **whole** record instead of a number.
  Non-printable bytes are shown as `.` and noted. The bundled
  `iso14229-identification` logger marks all its DIDs this way.
- The CSV has one row per parameter per tick: timestamp (ms), parameter id, name, raw value, scaled value, units, note and text. A text channel leaves `raw` and `value` empty and fills `text`. The **Log viewer** opens the file; text channels are listed in its notes rather than plotted.

## GPS (optional)

The **GPS** card adds a GNSS receiver. A u-blox style USB receiver shows up as
a serial port (`/dev/ttyACM0`, `COM5`); **Ports** lists them, likely receivers
first. **File…** replays a recorded NMEA file instead (in a loop). Press
**Open**: the tiles show position, GPS speed, altitude, course, satellites,
fix and HDOP; the pill says **fix**, **no fix** or **stale**.

While the card is open, each tick also logs `gps_lat`, `gps_lon`, `gps_alt_m`,
`gps_speed_kmh`, `gps_course_deg`, `gps_sats`, `gps_fix` and `gps_hdop` to the
CSV. Without a good fix the position and speed stay empty and the note says
why. When a vehicle-speed channel is logged (an id like `vss` or a name with
"vehicle speed"), **Speed error** shows vehicle minus GPS speed in km/h — only
with a good fix above 5 km/h. Speedometers usually read a few percent high.

Validation state: **synthetic-tested** (recorded NMEA). Not yet tested with a
real receiver.

## Common errors

- **Slow updates** — an ELM327 manages roughly 1–3 parameters per second. Log fewer parameters or use a faster adapter. Ticks are skipped while the adapter is still busy with the previous one.
- **Poll error: adapter busy** — another panel is using the adapter.
- **Fast mode says polling** — the ECU does not offer `0x2C` in this session or for these sources. Try **Session** `03`, a smaller **Max record**, or log without fast mode.
- **All values blank** — wrong bus, wrong target ids, or the definition does not match this ECU.

## Safety

Read-only on the vehicle. Have a passenger run the laptop if you log while
driving.
