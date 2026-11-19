# Third-party attribution — opentune-container

This crate (GPL-3.0-or-later, as part of OpenTune) contains algorithm ports from
the following open-source projects. Their licenses (MIT / BSD-2-Clause) permit
inclusion in a GPL work; attribution is preserved here and in the per-module
source headers.

## bri3d/VW_Flash — BSD-2-Clause
https://github.com/bri3d/VW_Flash

Ported: the FRF rolling-XOR stream cipher (`decrypt_data`), the Simos "LZSS10"
decompressor (`decompress_raw_lzss10`), the legacy-Simos compression
(`legacysimos.decompress`), the Simos counter-XOR, the DSG progressive
substitution cipher, and the ODX extraction logic (`extractodx.py`).

> Copyright (c) VW_Flash contributors. Redistribution and use in source and
> binary forms, with or without modification, are permitted provided that the
> BSD-2-Clause conditions are met. See the upstream LICENSE.

## lsdlsd88/TCU-FRF-SGO-UNPACKER — MIT
https://github.com/lsdlsd88/TCU-FRF-SGO-UNPACKER

Ported: the SGO v2 ("SGML Object File") container structure and parser
(`sgo_unpack.py`), the format-1 "progressive" substitution cipher
(`decrypt_table`) and the early "feedback" cipher (`sgo_early.decrypt_feedback`).

## dspl1236/vag-tcu-tools — MIT
https://github.com/dspl1236/vag-tcu-tools

Ported: the DQ-family LZSS decompressor (`core/lzss.py`), the ZF-8HP "LZZ"
2047-byte-window codec (`core/lzz.py`), the repeating-XOR and AES-CBC block
crypto (`core/crypto_xor.py`, `core/crypto_aes.py`), and the ODX-F external-`.bin`
parsing (`core/odx.py`).

## UCL NRV2B — GPL-2.0-or-later (algorithm only)
https://www.oberhumer.com/opensource/ucl/ — Markus F.X.J. Oberhumer

`src/nrv2b.rs` is an independent implementation of the NRV2B (8-bit)
decompressor from the public algorithm description; no UCL source is copied.
GPL-2.0-or-later is compatible with this crate's GPL-3.0-or-later.

## afflux/uclcli — GPL-3.0-or-later (test vector only)
https://github.com/afflux/uclcli — Copyright (C) 2020-2021 BMW Group

Its public doctest vector (`92 a5 aa a1 00 00 00 00 00 04 80 ff` → 1024 × `0xA5`)
is reproduced as a unit test of `nrv2b`; it also documents that BMW SWE segments
use `ucl_nrv2b_decompress_safe_8`.

## BMW SWE descriptor layout — facts from public sources
See `docs/BMW-CONTAINERS.md` and `docs/PRIOR-ART.md`. No code from any of those
sources is included; no BMW file was used to build or test this crate.

## NO BUNDLED KEYS

None of the recovered key material from these projects is included here. The FRF
key (`frf.key`), the DSG substitution table (`mqb_dsg_key.bin`), the ZF-8HP
XOR key string, the Simos/DQ AES keys/IVs, and the SGO format-1 family cipher
tables are **all caller-supplied** at the API boundary. This crate ships no
keys, for legal and security reasons.
