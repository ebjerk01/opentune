# SA2 script

VW-Group control units unlock programming (SecurityAccess, UDS `27 11`) with
a seed whose key is computed by a small bytecode program, the **SA2 script**.
Each module's script travels inside its own flash container. This panel runs
that script on your computer: it shows what the script does and computes the
key for a seed.

OpenTune ships **no** SA2 script. You bring your own: from your own flash
container, or typed in from your own notes.

## What you need

One of:

- the script as hex (`87 5A 5A 5A 5A 4C`) or SA2 assembler, one instruction
  per line (`eor 0x5A5A5A5A`, `finish`);
- a script file (raw bytes, hex text or assembler);
- your own flash container (`.frf`, `.zip`, `.odx`, `.sgo`). An encrypted
  `.frf` also needs your FRF key file.

No adapter and no car: everything except **Use for live sessions** is offline.

## Step by step

1. Paste the script and press **Disassemble**, or use **Load script file…**.
   For a container, choose the FRF key file first if it is an `.frf`, then
   **Open container…**.
2. A container can hold several candidates (the ODX `SECURITY` block's fields,
   or the SGO header's SA2 field). The table shows each one's ODX field,
   session and whether it decodes as a script. The first one that decodes is
   picked for you; press **Use** to pick another.
3. Read the **Disassembly**: offset, bytes, instruction (loop bodies are
   indented), branch targets with labels, and any **lints**.
4. Under **Seed → key**, enter the 4-byte seed and press **Compute key**. Tick
   **step trace** to see every executed instruction with the register and
   carry after it (only the first *rows* steps are shown).

## Reading the results

- **Lints** flag scripts that do not look like the usual ones: no `finish` at
  the end, a `loop 0`, a branch into the middle of an instruction, a `next`
  without a `loop`. A lint does not stop the script from running.
- **disassembly stopped** means a byte is not an SA2 opcode. In a container,
  that usually means the field is not the script: pick another candidate.
- The **key** is the register after the script, sent big-endian. For
  programming access (level `11`) it goes out as `27 12 <key>`.

## Use for live sessions (Bench)

In alpha and bench builds, **Arm as sa2** makes this script the seed/key
handler with the id `sa2` until the app closes. A service script whose JSON
says `"seedkey": "sa2"` then answers SecurityAccess with it (see
**Service fn**), and so does a plugin command that asks for `sa2`. **Disarm**
removes it. Arming refuses a script that does not decode or fails on a test
seed, and needs the confirmation box. If a plugin in the build also provides
`sa2`, the plugin's handler is used.

This path is **not vehicle-tested** yet, so it stays in the Bench tier.

## Common errors

- **choose your FRF key file** — the file is not a plain ZIP, ODX or SGO, so
  it is read as an encrypted FRF, and that needs your key file.
- **No SA2 script found** — the container has no ODX `SECURITY` block with
  method `SA2` and no SGO SA2 field.
- **the seed must be exactly 4 bytes** — SA2 seeds are 4 bytes: the ECU's
  reply is `67 11` followed by them.
- **step budget** — the script ran too long (a runaway loop). Real scripts
  finish in a few hundred steps.

## Safety

- The offline tools only read your files.
- The key is only as right as the script. Before arming it, compute the key
  for a seed whose key you already know.
- A wrong key counts against the module's attempt limit; after a few, the
  module locks SecurityAccess for a while.
