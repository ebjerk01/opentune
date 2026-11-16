# DoIP transport + UDS conformance notes

Scope: `crates/opentune-core/src/protocol/` — the ISO 13400 (DoIP) transport and
the ISO 14229 (UDS) client. Covers build-order items **#7 (real DoIP)** and
**#6 (UDS conformance)**.

References studied (open-source, MIT): `pylessard/python-udsoncan` (the canonical
UDS reference), `zeloscloud/zelos-extension-uds` (its `tests/doip_entity.py` is a
standard-table DoIP node), `life-challenge/DiagTools` (`doip_layer.py`). All code
here is reimplemented from ISO tables / observed wire formats, not copied.

Everything below is exercised **offline** — in-memory mock streams, a localhost
TCP "gateway" thread, and a localhost UDP entity. No silicon is required. The
on-real-CAN/Ethernet paths (`DoIpTransport::connect` dialing a real gateway, and
`discover` broadcasting to `255.255.255.255`) are marked 🔒 untested against
hardware; their framing/parsing cores are the same code the offline tests drive.

## #7 — DoIP (ISO 13400-2) transport

The pre-existing mock (header + routing activation + diagnostic-message framing +
responsePending loop + loopback test) was kept and built on, not rewritten.

Added:

- **Vehicle discovery.** `VehicleInfo` parses a vehicle announcement /
  identification response (type 0x0004): VIN(17) + logical address(2) + EID(6) +
  GID(6) + further-action(1) + optional VIN/GID sync status(1), with
  `vin_string()` trimming NUL/space padding. `vehicle_identification_request()`
  builds the (empty) 0x0001 request.
- **TCP identify.** `DoIpTransport::identify()` sends a vehicle identification
  request on the open data socket and returns the parsed `VehicleInfo`, learning
  the entity's logical address.
- **UDP discovery.** `discover(bind, dest, timeout, broadcast)` sends one
  identification request and collects every announcement within the timeout.
  Offline-tested unicast to a localhost entity; the broadcast path is 🔒.
- **Routing activation hardening.** `activate_with(type, …)` allows non-default
  activation types (WWH-OBD, central security). The entity's logical address is
  learned from the response (`entity_address()`). Response code 0x11
  ("activated, confirmation required") is accepted alongside 0x10. All routing /
  generic-NACK / diagnostic-NACK codes now carry ISO names in error messages
  (`routing_activation::name`, `generic_nack::name`, `diag_nack::name`).
- **Alive check.** Entity-initiated alive-check requests (type 0x0007) that
  interleave during activation or while awaiting a diagnostic response are now
  answered (type 0x0008 with the tester source address) instead of aborting the
  exchange.
- Added `DEFAULT_PORT` (13400) and `FUNCTIONAL_GROUP_ADDRESS` (0xE400) constants.

## #6 — UDS (ISO 14229-1) conformance vs. python-udsoncan

Service-by-service review. "OK" = already matches udsoncan/ISO; "FIXED" = a
divergence corrected here with a regression test.

| Service | SID | Finding |
|---|---|---|
| DiagnosticSessionControl | 0x10 | **FIXED** — now verifies the session echo in the positive response (udsoncan raises `UnexpectedResponseException` on a mismatch). Request/response encoding was already correct. |
| ECUReset | 0x11 | **FIXED** — now verifies the reset-type echo. `enableRapidPowerShutDown`'s trailing powerDownTime byte is passed through. |
| SecurityAccess | 0x27 | **FIXED** — `request_seed`/`send_key` now verify the echoed access level (low 7 bits, per udsoncan's odd=RequestSeed / even=SendKey normalisation). Existing all-zero-seed "already unlocked" detection and the 0x35/0x36→SecurityDenied mapping were already conformant; 0x37 (requiredTimeDelayNotExpired) correctly stays a raw negative response. |
| ReadDataByIdentifier | 0x22 | **FIXED** — now verifies the echoed DID equals the requested DID before returning the record, so a mis-routed answer is not returned as the wrong parameter's data. (Single-DID only — multi-DID reads are a known feature gap, not a conformance bug.) |
| RoutineControl | 0x31 | OK — request layout (type + 2-byte routine id + data) matches. Response is returned raw (routine status record); echo not enforced, consistent with udsoncan treating routineInfo as optional. |
| RequestDownload / RequestUpload | 0x34 / 0x35 | OK — `dataFormatIdentifier` + `addressAndLengthFormatIdentifier` 0x44 (4-byte addr + 4-byte size) is a valid ISO layout; `maxNumberOfBlockLength` parsing (length-format high nibble) matches udsoncan. |
| TransferData | 0x36 | OK — block-sequence-counter semantics (start 0x01, wrap 0xFF→0x00, NRC 0x73 on mismatch) match ISO §14.3. Caller owns the counter via `next_block_seq`. |
| RequestTransferExit | 0x37 | OK. |
| TesterPresent | 0x3E | OK for the `3E 00` (respond) form. **Known gap:** the `3E 80` suppressPositiveResponse form is not offered because the current `DiagTransport` always waits for a response; adding it needs a "fire-and-forget" transport path (integration note below). |
| NRC table | — | **FIXED** — `nrc_name` extended from ~20 codes to the full ISO 14229 Annex A set udsoncan names: 0x26, 0x34, 0x38–0x3A, 0x50–0x5D (certificate/auth), and the 0x81–0x94 conditions band (rpm/engine/temperature/speed/voltage/resource). ISO-reserved (0x95–0xEF) and manufacturer-specific (0xF0–0xFE) codes stay "unknown". |

The sub-function echo check (`check_subfunction_echo`) compares only the low 7
bits, since ISO §8.6.3 has the server clear bit 7 (the suppressPositiveResponse
bit) in the echo.

## Integration notes

- No new crate dependencies. Everything uses `std::net` (TCP/UDP) already linked.
- New public API in `protocol::doip`: `VehicleInfo`, `vehicle_identification_request`,
  `discover`, `DoIpTransport::{identify, activate_with, entity_address}`, the
  `DEFAULT_PORT` / `FUNCTIONAL_GROUP_ADDRESS` constants, and the
  `generic_nack` / `diag_nack` / expanded `routing_activation` modules.
- Behavioural change to watch at integration: `read_data_by_id`, `session`,
  `ecu_reset`, `request_seed`, `send_key` now return `Error::Protocol` if an ECU
  echoes the wrong DID / sub-function. This is stricter than before — intended —
  but if a non-conformant ECU is encountered in the field, that is where to look.
- A CLI/driver wiring DoIP would: `DoIpTransport::connect(ip, DEFAULT_PORT, …)`
  (or `discover` first to find the entity + VIN), then `activate()`, then drive
  `UdsClient` as usual. A `--doip <ip>` flag is the natural surface; not added
  here (CLI is out of scope).
