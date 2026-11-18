#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""UDS / DoIP conformance of OpenTune's simulated ECU against independent clients.

python-udsoncan (MIT) is the UDS *client* and python-doipclient (MIT) the DoIP
*client*; the *server* is `opentune-udssim` behind the `doip_tcp_entity` example
(a loopback TCP bridge in front of `DoIpSimStream`). Each case starts a fresh
entity with the options it needs, drives it with the third-party clients and
checks the ISO 14229-1 / ISO 13400-2 behaviour. Offline only: 127.0.0.1.

    python3 tools/conformance/udsoncan_harness.py --entity <path/to/doip_tcp_entity>

See README.md next to this file.
"""
import argparse
import json
import os
import subprocess
import sys
import threading
import time
import traceback

import udsoncan
import udsoncan.configs
from udsoncan import DynamicDidDefinition, MemoryLocation
from udsoncan.client import Client
from udsoncan.exceptions import NegativeResponseException
from doipclient import DoIPClient
from doipclient.connectors import DoIPClientUDSConnector

ECU = 0x0010
TESTER = 0x0E00
VIN = "WVWZZZ00000000000"  # placeholder, as seeded by the entity
MASK = 0xA5  # the simulator's documented test seed->key relationship

CASES = []


def case(*entity_args):
    """Register a test case run against a fresh entity started with `entity_args`."""

    def deco(fn):
        CASES.append((fn.__name__, list(entity_args), fn))
        return fn

    return deco


class Entity:
    """The simulated ECU as a DoIP entity on an ephemeral loopback port."""

    def __init__(self, exe, args):
        self.proc = subprocess.Popen([exe, "--port", "0", *args], stdout=subprocess.PIPE, text=True)
        line = self.proc.stdout.readline().split()
        assert line and line[0] == "LISTENING", f"entity did not start: {line}"
        self.port = int(line[1])
        self.events = []
        self._t = threading.Thread(target=self._drain, daemon=True)
        self._t.start()

    def _drain(self):
        for line in self.proc.stdout:
            self.events.append(line.strip())

    def wait_event(self, prefix, timeout=2.0):
        end = time.time() + timeout
        while time.time() < end:
            hit = [e for e in self.events if e.startswith(prefix)]
            if hit:
                return hit
            time.sleep(0.01)
        return []

    def stop(self):
        self.proc.terminate()
        self.proc.wait(timeout=5)


def xor_key(level, seed, params=None):
    return bytes(b ^ MASK for b in seed)


def wrong_key(level, seed, params=None):
    return bytes(len(seed))


def doip(entity, activation=0, tester=TESTER):
    return DoIPClient("127.0.0.1", ECU, tcp_port=entity.port, client_logical_address=tester,
                      activation_type=activation)


def uds(entity, **overrides):
    cfg = dict(udsoncan.configs.default_client_config)
    cfg.update({
        "data_identifiers": {
            0xF190: udsoncan.AsciiCodec(17),
            0xF18C: udsoncan.AsciiCodec(7),
            0x1000: ">I",
            0x1234: ">H",
            0xF200: ">H",
            0xF201: ">I",
        },
        "security_algo": xor_key,
        "server_address_format": 32,
        "server_memorysize_format": 32,
        "request_timeout": 3,
    })
    cfg.update(overrides)
    client = Client(DoIPClientUDSConnector(doip(entity)), config=cfg)
    client.open()
    return client


def expect_nrc(code, fn, *args, **kw):
    try:
        fn(*args, **kw)
    except NegativeResponseException as e:
        got = e.response.code
        assert got == code, f"expected NRC {code:#04x}, got {got:#04x}"
        return
    raise AssertionError(f"expected NRC {code:#04x}, got a positive response")


def raw(client, payload, timeout=2):
    """Send raw UDS bytes and return the raw answer (or None)."""
    client.conn.empty_rxqueue()
    client.conn.send(bytes(payload))
    try:
        return client.conn.wait_frame(timeout=timeout, exception=True)
    except Exception:
        return None


# --------------------------------------------------------------------- DoIP


@case()
def doip_routing_activation(e):
    c = doip(e)  # default activation in the constructor; raises if refused
    r = c.request_activation(0x01)  # WWH-OBD, same tester: accepted again
    assert r.response_code == 0x10, hex(r.response_code)
    assert r.logical_address == ECU and r.client_logical_address == TESTER
    c.close()


@case()
def doip_routing_activation_unsupported_type(e):
    c = doip(e, activation=None)
    r = c.request_activation(0x02)
    assert r.response_code == 0x06, f"unsupported type: {r.response_code:#04x}"
    c.close()


@case()
def doip_second_tester_on_socket_refused(e):
    c = doip(e)
    c._client_logical_address = 0x0E01
    r = c.request_activation(0x00)
    assert r.response_code == 0x02, f"other tester: {r.response_code:#04x}"
    c.close()


@case()
def doip_alive_check_tester_initiated(e):
    c = doip(e)
    r = c.request_alive_check()
    assert r.source_address == ECU, hex(r.source_address)
    c.close()


@case("--alive-check")
def doip_alive_check_entity_initiated(e):
    client = uds(e)
    client.tester_present()  # doipclient answers the queued 0x0007 while reading
    client.close()
    hits = e.wait_event("ALIVE_CHECK_RESPONSE")
    assert hits == [f"ALIVE_CHECK_RESPONSE {TESTER:#06X}".replace("0X", "0x")], hits


@case()
def doip_diagnostic_before_activation_nacked(e):
    c = doip(e, activation=None)
    try:
        c.send_diagnostic(bytearray(b"\x3e\x00"))
    except IOError as err:
        assert "code: 2" in str(err), err
    else:
        raise AssertionError("diagnostic message accepted before routing activation")
    c.close()


@case()
def doip_unknown_target_nacked(e):
    c = doip(e)
    try:
        c.send_diagnostic_to_address(0x0099, bytearray(b"\x3e\x00"))
    except IOError as err:
        assert "code: 3" in str(err), err
    else:
        raise AssertionError("diagnostic message to an unknown target accepted")
    c.close()


@case()
def doip_unknown_payload_type_generic_nack(e):
    c = doip(e)
    c.send_doip(0x1234, b"")
    try:
        c.read_doip(timeout=1)
    except IOError as err:
        assert "NACK Code: 1" in str(err), err
    else:
        raise AssertionError("no generic NACK for an unknown payload type")
    c.close()


# ---------------------------------------------------------------------- UDS


@case()
def uds_session_control_and_timing(e):
    client = uds(e)
    for s in (0x03, 0x02, 0x01):
        r = client.change_session(s)
        assert r.service_data.session_echo == s
        assert r.service_data.p2_server_max == 0.050, r.service_data.p2_server_max
        assert r.service_data.p2_star_server_max == 5.0, r.service_data.p2_star_server_max
    expect_nrc(0x12, client.change_session, 0x40)
    client.close()


@case()
def uds_tester_present_and_suppressed_responses(e):
    client = uds(e)
    assert client.tester_present().positive
    with client.suppress_positive_response:
        assert client.tester_present() is None
        client.change_session(0x03)
    # No stray (suppressed) answers left on the wire: the next read correlates.
    assert client.read_data_by_identifier(0xF190).service_data.values[0xF190] == VIN
    assert raw(client, [0x3E, 0x01]) == b"\x7f\x3e\x12", "reserved TesterPresent sub-function"
    client.close()


@case()
def uds_ecu_reset(e):
    client = uds(e)
    assert client.ecu_reset(0x01).service_data.reset_type_echo == 0x01
    r = client.ecu_reset(0x04)
    assert r.service_data.reset_type_echo == 0x04 and r.service_data.powerdown_time == 0xFF
    expect_nrc(0x12, client.ecu_reset, 0x06)
    client.close()


@case()
def uds_security_access_unlock(e):
    client = uds(e)
    client.change_session(0x03)
    r = client.unlock_security_access(0x01)
    assert r.service_data.security_level_echo == 0x02
    # Already unlocked: an all-zero seed, udsoncan sends no key.
    r = client.unlock_security_access(0x01)
    assert r.service_data.seed == b"\x00" * 4
    client.close()


@case("--max-attempts", "2", "--sec-delay-ms", "300")
def uds_security_invalid_key_exceeded_and_delay(e):
    client = uds(e, security_algo=wrong_key)
    expect_nrc(0x35, client.unlock_security_access, 0x01)
    expect_nrc(0x36, client.unlock_security_access, 0x01)
    expect_nrc(0x37, client.request_seed, 0x01)
    time.sleep(0.35)
    client.set_config("security_algo", xor_key)
    client.unlock_security_access(0x01)
    client.close()


@case()
def uds_security_key_without_seed(e):
    client = uds(e)
    expect_nrc(0x24, client.send_key, 0x02, b"\x00\x00\x00\x00")
    client.close()


@case()
def uds_read_data_by_identifier(e):
    client = uds(e)
    assert client.read_data_by_identifier(0xF190).service_data.values[0xF190] == VIN
    vals = client.read_data_by_identifier([0xF190, 0xF18C]).service_data.values
    assert vals == {0xF190: VIN, 0xF18C: "SIM0001"}, vals
    expect_nrc(0x31, client.read_data_by_identifier, 0x1234)
    assert raw(client, [0x22, 0xF1]) == b"\x7f\x22\x13", "malformed RDBI"
    assert raw(client, [0xBA, 0x00]) == b"\x7f\xba\x11", "unknown service"
    client.close()


@case("--secure-write")
def uds_write_data_by_identifier_needs_security(e):
    client = uds(e)
    expect_nrc(0x33, client.write_data_by_identifier, 0xF18C, "SIM0002")
    client.change_session(0x03)
    client.unlock_security_access(0x01)
    client.write_data_by_identifier(0xF18C, "SIM0002")
    assert client.read_data_by_identifier(0xF18C).service_data.values[0xF18C] == "SIM0002"
    client.close()


@case("--restrict", "0x2E:3")
def uds_service_not_supported_in_active_session(e):
    client = uds(e)
    expect_nrc(0x7F, client.write_data_by_identifier, 0xF18C, "SIM0003")
    client.change_session(0x03)
    client.write_data_by_identifier(0xF18C, "SIM0003")
    client.change_session(0x01)
    expect_nrc(0x7F, client.write_data_by_identifier, 0xF18C, "SIM0004")
    client.close()


@case()
def uds_routine_control(e):
    client = uds(e)
    for fn, kind in ((client.start_routine, 1), (client.stop_routine, 2), (client.get_routine_result, 3)):
        r = fn(0x0203)
        assert r.service_data.control_type_echo == kind and r.service_data.routine_id_echo == 0x0203
        assert r.service_data.routine_status_record == b"\x00"
    expect_nrc(0x31, client.start_routine, 0x9999)
    assert raw(client, [0x31, 0x04, 0x02, 0x03]) == b"\x7f\x31\x12", "reserved control type"
    client.close()


@case()
def uds_download_transfer_exit_sequence(e):
    client = uds(e)
    loc = MemoryLocation(0x3000, 8, 32, 32)
    r = client.request_download(loc)
    assert r.service_data.max_length == 0x0402, r.service_data.max_length
    client.transfer_data(1, b"\x01\x02\x03\x04")
    expect_nrc(0x22, client.request_download, loc)  # one transfer at a time
    client.transfer_data(1, b"\x01\x02\x03\x04")  # retransmit of the previous block
    expect_nrc(0x73, client.transfer_data, 3, b"\x05")
    expect_nrc(0x71, client.transfer_data, 2, b"\x05\x06\x07\x08\x09")
    expect_nrc(0x24, client.request_transfer_exit)  # 4 of 8 bytes so far
    client.transfer_data(2, b"\x05\x06\x07\x08")
    client.request_transfer_exit()
    expect_nrc(0x24, client.transfer_data, 3, b"\x00")
    expect_nrc(0x24, client.request_transfer_exit)
    assert client.read_memory_by_address(loc).service_data.memory_block == bytes(range(1, 9))
    client.request_upload(loc)
    assert client.transfer_data(1).service_data.parameter_records == bytes(range(1, 9))
    client.request_transfer_exit()
    client.close()


@case("--secure-transfer")
def uds_download_needs_security(e):
    client = uds(e)
    expect_nrc(0x33, client.request_download, MemoryLocation(0x3000, 4, 32, 32))
    client.change_session(0x03)
    client.unlock_security_access(0x01)
    client.request_download(MemoryLocation(0x3000, 4, 32, 32))
    client.close()


@case()
def uds_dynamically_define_data_identifier(e):
    client = uds(e)
    client.dynamically_define_did(0xF200, DynamicDidDefinition(source_did=0x1000, position=2, memorysize=2))
    # (a struct-format codec decodes to a tuple)
    assert client.read_data_by_identifier(0xF200).service_data.values[0xF200] == (0x2030,)
    client.dynamically_define_did(0xF201, MemoryLocation(0x2004, 4, 32, 8))
    assert client.read_data_by_identifier(0xF201).service_data.values[0xF201] == (0x04050607,)
    client.do_clear_dynamically_defined_did(0xF200)
    expect_nrc(0x31, client.read_data_by_identifier, 0xF200)
    expect_nrc(0x31, client.dynamically_define_did, 0xF202,
               DynamicDidDefinition(source_did=0x7777, position=1, memorysize=1))
    client.do_clear_dynamically_defined_did()
    expect_nrc(0x31, client.read_data_by_identifier, 0xF201)
    client.close()


@case("--pending", "0x31:3", "--pending", "0x22:2")
def uds_response_pending_is_waited_out(e):
    client = uds(e)
    assert client.start_routine(0x0203).service_data.routine_status_record == b"\x00"
    assert client.read_data_by_identifier(0xF190).service_data.values[0xF190] == VIN
    client.close()


@case("--nrc", "0x31:0x22", "--nrc", "0x22:0x31")
def uds_injected_negative_responses(e):
    client = uds(e)
    expect_nrc(0x22, client.start_routine, 0x0203)
    expect_nrc(0x31, client.read_data_by_identifier, 0xF190)
    # Each fault fires once: the next requests are answered normally.
    client.start_routine(0x0203)
    client.read_data_by_identifier(0xF190)
    client.close()


@case()
def uds_dtc_services(e):
    client = uds(e)
    assert client.get_number_of_dtc_by_status_mask(0xFF).service_data.dtc_count == 0
    assert client.get_dtc_by_status_mask(0xFF).service_data.dtcs == []
    client.clear_dtc()
    client.close()


@case("--flash")
def uds_flash_programming_sequence(e):
    client = uds(e)
    expect_nrc(0x22, client.change_session, 0x02)  # programming only from extended
    client.change_session(0x03)
    client.change_session(0x02)
    client.unlock_security_access(0x11)
    # eraseMemory answers after two responsePending frames.
    erase = bytes([0x44]) + (0x10000).to_bytes(4, "big") + (0x100).to_bytes(4, "big")
    assert client.start_routine(0xFF00, erase).service_data.routine_status_record == b"\x00"
    loc = MemoryLocation(0x10000, 0x100, 32, 32)
    mbl = client.request_download(loc).service_data.max_length
    data = bytes((i * 7) & 0xFF for i in range(0x100))
    step = mbl - 2  # maxNumberOfBlockLength counts the SID and the counter
    for n, off in enumerate(range(0, len(data), step), start=1):
        client.transfer_data(n & 0xFF, data[off:off + step])
    client.request_transfer_exit()
    assert client.read_memory_by_address(loc).service_data.memory_block == data
    client.ecu_reset(0x01)
    client.close()


# ------------------------------------------------------------------- runner


def default_entity():
    tgt = os.environ.get("CARGO_TARGET_DIR") or os.path.join(os.path.dirname(__file__), "..", "..", "target")
    return os.path.join(tgt, "debug", "examples", "doip_tcp_entity")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--entity", default=default_entity(), help="path to the built doip_tcp_entity example")
    ap.add_argument("-k", default="", help="only run cases whose name contains this")
    ap.add_argument("--json", help="also write the results as JSON here")
    ap.add_argument("-v", action="store_true", help="print tracebacks of failures")
    a = ap.parse_args()
    if not os.path.exists(a.entity):
        sys.exit(f"entity not found: {a.entity} (cargo build -p opentune-udssim --example doip_tcp_entity)")
    results = []
    for name, args, fn in CASES:
        if a.k not in name:
            continue
        ent = Entity(a.entity, args)
        t0 = time.time()
        try:
            fn(ent)
            ok, msg = True, ""
        except Exception as err:  # noqa: BLE001 - every failure is a result
            ok, msg = False, f"{type(err).__name__}: {err}"
            if a.v:
                traceback.print_exc()
        finally:
            ent.stop()
        results.append({"case": name, "ok": ok, "error": msg, "seconds": round(time.time() - t0, 3)})
        print(f"{'PASS' if ok else 'FAIL'}  {name}{'  -- ' + msg if msg else ''}", flush=True)
    passed = sum(r["ok"] for r in results)
    print(f"\n{passed}/{len(results)} cases passed "
          f"(udsoncan {udsoncan.__version__ if hasattr(udsoncan, '__version__') else '?'})")
    if a.json:
        with open(a.json, "w") as f:
            json.dump(results, f, indent=2)
    sys.exit(0 if passed == len(results) else 1)


if __name__ == "__main__":
    main()
