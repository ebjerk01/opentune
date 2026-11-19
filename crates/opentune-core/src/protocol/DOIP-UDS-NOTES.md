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
  Since batch 4 it wraps `doip_discovery::DoIpUdpClient` (below).

### Discovery, entity status, power mode (`doip_discovery`)

- `DoIpUdpClient::identify` / `identify_first`: vehicle identification request
  `0x0001` (all), `0x0002` (by EID) or `0x0003` (by VIN) — `VehicleIdRequest` —
  to unicast, IPv4 broadcast or IPv6 `ff02::1` (with a scope id), collecting
  `0x0004` answers into a `Discovery`: one `DiscoveredEntity` per (IP, logical
  address, EID), repeats counted in `duplicates`, generic NACKs in `nacks`,
  malformed datagrams in `rejected`, other messages in `ignored`.
- `DoIpUdpClient::listener` + `listen`: unsolicited announcements on the
  discovery port (`[::]` also joins `ff02::1`).
- `entity_status` (`0x4001` → `0x4002`: node type, max / open TCP sockets,
  optional max data size) and `power_mode` (`0x4003` → `0x4004`). A generic
  NACK is an error naming the code; a timeout during which only malformed
  answers arrived says so.
- Header validation shared with the simulator: `doip::check_header` (version
  and inverse, versions 0x01-0x03, 0xFF for identification requests only, max
  payload) and `doip_discovery::parse_datagram` (exactly one message per
  datagram, per-type payload length from `doip::payload_len_valid`), returning
  the generic NACK code an entity answers with.
- `connect_entity(&DiscoveredEntity, &ConnectOptions)`: TCP connect to the
  entity's address (scope id kept) on the data port, routing activation with
  the default type — or central security `0xE0` when the entity announced
  further action `0x10` — targeting the announced logical address. Returns the
  activated `DoIpTransport` for a `UdsClient`.
- Entity side: `opentune-udssim::doip_udp` (`DoIpUdpEntity`, `UdpEntityServer`,
  `TcpEntityServer`) with no-reply / bad-version / NACK fault injection; the
  integration test `crates/opentune-udssim/tests/doip_discovery.rs` and the
  python-doipclient cases in `tools/conformance/` drive it. CLI:
  `opentune doip discover|status|power` (`--sim` for the simulated entity).
- 🔒 Broadcast / multicast on a real vehicle network and a real gateway's
  answers are untested.
- **Routing activation hardening.** `activate_with(type, …)` allows non-default
  activation types (WWH-OBD, central security). The entity's logical address is
  learned from the response (`entity_address()`). The response is now validated
  in full: the fixed 9-byte form is required, and the entity must echo our tester
  (source) logical address (`routing_activation_rejects_wrong_tester_echo`).
  Response code 0x11 ("activated — confirmation required / further action") is
  accepted but recorded **distinctly** from 0x10 via
  `activation_confirmation_required()`, so a caller is not misled into treating
  0x11 as an unconditional success. All routing / generic-NACK / diagnostic-NACK
  codes carry ISO names in error messages (`routing_activation::name`,
  `generic_nack::name`, `diag_nack::name`).
- **Diagnostic-message correlation.** Diagnostic messages and their pos/neg acks
  are now address-correlated: the reply target must be our tester address and the
  source must be the ECU/entity we addressed, else the message is rejected
  (`diagnostic_reply_to_wrong_tester_is_rejected`,
  `diagnostic_reply_from_wrong_source_is_rejected`). Pos/neg acks must carry the
  full source+target+code (5 bytes); a short ack is rejected
  (`short_diagnostic_positive_ack_is_rejected`).
