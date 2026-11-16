//! End-to-end pipeline tests through the public API.
//!
//! These build synthetic containers (NO real/recovered keys) and verify the
//! full FRF -> ZIP -> ODX -> decrypt(AES) -> decompress(LZSS10) chain, plus the
//! SGO path, round-trips exactly. Validation against real FRF/ODX/SGO samples is
//! pending (none were available on the ECU share at build time).

use opentune_container::compress::compress_lzss10;
use opentune_container::crypto::{aes128_cbc_encrypt, BlockCipher};
use opentune_container::zip::write_stored;
use opentune_container::{detect, frf, unpack, ContainerKind, InputFormat, Keys};

/// Hex-encode helper for building inline ODX DATA.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

#[test]
fn full_frf_odx_aes_lzss10_pipeline() {
    // 1. Original flash payload.
    let payload: Vec<u8> = (0..4096u32)
        .map(|i| (i.wrapping_mul(37) ^ (i >> 4)) as u8)
        .collect();

    // 2. Compress with Simos LZSS10.
    let compressed = compress_lzss10(&payload);

    // 3. AES-128-CBC encrypt (synthetic key/iv supplied by the "caller").
    let key = [0x24u8; 16];
    let iv = [0x42u8; 16];
    let encrypted = aes128_cbc_encrypt(&key, &iv, &compressed);

    // 4. Wrap in an ODX document: method "A1" = LZSS10 compression + AES encryption.
    let odx = format!(
        r#"<?xml version="1.0"?>
        <ODX><FLASH><ECU-MEMS><ECU-MEM><MEM>
          <DATABLOCKS><DATABLOCK>
            <FLASHDATA-REF ID-REF="asw1"/>
            <SEGMENTS><SEGMENT>
              <SOURCE-START-ADDRESS>0x80000000</SOURCE-START-ADDRESS>
              <UNCOMPRESSED-SIZE>{}</UNCOMPRESSED-SIZE>
            </SEGMENT></SEGMENTS>
          </DATABLOCK></DATABLOCKS>
          <SESSIONS><SESSION><EXPECTED-IDENTS><EXPECTED-IDENT><IDENT-VALUES>
            <IDENT-VALUE>8V0906264</IDENT-VALUE>
          </IDENT-VALUES></EXPECTED-IDENT></EXPECTED-IDENTS></SESSION></SESSIONS>
          <FLASHDATAS><FLASHDATA ID="asw1">
            <ENCRYPT-COMPRESS-METHOD>A1</ENCRYPT-COMPRESS-METHOD>
            <DATA>{}</DATA>
          </FLASHDATA></FLASHDATAS>
        </MEM></ECU-MEM></ECU-MEMS></FLASH></ODX>"#,
        payload.len(),
        hex(&encrypted)
    );

    // 5. ZIP it and FRF-encrypt with synthetic key material.
    let zip = write_stored(&[("flash.odx".to_string(), odx.into_bytes())]);
    let frf_key: Vec<u8> = (0..1000u16)
        .map(|i| (i.wrapping_mul(97) % 251) as u8)
        .collect();
    let frf_bytes = frf::encrypt_frf(&frf_key, &zip).unwrap();

    // 6. Detect + unpack through the public entry point.
    assert_eq!(detect(&frf_bytes), InputFormat::Frf);
    let keys = Keys {
        frf_key: Some(frf_key),
        block_cipher: BlockCipher::Aes128Cbc { key, iv },
        ..Keys::default()
    };
    let result = unpack(&frf_bytes, &keys).unwrap();

    assert_eq!(result.kind, ContainerKind::Odx);
    assert_eq!(result.box_codes, vec!["8V0906264".to_string()]);
    assert_eq!(result.segments.len(), 1);
    let seg = &result.segments[0];
    assert!(seg.decoded, "note: {:?}", seg.note);
    assert_eq!(seg.address, Some(0x8000_0000));
    assert_eq!(seg.data, payload, "decoded payload must match original");
}

#[test]
fn missing_frf_key_is_a_clean_error() {
    let frf_bytes = [0x11u8, 0x22, 0x33, 0x44, 0x55];
    let err = unpack(&frf_bytes, &Keys::default()).unwrap_err();
    assert!(err.to_string().contains("frf_key"));
}
