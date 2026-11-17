# Analyze bin

One button that tells you everything OpenTune can work out from a raw dump,
offline: ECU family by structure, identity strings, catalog and definition
matches, checksum status and known maps. It is **early alpha** and read-only.

## What you need

- A flash dump. Either the opened image (Image panel) or any other file.
- No adapter and no car.

## Step by step

1. Open a dump in **Image**, then press **Analyze opened image**. Or press **Analyze another…** to analyze a different file. This does not change the opened image.
2. Wait for the result card. Large files take a few seconds.

## Reading the results

- **Assignment** — the car OpenTune thinks this dump belongs to, with a confidence pill (**High** in green, **None** in red, others in between) and the **basis** for it. A car is only named when a database match or a VIN supports it. "Not determinable offline" is an honest answer, not an error.
- **ECU family** — guessed from the file's structure and size, with the **family signals** that led to it.
- **VAG part #, Bosch #, Software #, Mercedes #** — identity strings found in the image.
- **VIN** — if one is stored in the image, plus the make and model year decoded from it.
- **ASAM/EPK markers** — how many calibration-identity markers were found.
- **Part number** — a breakdown of the main part number (supplier, VAG groups, Bosch family) when it can be decoded.
- **Catalog / matched def** — a vehicle-catalog entry and a bundled OpenTune definition, when either matches.
- **Checksum blocks** — verified with the algorithm for the detected family. Green **OK**, red **BAD**. "No checksum algorithm for family …" means this edition has no checksum for that family.
- **Known maps** — maps from the matched definition, with size, address and unit.

## Common errors

- **Family: Unknown** — the structure did not match a known family. Try **Identify** with a part number you read off the ECU label.
- **Checksum BAD on a stock file** — the family guess or the dump may be wrong (for example a partial read). Do not "fix" a file you have not edited.

## Safety

Read-only. Nothing is written. Treat every result as a lead to check, not as a
fact, especially at low confidence.
