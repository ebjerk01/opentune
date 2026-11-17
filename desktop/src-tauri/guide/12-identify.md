# Identify

Read the ECU identity from a dump, match it to a bundled OpenTune definition,
and list vehicle-catalog candidates. You can also type a part or software number
by hand.

## What you need

- For **Identify opened image**: a dump opened in **Image**.
- For **Match a part / software number**: only the number. No file needed.
- No adapter and no car.

## Step by step

1. Open a dump in **Image**.
2. Press **Identify opened image**. The card shows ECU name, HW #, SW #, VIN and size, then the definition match and catalog candidates.
3. To match by number, type the hardware number (HW #, for example a Bosch `0281…` number) and/or software number (SW #) and press **Match**. The example chips fill in sample numbers and run the match.

## Reading the results

- **Matched definition** (green) — a bundled definition fits. It shows the id, maker and family, protocol, the **checksum** family to use in the Checksum panel, and the seed/key reference if the definition names one.
- **No OpenTune definition matched** (blue) — there is no detailed definition for this ECU yet. Catalog candidates can still narrow down the car.
- **Catalog candidates** — make, models, years and family from the bundled vehicle catalog. These are candidates, not a confirmed car.

## Common errors

- **SW # shows a byte count** — offline, the file driver only knows the file name and size, so it reports the size where a real ECU would report its software number. Use **Match a part / software number** with the numbers from the ECU label, or use **Analyze bin**, which reads identity strings from inside the image.
- **"Open an image first"** — open a dump in **Image**.

## Safety

Read-only. Nothing is written.
