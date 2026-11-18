# Live log

Stream values from the ECU using a RomRaider-style `logger.xml` definition,
watch them in a live table, and save the samples as CSV.

## What you need

- **Adapter: CAN**, connected and detected.
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
**Stop**, switch to another panel, the adapter disconnects, or no channel answers
for five ticks in a row. If the app stops ticking for 15 s the session also
stops itself.

With the toggle off, the panel polls exactly as before.

Validation state: **synthetic-tested** (UDS simulator). Not yet tested on a car.

## Reading the results

- **Value** and **Units** per parameter. **Note** explains a missing value.
- UDS parameters (service `0x22`, read by identifier), OBD-II mode-01 PIDs and UDS `0x23` memory reads are read. SSM and other protocols come back with a note instead of a value.
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
