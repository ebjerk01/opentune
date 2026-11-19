# Security Access and the Seed/Key Hook

ECUs lock the services that read or reflash memory behind **security access**
(UDS/KWP service `0x27`). Before `requestDownload` (`0x34`) — and, on many ECUs,
before a full `requestUpload` read (`0x35`) — the tester must prove it knows the
ECU's secret by answering a challenge. This document explains the handshake, how
OpenTune models it, and how a driver uses it.

> **OpenTune ships NO OEM seed/key algorithms.** The repository contains only the
> *mechanism* (the trait and the driver hook) and a **demo** transform used in
> tests. Users supply the real algorithm for their ECU. See
> [Legal and scope](#legal-and-scope).

---

## How UDS/KWP security access works

Service `0x27` uses paired **access levels**: an **odd** level requests a seed,
the next **even** level sends the key.

1. **Request seed** — tester sends `0x27 <level>` (odd, e.g. `0x11`). The ECU
   replies `0x67 <level> <seed…>` with a random seed (commonly 2–6 bytes; some
   ECUs send more).
2. **Compute key** — the tester transforms the seed into a key using the ECU's
   algorithm. The algorithm is specific to the ECU family/supplier and is the
   secret part. Examples of *forms* it can take (OpenTune ships no OEM
   algorithm for any of them): a fixed XOR/add/rotate scheme, a lookup against a
   secret constant, a small bytecode program shipped in the car's ODX (VAG
   "SA2"), or an AES/HMAC-based challenge. For the SA2 form, `opentune-sa2`
   runs the real SA2 instruction set over a **caller-supplied** script — the
   per-ECU bytecode itself is never bundled (see
   [VAG SA2 scripts](#vag-sa2-scripts)).
3. **Send key** — tester sends `0x27 <level+1> <key…>` (even). The ECU replies:
   - `0x67 <level+1>` — **unlocked**; protected services are now available.
   - `0x7F 0x27 0x35` — **invalidKey** (`Error::SecurityDenied`).
   - `0x7F 0x27 0x36` — **exceededNumberOfAttempts** (locked out).
   - `0x7F 0x27 0x37` — **requiredTimeDelayNotExpired** (cooldown after bad
     tries; wait before retrying).

A seed of **all zeros** conventionally means the ECU is **already unlocked** —
no key needs to be sent. Different levels gate different operations (e.g. one
level for extended-session reads, a higher one for programming); a programming
flash typically needs the programming-session level (VAG UDS: `0x11`).

The matching service clients already implement the wire format:

- `core::protocol::uds::UdsClient::request_seed(level)` /
  `send_key(level, key)` (`send_key` maps NRC `0x35` → `Error::SecurityDenied`).
- `core::protocol::kwp2000::Kwp2000Client::request_seed(level)` /
  `send_key(level, key)`.

`request_seed` returns **only the seed bytes** (the echoed level is stripped).

---

## The `SeedKey` trait

`crates/opentune-core/src/protocol/seedkey.rs`:

```rust
pub trait SeedKey {
    /// Identifier matching an ECU definition's `seedkey` field.
    fn id(&self) -> &str;
    /// Compute the key for a seed at the given access level.
    fn compute(&self, seed: &[u8], level: u8) -> Result<Vec<u8>>;
}
```

- `id()` matches the `seedkey` field of an `EcuDef`
  (`crates/opentune-core/src/ecudef.rs`), so the app can select the right handler
  for a known ECU.
- `compute(seed, level)` is the user's algorithm. It receives the raw seed and
  the request level (so one implementation can serve several levels) and returns
  the key bytes. The trait carries **no** data and OpenTune provides **no**
  OEM implementation of it. The one generic implementation, `opentune-sa2`'s
  `Sa2SeedKey` (id `"sa2"`), is an interpreter: it computes nothing until the
  caller supplies the SA2 script for their ECU.

Because it is a trait object (`Box<dyn SeedKey>`), the algorithm can live in a
closed plugin, be loaded from the user's own file, or be compiled into a private
build — none of which is part of this open-source repo.

---

## How a driver obtains and uses a `SeedKey`

Each interface driver (`opentune-driver-openport2`, `-socketcan`, `-elm327`)
now carries an **optional** handler and runs the handshake itself, before the
download, through the existing service client.

### Installing the handler

```rust
let mut driver = OpenPort2Driver::new();
driver.set_seed_key(Box::new(my_ecu_seedkey)); // user-supplied; see scope note
```

`set_seed_key(Box<dyn SeedKey>)` exists on all three drivers (and, for API
parity, on the non-Linux SocketCAN stub, where it is accepted and ignored).

### Telling the driver security is required

The driver reads these keys from `ConnectConfig.options`
(`crates/opentune-core/src/types.rs`):

| option          | effect                                                              |
|-----------------|--------------------------------------------------------------------|
| `seedkey`       | expected algorithm id (matches `EcuDef.seedkey`); **its presence marks security as required** |
| `security`      | `required` / `true` / `1` / `yes` also marks security as required   |
| `seedkey_level` | request-seed access level (odd), decimal or `0x`-hex; default `0x11` UDS, `0x01` KWP |

So an ECU definition that sets `seedkey` already signals that a handler must be
present.

In the desktop app, handlers come from a plugin build, or from the user's own
SA2 script (see [VAG SA2 scripts](#vag-sa2-scripts)): its
`DesktopConfig::seedkeys` resolvers are asked by id for service scripts that
name a `seedkey`, for the Utilities calculator and, through
`adapter::apply_seed_key`, for the plugin's own commands. See
[`PLUGINS.md`](PLUGINS.md#seedkey-algorithms).

### The handshake, inside `write_memory` (and the read path)

Each driver has a private `security_access()` step called from `write_memory`
(and from `read_memory`, since a locked ECU also gates the full read). It:

1. Decides whether security is **required** and which **level** to use (from the
   config keys above).
2. Runs, over the existing `UdsClient` / `Kwp2000Client`:
   `request_seed(level)` → (skip if the seed is all-zero / already unlocked) →
   `seedkey.compute(seed, level)` → `send_key(level + 1, key)`.
3. Returns a clear error when **security is required but no handler is set**:

   > `configuration error: ECU requires security access but no seed/key handler
   > is set (call set_seed_key with an algorithm for this ECU)`

Behaviour matrix:

| handler set? | `security_required`? | result                                             |
|--------------|----------------------|----------------------------------------------------|
| yes          | (either)             | handshake runs (no-op if seed is all-zero)         |
| no           | yes                  | `Error::Config` before any write — fail fast       |
| no           | no                   | no-op — open / already-unlocked ECU keeps working  |

This keeps the existing read/write paths intact: an unprotected or
pre-unlocked ECU with no handler configured behaves exactly as before.

---

## VAG SA2 scripts

VW-Group control units answer programming security access (UDS `0x27`,
level `0x11`; KWP on older units) with a seed whose key is computed by a small
bytecode program — the **SA2 script** — shipped inside the module's own flash
container. `opentune-sa2` is a virtual machine for that bytecode; OpenTune
ships no script.

### Where the script is

- **ODX / ODX-F** (bare, in a ZIP, or inside an FRF): the `SECURITY` block
  (`SESSION/SECURITYS/SECURITY`) whose `SECURITY-METHOD` is `SA2`. The bytes
  are read from that block's byte field — `FW-SIGNATURE` first, then
  `FW-CHECKSUM` / `VALIDITY-FOR` — and every candidate is listed, so the one
  that decodes as an SA2 program can be picked with `--index`. The element
  names are the ODX-F schema's; that the bytecode sits in `FW-SIGNATURE` is
  inferred from public descriptions and **not yet confirmed on a real file**.
- **SGO**: the SA2 field the header points to (index at header offset
  `0x29`: a 32-bit length, then the bytes).

`opentune container unpack --in <file>` prints `SA2 script: #i …` for each one
found (`Unpacked::sa2_scripts` in the library).

### The instruction set

One 32-bit **register** (loaded with the 4-byte seed, big-endian; it is the key
when the script ends) and one **carry** flag (clear at start).

| byte | mnemonic | operand | effect |
|---|---|---|---|
| `81` | `rsl` | — | rotate left 1; carry := old bit 31 |
| `82` | `rsr` | — | rotate right 1; carry := old bit 0 |
| `93` | `add` | 4 bytes BE | add mod 2³²; carry := overflow |
| `84` | `sub` | 4 bytes BE | subtract mod 2³²; carry := borrow |
| `87` | `eor` | 4 bytes BE | exclusive or; carry unchanged |
| `68` | `loop` | count | run the following bytes up to the matching `next` *count* times (loops nest) |
| `49` | `next` | — | end of the innermost loop |
| `4A` | `bcc` | skip | if carry is clear, skip *skip* bytes past this instruction |
| `6B` | `bra` | skip | skip *skip* bytes past this instruction |
| `4C` | `finish` | — | end of script |

Branches only go forward; a branch to or past the end ends the script.
OpenTune stops at `finish` and refuses `loop 0` (public implementations
disagree on it); a step budget and a loop-depth limit stop runaway scripts
with a typed error. Sources: `bri3d/sa2_seed_key` (MIT) and its ports — see
[PRIOR-ART.md](PRIOR-ART.md). The VM is checked against that reference
implementation on synthetic programs ([tools/sa2](../tools/sa2/README.md)).

### Using it

```sh
# Compute a key from your own container's script and the ECU's seed:
opentune seedkey sa2 --container my-update.frf --frf-key frf.key --seed 1A2B3C4D
# ...or from a script you have as hex / a file (raw, hex text or assembler):
opentune seedkey sa2 --script "68 05 81 … 4C" --seed 1A2B3C4D
opentune seedkey sa2 --script-file my.sa2 --disasm          # listing + lints
opentune seedkey sa2 --script-file my.sa2 --seed 1A2B3C4D --trace
```

For the commands that unlock a live ECU (`flash --uds-module`, `service`,
`read`, …), `--seedkey sa2:<hex>` or `--seedkey sa2:@<file>` builds the `sa2`
handler from your script. In Rust: `Sa2SeedKey::new(script)` /
`Sa2SeedKey::from_hex(..)`, or `opentune_sa2::execute(seed, script)`.

In the desktop app, the **SA2 script** panel does the same offline: paste the
script (hex or assembler), load a file, or open your own container (with your
FRF key file for an `.frf`) and pick one of its candidates; it shows the
disassembly with lints and computes the key for a seed, with an optional
bounded step trace. In alpha / bench builds it can also **arm** the script as
seed/key id `sa2` for the rest of the run (Bench tier until vehicle-tested):
a service script naming `"seedkey": "sa2"` then unlocks with it. The desktop
also resolves an inline `sa2:<hex>` id and `sa2:@<file>` (raw bytes, hex or
assembler; files over 1 MiB or scripts over 4096 bytes are refused). Plugin resolvers
are asked first, so a plugin that provides `sa2` itself keeps precedence.

---

## Your own definitions file (UnlockECU format)

If you keep seed/key definitions in the UnlockECU JSON format, OpenTune can
load **your** file and route each key to **your** provider program (or to the
SA2 interpreter for `VolkswagenSA2` entries): `--seedkey unlockecu:<EcuName>`
with `--seedkey-defs <FILE> --seedkey-provider <builtin:sa2|/abs/path>`. It
is off by default, refuses immobiliser / anti-theft entries, and ships no
algorithm or data. See [SEEDKEY-LOADER.md](SEEDKEY-LOADER.md).

---

## Example (demo) implementation

A **demonstration** `SeedKey` lives in each driver's `#[cfg(test)]`
`seedkey_hook_tests` module — a trivial reversible transform (XOR `0x5A` then
`+1` per byte):

```rust
struct DemoXorAddKey;
impl SeedKey for DemoXorAddKey {
    fn id(&self) -> &str { "demo-xor-add" }
    fn compute(&self, seed: &[u8], _level: u8) -> Result<Vec<u8>> {
        Ok(seed.iter().map(|b| (b ^ 0x5A).wrapping_add(1)).collect())
    }
}
```

The tests drive it through a scripted transport to prove the driver sends
`0x27 <level>`, computes the key, and replies `0x27 <level+1> <key>` — i.e. the
hook is wired correctly. **This is not a real OEM algorithm and will not unlock
any ECU.** It exists only so the hook is exercised in CI.

---

## Legal and scope

- OpenTune ships **no** OEM seed/key algorithms, tables, bytecode, or keys, and
  will not accept contributions that add them.
- OpenTune's scope is **calibration tuning**. The seed/key hook exists to unlock
  the diagnostic services needed to read a backup and write a calibration — not
  to defeat emissions controls or immobilisers. See
  [FLASHING-NOTES.md](FLASHING-NOTES.md) for the matching scope statement.
- Users are responsible for supplying a `SeedKey` only for ECUs they own and are
  permitted to modify, and for complying with local law on vehicle
  modification.
