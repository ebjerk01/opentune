# Coding

Read a module's configuration (coding), see it decoded into named parameters,
preview a change, and write it back with a backup and read-back verify.

## What you need

- **Adapter: CAN**, connected and detected.
- A **coding definition** JSON for the module. It names the configuration blocks and the bit-fields inside them. It may include the module's CAN `req_id` / `resp_id`.
- The **target module address**: from the definition, or typed as **tx / rx** in the sidebar. Reads fall back to the engine ECU (7E0 / 7E8) and say so. **Writes are refused unless the target is known.**
- **Ignition on**, engine off.
- The dongle's **HS/MS-CAN switch** on the module's bus. Engine modules are on HS; body and comfort modules are often on MS.
- A battery charger or stable supply for writes.

## Step by step

1. **Pick coding def…** and choose the definition.
2. Press **Read coding**. The table lists each parameter: kind, current value, decoded label, and the allowed options.
3. Click a row to put its name in the edit box. Type the new value (decimal, or hex such as `0x02`).
4. Press **Preview**. Nothing is written. The report shows the parameter change and the exact bytes that would change.
5. If the preview is what you want, press **Write**.
6. Read the outcome. Press **Read coding** again to confirm.

## Reading the results

- **planned (dry run)** — Preview result. Nothing was sent except reads.
- **no change** — the new value equals the current one.
- **written & verified** — written and read back correctly.
- **written (verification skipped)** — written, but not read back.
- **VERIFY FAILED** / **WRITE ERROR** — see Safety below.
- **module 7E0/7E8 — assumed engine ECU** — no target was set, so a read went to the engine. Set the target if you meant a different module.
- **requires SecurityAccess (0x27)** — the module needs an unlock before it accepts a write. OpenTune does not perform that unlock, so such a module will usually refuse the write.

## Common errors

- **Target unknown** on Write — set tx/rx in the sidebar or add `req_id` / `resp_id` to the definition.
- **Negative response** — the module refused. Common reasons: security access needed, wrong session, or conditions not correct (for example engine running).
- **Value rejected** — outside the parameter's bit width or not one of its options.

## Safety

- **A wrong coding can disable functions or immobilise a module.** Always Preview first.
- Before writing, OpenTune reads a fresh backup of the coding blocks and shows it under **Backup before write**. Copy those bytes somewhere before you continue. After a failed write or verify the backup opens automatically: these are the bytes to restore.
- Write only with a healthy battery or a charger connected. Do not switch the ignition off during a write.
- Write is not behind a confirmation prompt. Check the module, bus and value before you press it.
