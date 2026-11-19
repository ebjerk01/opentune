# Seed/Key Definitions Loader (UnlockECU format)

OpenTune can read a seed/key **definitions file** in the format of
[jglim/UnlockECU](https://github.com/jglim/UnlockECU) (MIT) and use it through
the normal [seed/key hook](SEEDKEY.md) (`Interface::set_seed_key`,
`--seedkey`, the desktop's `SeedKeys`). It is an **interop loader**, nothing
more.

> **OpenTune ships no algorithms and no data.** The crate
> `opentune-seedkey-loader` contains no seed/key algorithm ("provider"), no
> definition, no table and no constant, and none will be accepted. Nothing
> from UnlockECU's providers or its `db.json` was read, ported or bundled; only
> the definition *format* described in its public README is used
> ([PRIOR-ART.md](PRIOR-ART.md)). The file and the algorithms are always the
> **user's own**. Evidence level: `synthetic-tested` (synthetic definitions,
> a synthetic provider program and the UDS simulator); not used on a vehicle.

---

## Off by default

Nothing loads unless you name **both** a definitions file **and** a provider.
There is no search path, no default file name and no auto-discovery: a file
lying next to the binary is ignored.

| front end | how to switch it on |
|---|---|
| CLI | `--seedkey-defs <FILE> --seedkey-provider <SPEC>` (global flags, any position; `--seedkey-provider` repeatable). Either flag alone is an error. |
| CLI embedder | `CliConfig::seedkey_loader = Some(LoaderConfig::new(file, providers))` (the flags take precedence). |
| Desktop embedder | `DesktopConfig::seedkey_loader = Some(LoaderConfig { .. })`. **Bench tier**: honoured only on a channel with `debug_enabled()` (alpha / bench builds); on stable it is ignored with a startup message. |

A provider `SPEC` is `builtin:sa2` or the **absolute path** of your provider
program (see below). At most one program may be configured.

Once loaded, the seed/key id **`unlockecu:<EcuName or alias>`** resolves
(after any plugin resolver and the `sa2` ids). When the driver asks for a key,
the definition whose `AccessLevel` equals the requested level is used; its
`SeedLength` must equal the seed's length and the provider's key must have
`KeyLength` bytes, or the unlock fails with an error (no key is sent).

```sh
# Check your file first: what loads, what is refused, what is rejected.
opentune seedkey defs my-defs.json

# Program a module, the key coming from your own provider program:
opentune flash --driver socketcan --port can0 --addr 0x... --data new.bin \
    --seedkey unlockecu:MY-ECU \
    --seedkey-defs /home/me/my-defs.json --seedkey-provider /opt/my-provider/provider
```

On load the CLI prints one summary line to stderr (counts and providers),
then one line per refused or rejected entry.

---

## File format

The file is UTF-8 JSON (a BOM is accepted), at most 8 MiB: a top-level
**array** of definitions, or an object with **exactly one** array-valued member
holding them. At most 50 000 definitions. Each definition:

| field | type | rule |
|---|---|---|
| `EcuName` | string | required, non-empty, ≤ 128 chars, no control characters |
| `Aliases` | array of strings | optional; ≤ 64 non-empty names, same rules as `EcuName` |
| `AccessLevel` | integer | required, `0x01..=0x7E` — the request-seed (odd) SecurityAccess sub-function it serves |
| `SeedLength` | integer | required, 1..=64 bytes |
| `KeyLength` | integer | required, 1..=64 bytes |
| `Provider` | string | required, ≤ 128 chars of `A-Z a-z 0-9 _ - .` — the algorithm name your provider implements |
| `Origin` | string | optional free text (≤ 128 chars) |
| `Parameters` | array | optional; ≤ 64 entries of `{ "Key", "Value", "DataType" }` with unique `Key`s |

`DataType` (case-sensitive) is one of:

| DataType | `Value` |
|---|---|
| `ByteArray` | hex text (whitespace ignored, even digit count), ≤ 4096 bytes |
| `Int32` | a JSON number, or decimal / `0x`-hex text, within `-2^31 ..= 2^32-1` |
| `String` | text, ≤ 4096 chars |

Any other `DataType` rejects the entry. Unknown extra fields are ignored.

A file that is not JSON, has no definition list, or exceeds a file-level
limit fails as a whole. A single entry that breaks a rule is **rejected**
(reported with its index and reason) and the rest still load; a later entry
that repeats an `EcuName`/alias at the same `AccessLevel` is rejected as a
duplicate. Rejected entries never run. Messages never echo a parameter value.

---

## Providers

### `builtin:sa2`

The one built-in provider is OpenTune's generic SA2 interpreter
(`opentune-sa2`, see [SEEDKEY.md](SEEDKEY.md#vag-sa2-scripts)). With
`builtin:sa2` configured, a definition whose `Provider` is `VolkswagenSA2`
(the name UnlockECU uses for this VM) runs **its own** `InstructionTape`
parameter (`ByteArray`, 1..=4096 bytes) on the VM. It needs `SeedLength` 4 and
`KeyLength` 4. No tape ships with OpenTune. Definitions naming any other
provider go to your provider program, or fail with "provider '…' is not
available" when none is configured.

### Your provider program (runtime plugin boundary)

Every other algorithm lives in a program **you** install, outside OpenTune.
OpenTune runs it once per key:

- directly, by the absolute path you gave — **no shell, no `PATH` lookup, no
  arguments**; it inherits OpenTune's environment;
- writes **one JSON request** plus a newline to its stdin, then closes stdin;
- reads its stdout: at most **64 KiB**, which must be **one JSON object**;
- drains and discards stderr (bounded) — it is never shown;
- gives it **5 s** (`LoaderConfig::timeout`, 1 ms..=60 s) to exit, then kills it.

Request (protocol `opentune-seedkey-provider/1`):

```json
{
  "protocol": "opentune-seedkey-provider/1",
  "provider": "MyAlgo",
  "ecu": "MY-ECU",
  "access_level": 17,
  "seed": "11223344",
  "seed_length": 4,
  "key_length": 4,
  "parameters": [
    { "key": "Mask",   "type": "ByteArray", "value": "0102" },
    { "key": "Rounds", "type": "Int32",     "value": 3 },
    { "key": "Note",   "type": "String",    "value": "text" }
  ]
}
```

`seed` and `ByteArray` values are upper-case hex; `access_level` is the
request-seed level the driver is unlocking; `provider`, `ecu` and the
parameters come from the matched definition.

Response, with exit status 0:

```json
{ "key": "A1B2C3D4" }
```

or, to refuse (any exit status):

```json
{ "error": "MyAlgo does not support level 0x11" }
```

The `error` text (first 200 characters, control characters blanked) is passed
on. A non-zero exit, a timeout, output over the limit, output that is not
exactly one of these objects, or a key whose length is not `KeyLength` is an
error, and no key is sent to the ECU. The program's stdout is never echoed in
an error.

A provider program is ordinary software you write or obtain yourself; any
language works. A skeleton:

```python
#!/usr/bin/env python3
import json, sys
req = json.load(sys.stdin)
if req.get("protocol") != "opentune-seedkey-provider/1":
    print(json.dumps({"error": "unsupported protocol"})); sys.exit(1)
seed = bytes.fromhex(req["seed"])
key = my_algorithm(req["provider"], req["access_level"], seed, req["parameters"])  # yours
print(json.dumps({"key": key.hex().upper()}))
```

(For a script, the path you give OpenTune is the script itself, made
executable with a `#!` line; OpenTune does not start an interpreter for you.)

### Why a program and not a compiled plugin

OpenTune's existing compiled hook (`CliConfig::seedkeys` /
`DesktopConfig::seedkeys`, a `fn(&str) -> Option<Box<dyn SeedKey>>`) needs
the algorithm linked into a custom build; it stays available for embedders.
A separate program needs no rebuild, no `unsafe` dynamic loading and no Rust
ABI, keeps the algorithm in its own process with its own licence, and lets
the loader bound what it can do (time, output size, no shell).

---

## Refuse-list (immobiliser / anti-theft)

OpenTune's scope excludes immobiliser removal or defeat
([CONTRIBUTING.md, "Scope boundaries"](../CONTRIBUTING.md#scope-boundaries)).
The loader therefore **refuses** — even in a file you supply — every
definition whose `EcuName`, any of its `Aliases`, its `Origin` or its
`Provider` matches the list below. Refused entries are reported (index, ECU
name, field, matched term, and the scope note) and never run; an
`unlockecu:` id whose name matches is refused the same way.

Matching is case-insensitive and word-ish: each field is split into
alphanumeric runs, each run into tokens at camel-case and letter/digit
boundaries (`EZS167` → `EZS` `167`, `KeylessGo` → `Keyless` `Go`). Every
token, every pair of adjacent tokens joined, and the whole run are checked:

| rule | terms |
|---|---|
| equals | `ezs`, `esl`, `elv`, `wfs`, `immo`, `kessy` |
| starts with | `immobili` (immobiliser / immobilizer), `keyless`, `entriegel` (entriegeln, Entriegelung), `unlock`, `wegfahrsperre`, `kessy` |
| contains | `theft` (anti-theft, antitheft) |

The list errs on the side of refusing. Note that `unlock` matches `UnlockECU`
too: an entry whose `Origin` cites that name is refused — edit your own
file's `Origin` if that is the only match. The exact list lives in
`crates/opentune-seedkey-loader/src/scope.rs` and is unit-tested.

---

## Secrets in logs

Keys and parameter values are never printed or logged by default: `Debug`
output of definitions and parameters shows `<redacted, N bytes>`, the load
report and `seedkey defs` list parameter keys and types only, and errors name
fields and lengths, never values or provider output.

## Legal and scope

As for the seed/key hook in general ([SEEDKEY.md](SEEDKEY.md#legal-and-scope)):
supply definitions and algorithms only for ECUs you own and may modify; the
loader exists to unlock the diagnostic services a calibration backup and
write need, not to defeat immobilisers or emissions controls.
