# Unit access profiles and wiring diagrams

Every control unit in the database (`database/units/*.json`, crate
`opentune-db`) describes how it can be read and wired. This page covers the
**wiring diagram** that the app draws from that data. The structured `access`
block (OBD / bench / boot access, boot entry, security) is being added next and
will be documented here.

## What "confidence" means

Pins carry a confidence (their own, else the unit's `pinout_confidence`):

| Level | Meaning |
|---|---|
| `verified` | **Two or more independent sources agree.** It does **not** mean the pin was measured on a physical unit. The validator enforces at least two distinct sources. |
| `community` | One source. |
| `unverified` | Disputed between sources, or a single conflicting source. |

A unit's `pinout_confidence` may not be higher than its weakest bench pin.

## Wiring diagram (data-driven)

`desktop/ui/src/pinout-svg.js` (`OTPinout.svg(unit)`) turns a unit's
`connectors[].pins[]` into an inline SVG, so every unit with pins gets a diagram
for free and it never drifts from the data.

- **It is a wiring diagram, not a connector face.** Sources give pin numbers and
  functions, not physical positions, so the diagram does not pretend to know
  the layout.
- **Left:** one block per connector, pins in numeric order, labelled
  `<connector>-<pin>` with the function text (full text and the pin note in a
  tooltip).
- **Right:** the bench targets some pin needs, in a fixed order: +12 V
  permanent (KL30), +12 V ignition (KL15), ground, K-line, CAN high, CAN low,
  boot strap. Pins with signal `other` get no wire.
- **Wires** are coloured by signal (theme tokens, so light and dark mode both
  work; standalone it falls back to fixed colours).
- **Confidence:** a pin with two or more distinct sources shows "2 src ✓";
  an `unverified` pin is highlighted, marked **?** and its wire is **dashed**
  ("disputed — confirm by continuity"). The legend states the unit's level and
  that "verified" means sources agree.

Where it appears:

- **ECU database** panel: above the pin table of every unit with pins.
- **Guide pages:** a fenced block

  ````markdown
  ```pinout bosch-edc15c6-mercedes
  ```
  ````

  renders as that unit's diagram (`OTUtil.renderMarkdown` emits a placeholder,
  `OTPinout.hydrate` fills it from `db_get`). Unknown ids show a clear message.

## Sources

Public sources only (pinout sites, datasheets, public forums, open projects),
facts only, each cited in the unit's `sources` and referenced per pin. No data
from commercial-tool internals, leaked manuals or private research.