- **Absolute monotonic deadlines.** The responsePending (0x78), positive-ack,
  alive-check and vehicle-discovery loops now share one absolute monotonic
  deadline per exchange (`TOTAL_DIAG_TIMEOUT_MS`, with `P2_STAR_MS` for the
  extended per-response window) instead of resetting a per-message timeout, so
  trickled traffic can't keep an exchange alive forever. (Batch 5: that cap is
  a ceiling only; the response window itself follows the caller's timeout, see
  "Response timing" below.) The UDS request is sent
  **once**; on 0x78 the transport keeps receiving without re-sending
  (`responsepending_does_not_resend_the_request`).
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
| RoutineControl | 0x31 | **FIXED** — now verifies the echoed `routineControlType` (low 7 bits) and the 2-byte `routineIdentifier`, and rejects a response too short to carry them; the optional `routineStatusRecord` no longer excuses a missing echo correlation. Returns the status record (may be empty). Runs under P2\*. |
| RequestDownload / RequestUpload | 0x34 / 0x35 | **FIXED** — `maxNumberOfBlockLength` parsing now also rejects a zero length-count, a truncated length field, and a zero block length (a 0-byte block would make the `transferData` loop meaningless). Layout (`dataFormatIdentifier` + `addressAndLengthFormatIdentifier` 0x44) unchanged. Runs under P2\*. |
| TransferData | 0x36 | **FIXED** — the echoed `blockSequenceCounter` in the 0x76 response is now verified against the counter sent (missing byte, wrong counter, and the 0xFF→0x00 wrap all covered); counter arithmetic alone is no longer treated as acknowledgement. Caller still owns the counter via `next_block_seq`. Runs under P2\*. |
| RequestTransferExit | 0x37 | OK. |
| TesterPresent | 0x3E | OK for the `3E 00` (respond) form. **Known gap:** the `3E 80` suppressPositiveResponse form is not offered because the current `DiagTransport` always waits for a response; adding it needs a "fire-and-forget" transport path (integration note below). |
| NRC table | — | **FIXED** — `nrc_name` extended from ~20 codes to the full ISO 14229 Annex A set udsoncan names: 0x26, 0x34, 0x38–0x3A, 0x50–0x5D (certificate/auth), and the 0x81–0x94 conditions band (rpm/engine/temperature/speed/voltage/resource). ISO-reserved (0x95–0xEF) and manufacturer-specific (0xF0–0xFE) codes stay "unknown". |

The sub-function echo check (`check_subfunction_echo`) compares only the low 7
bits, since ISO §8.6.3 has the server clear bit 7 (the suppressPositiveResponse
bit) in the echo.

**Timing model (ISO 14229-2 §7).** `UdsClient` now carries an explicit
P2 / P2\* / total-request budget (`P2_CLIENT_MS`, `P2_STAR_CLIENT_MS`,
`TOTAL_REQUEST_TIMEOUT_MS`) instead of one opaque timeout. Quick request/response
services use P2; the long-running programming services (routineControl,
request(Download|Upload), transferData, transferExit) use P2\*. Each request's
wait is clamped to the remaining total budget captured from a monotonic clock, so
no single exchange waits past the ceiling. The client issues every request exactly
once and never auto-retries — a 0x78 that reaches the client is surfaced, not
re-sent (`responsepending_at_client_is_not_resent`), so the NRC 0x78 receive loop
stays the transport's responsibility (where it re-reads without re-sending).

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
- The `doip` registry driver (`crates/drivers/doip`, `opentune-driver-doip`)
  wires this up: `DoIpUdpClient::identify` to find the entity (optional:
  `discover=1` / `vin=` / `eid=`), else a direct TCP dial; `connect_entity` or
  `DoIpTransport::activate_with` for routing activation; `Interface::diag()`
  hands the activated transport to every engine. `--driver doip` works for
  coding, adaptation, auto-scan, service, log and OBD inspection; options and
  the `HOST[:PORT][?key=value]` address string are in `docs/DRIVERS.md` §3i.
  Its loopback tests run those engines against the udssim TCP/UDP entity.
  `Capabilities::doip` marks it (it declares no `can`).
- Response timing (`DoIpTransport::request`, since batch 5): the wait for the
  UDS response honours the caller's per-request timeout (P2). Until the entity
  acknowledges the diagnostic message the window is at least
  `DIAG_ACK_TIMEOUT_MS` (2 s, ISO 13400-2 A_DoIP_Diagnostic_Message); a
  positive ack restarts it at the caller's timeout; a `responsePending` that
  echoes this request's service restarts it at P2\* (`P2_STAR_MS`, 5 s); a
  0x78 for another service is handed back, not waited on. Nothing extends the
  exchange past `TOTAL_DIAG_TIMEOUT_MS` (60 s, or the caller's timeout if
  longer). So a silent ECU behind an acknowledging gateway fails after the
  caller's timeout, and one with no ack at all after 2 s — previously both held
  an engine for 60 s. Tested against the udssim TCP entity's silent-target mode
  (`DoIpSimStream::set_silent`, `crates/opentune-udssim/tests/doip_timeout.rs`).
  A target the entity does not know is answered at once with diagnostic NACK
  `0x03` (an absent module in a scan).
