# Safety & scope

Writing to an ECU can brick it. OpenTune is built so that no write skips the
safety sequence, and so that the free edition stays within a clear, lawful scope.

## The flash safety contract

No driver writes to an ECU directly. Every write goes through one orchestrator
that enforces, in order:

1. **Voltage gate** — refuse to flash below a minimum battery voltage. A dropout mid-write is the classic way to brick a module. The gate also refuses if an interface that *can* measure voltage returns no reading.
2. **Verified backup** — read the region back into a buffer first, and abort if the backup read is short. An original is never a single unsaved buffer.
3. **Write**.
4. **Read-back verify** — read the region again and compare it byte-for-byte to what was written; any mismatch is an error, not a warning.

These rules live in the core, so no driver and no app can opt out of them.

## Your own habits

- Keep **two** good backups before any write, in the **Vault** with their CRC-32 sidecars.
- Re-run **Checksum → Verify** after every edit and every fix.
- Never firmware-"update" a clone interface — it can erase the device.
- Work on a car with a solid battery or a bench supply; watch the voltage gate.

## Scope of this edition

OpenTune (this app) is **calibration-only**. It identifies ECUs, verifies and
fixes checksums, decodes and diffs maps, and runs the safety-gated flash. It does
**not** include immobiliser, EEPROM, or engine-standalone tooling.

It does **not** defeat an immobiliser, disable an emissions-control system, or
remove safety monitors. Use it only on an ECU and a vehicle you are authorised to
work on, and keep road vehicles road-legal. The project ships this edition openly
so people can learn how an ECU image is structured and calibrated — responsibly.
