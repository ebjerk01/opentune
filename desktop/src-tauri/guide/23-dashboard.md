# Dashboard

Live gauges for the engine over generic OBD-II (mode 01): RPM, engine load,
coolant temperature, intake air temperature, manifold pressure / boost, MAF,
exhaust gas temperature (bank 1 and 2), fuel rail pressure and battery voltage.

## What you need

- **Adapter: CAN**, connected and detected.
- **Ignition on** for most values; **engine running** for RPM, load, MAF, boost and EGT to mean anything.
- The dongle's **HS/MS-CAN switch on HS**. The engine ECU is on HS-CAN.

## Step by step

1. Set the adapter in the sidebar and press **Detect adapter**.
2. Open **Dashboard** and press **Start**.
3. The panel first asks the ECU which values it supports, then shows a gauge for each supported one.
4. Press **Stop** before using another live panel.

## Recording

Press **● Record** to write the polled values to a CSV file. Pick where to save
it; recording starts the dashboard if it is not running yet. While it runs the
bar shows **REC**, the elapsed time, the number of samples and the file name.
Press **■ Stop recording** to finish.

- The file is a **Live-log CSV** (`ts,param_id,name,raw,value,units,note,text`,
  one row per value per sample, time in ms): open it in the **Log viewer** to
  plot it or overlay it on a map. Multi-sensor values (EGT) get one channel per
  sensor; the VIN goes into the `text` column.
- Rows are written about once a second, so a crash loses at most the last second.
- Recording stops by itself when you switch to another panel, the adapter
  disconnects, polling fails three times in a row, or **Stop** is pressed. The
  bar then says how many samples went into which file.

## Reading the results

- **Only supported values are shown.** Many engines do not report EGT, fuel rail pressure or MAF over generic OBD-II. A missing gauge means the ECU does not offer that value, not that the sensor is broken.
- **Colours** — a gauge turns to the warning colour, then the critical colour, past fixed thresholds. These are generic defaults, not limits for your engine.
- **VIN** — when the ECU reports it (mode 09, or UDS `F190`), the VIN is shown as a text tile at the start of the grid. Text tiles show the text itself, with no colour, sparkline or min/max.
- **MAP / boost** — manifold absolute pressure. Boost is the part above atmospheric pressure.
- Update speed depends on the adapter. An ELM327 reads only a few values per second, so more gauges means slower updates per gauge.

## Common errors

- **No gauges / all blank** — ignition off, wrong bus, or no adapter.
- **Adapter busy** — another panel is using the adapter. Stop it first.
- **Values freeze** — the adapter dropped out. Stop, press **Detect adapter**, and start again.

## Safety

- Read-only. Nothing is written to the ECU.
- Do not watch the screen while driving. Have a passenger operate the laptop, or log on a dyno or a closed road.
