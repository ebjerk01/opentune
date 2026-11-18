# UDS / DoIP conformance vs. python-udsoncan

`udsoncan_harness.py` checks OpenTune's simulated ECU (`opentune-udssim`) against
**independent, third-party clients**: [python-udsoncan] (MIT) as the ISO 14229 UDS
client and [python-doipclient] (MIT) as the ISO 13400 DoIP client. The Rust
conformance tests (`crates/opentune-udssim/tests/`) drive the simulator with
OpenTune's *own* client; this harness closes the loop from the other side, so a
simulator that merely agrees with OpenTune's assumptions is caught.

The server runs as the `doip_tcp_entity` example: a loopback TCP bridge in front
of `DoIpSimStream` (127.0.0.1, ephemeral port, one fresh ECU per connection),
plus the entity's UDP side (`DoIpUdpEntity`, its own ephemeral port; the
example prints `LISTENING <tcp> UDP <udp>`).
Each case starts its own entity with the options it needs (fault injection,
security policy, session restrictions, the flash model). Offline only.

## Run

```sh
python3 -m venv ~/udsvenv && ~/udsvenv/bin/pip install -r tools/conformance/requirements.txt
cargo build -p opentune-udssim --example doip_tcp_entity
~/udsvenv/bin/python3 tools/conformance/udsoncan_harness.py \
    --entity "${CARGO_TARGET_DIR:-target}/debug/examples/doip_tcp_entity"
```

`-k <text>` runs only matching cases, `-v` prints tracebacks, `--json out.json`
saves the results. Exit status is non-zero when a case fails.
`scripts/check.sh` runs it as its last gate when `OPENTUNE_CONFORMANCE_PYTHON`
(or `python3`) can import both packages, and skips it with a notice otherwise; CI
runs it in the `conformance` job.

## Coverage (34 cases)

| Area | Cases |
|---|---|
| DoIP routing activation | default + WWH-OBD accepted; unsupported type → `0x06`; a second tester on the socket → `0x02` |
| DoIP alive check | tester-initiated (`request_alive_check`); entity-initiated, answered by doipclient while it waits for a UDS reply |
| DoIP NACKs | diagnostic message before activation → diagnostic NACK `0x02`; unknown target → `0x03`; unknown payload type → generic NACK `0x01` |
| DoIP over UDP | vehicle identification (all, by EID, by VIN; another VIN gets no answer) checked field by field (VIN, logical address, EID, GID, further action, sync status); unsolicited vehicle announcement (`await_vehicle_announcement`); entity status; diagnostic power mode; bad request headers / TCP-only type / bad length → generic NACK `0x00` / `0x01` / `0x04`; injected faults: NACK surfaces as doipclient's `IOError`, a bad-version reply is dropped by doipclient's parser |
| 0x10 / 0x3E / 0x11 | session echo + P2/P2* timing record; unsupported session `0x12`; suppressPosRspMsgIndicationBit (no answer, nothing stray on the wire); reserved TesterPresent / ECUReset sub-functions `0x12`; rapid power shutdown time |
| 0x27 | unlock; already-unlocked zero seed; invalid key `0x35`; exceeded attempts `0x36`; delay timer `0x37`, then unlock; key without seed `0x24` |
| 0x22 / 0x2E | single and multi-DID read; unknown DID `0x31`; malformed `0x13`; unknown service `0x11`; write needs security `0x33`; service not in active session `0x7F` |
| 0x31 | start / stop / results echoes and status record; unknown routine `0x31`; reserved control type `0x12` |
| 0x34 / 0x36 / 0x37 / 0x35 / 0x23 | maxNumberOfBlockLength; second request while active `0x22`; retransmit of the previous block accepted; wrong counter `0x73`; too much data `0x71`; early exit `0x24`; transfer data / exit after exit `0x24`; read-back by 0x23 and upload; download needs security `0x33` |
| 0x2C | define by identifier and by memory address, read back, clear one / all, bad source `0x31` |
| 0x78 | responsePending storms on 0x31 and 0x22 waited out by udsoncan |
| Injected NRCs | one-shot faults surface as `NegativeResponseException` with the right code, then normal service |
| 0x19 / 0x14 | DTC count / list, clear |
| Flash model | programming session only from extended (`0x22`), level-0x11 unlock, eraseMemory behind two `0x78`, download in `maxNumberOfBlockLength - 2` chunks, read-back, reset |

## Findings (2026-11-18)

Against `main` at 0b6455b the harness passed 12/25 cases. The deviations, all
fixed in `opentune-udssim` with Rust regression tests, now 25/25:

- **DoIP entity** accepted every routing activation type and any tester,
  answered diagnostic messages before routing activation and for any target
  address, ignored unknown payload types and tester-initiated alive checks, and
  sent an empty diagnostic message for a response the ECU drops (the flash
  model's lost response / power loss). It also did not interpose the flash
  model's own responsePending frames (slow erase) the way `SimTransport` does.
- **suppressPosRspMsgIndicationBit** was ignored (a stray positive answer on
  the wire), and SecurityAccess took bit 7 as part of the level.
- **TesterPresent / ECUReset** accepted reserved sub-functions (`0x12` now).
- **ReadDataByIdentifier** rejected multi-DID requests with `0x13`.
- **Transfers**: TransferData / RequestTransferExit were still accepted after
  RequestTransferExit; a retransmitted previous block was `0x73` (ISO: accept
  without rewriting); more data than announced was accepted (`0x71`); exit
  before the announced size arrived was accepted (`0x24`); a second
  RequestDownload/Upload while one was active silently replaced it (`0x22`).
- **SecurityAccess delay** (`0x37`) and **session-restricted services**
  (`0x7F`) could not be modelled; both are now opt-in
  (`UdsServer::set_security_delay`, `UdsServer::restrict_service_to_sessions`).

Compatibility: `UdsServer::handle` still returns the computed response even for
a suppressed request (the virtual car applies the bit itself); `respond`,
`SimTransport` and `DoIpSimStream` model the wire and send nothing.

[python-udsoncan]: https://github.com/pylessard/python-udsoncan
[python-doipclient]: https://github.com/jacobschaer/python-doipclient
