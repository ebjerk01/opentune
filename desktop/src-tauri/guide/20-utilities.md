# Utilities

Small offline decoders. **Early alpha.**

## What you need

Nothing but the number or code to decode. No file, no adapter, no car.

## VIN decoder

Type a 17-character VIN and press **Decode**. You get the manufacturer
identifier (WMI), country, the descriptor and serial sections, the check digit
and whether it is valid, and the model year. Many European VINs do not use the
check digit, so "invalid" there is normal.

## Part-number decoder

Type a part number (for example a VAG `03G906…` number or a Bosch `0281…`
number) and press **Decode**. It splits the number into its known fields and
flags anything unusual.

## DTC lookup

Type a fault code such as `P0101` and press **Look up**. It shows the system
(powertrain, chassis, body, network) and a generic description.
Manufacturer-specific codes may only get a generic description.

## Seed / key calculator

Computes a key from a seed with simple, **generic** transforms you choose and
set: identity, XOR mask, add constant, XOR then add, and reverse byte order.

This edition ships **no OEM or vehicle-specific** seed/key routines. These
transforms will not unlock a real ECU unless its algorithm happens to be one of
them. They exist for learning and for testing against the offline simulator.

## Safety

All four tools are offline and read-only.
