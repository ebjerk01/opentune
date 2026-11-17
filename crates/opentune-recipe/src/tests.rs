use super::*;

const N: usize = 8192;
const ALGO: &str = "crc32-tail";

fn noise(len: usize, seed: u32) -> Vec<u8> {
    let mut s = seed;
    (0..len)
        .map(|_| {
            s = s.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (s >> 16) as u8
        })
        .collect()
}

fn fix(img: &mut [u8]) {
    let algo = opentune_checksum::get(ALGO).unwrap();
    algo.correct(img).unwrap();
}

/// A synthetic "stock" image with (fake) identity strings and a valid CRC tail.
fn stock() -> Vec<u8> {
    let mut img = noise(N, 42);
    img[0x40..0x4C].copy_from_slice(b" 0281099999 ");
    img[0x60..0x6C].copy_from_slice(b" 1037999999 ");
    fix(&mut img);
    img
}

/// Two edits (a 6-byte "map" change and a 2-byte change), CRC re-fixed.
fn tuned(stock: &[u8]) -> Vec<u8> {
    let mut t = stock.to_vec();
    for b in &mut t[0x1000..0x1006] {
        *b = b.wrapping_add(7);
    }
    t[0x1800] ^= 0x55;
    t[0x1801] ^= 0xAA;
    fix(&mut t);
    t
}

fn opts() -> CreateOptions {
    CreateOptions {
        checksum: Some(ALGO.into()),
        ..CreateOptions::default()
    }
}

fn relocate() -> ApplyOptions {
    ApplyOptions {
        mode: ApplyMode::Relocate,
        checksum: None,
    }
}

#[test]
fn create_then_apply_to_stock_reproduces_tuned_exactly() {
    let s = stock();
    let t = tuned(&s);
    let (r, warnings) = create(&s, &t, &opts()).unwrap();
    assert!(warnings.is_empty());
    assert_eq!(r.patches.len(), 2);
    assert_eq!(r.patches[0].offset, 0x1000);
    assert_eq!(r.patches[0].len(), 6);
    assert_eq!(r.patches[0].context_before.len(), 16);
    // The CRC tail is recognised as a checksum field, not a patch.
    assert_eq!(
        r.checksum_fields,
        vec![Range {
            start: (N - 4) as u64,
            end: N as u64
        }]
    );
    assert!(r.source.part_numbers.contains(&"0281099999".to_string()));
    assert!(r.source.sw_numbers.contains(&"1037999999".to_string()));

    // Round-trip through the JSON format.
    let r = Recipe::from_json(&r.to_json()).unwrap();
    let rep = apply(&r, &s, &ApplyOptions::default());
    assert_eq!(rep.refusal, None, "{rep:?}");
    assert!(rep
        .patches
        .iter()
        .all(|p| matches!(p.status, PatchStatus::Anchored { .. })));
    assert_eq!(rep.output.as_deref(), Some(&t[..]));
    assert!(rep.identical_to_recipe_result);
    let att = rep.attestation.unwrap();
    assert!(att.satisfied);
    assert!(att.verify_for(&t).is_ok());
}

#[test]
fn strict_refuses_a_different_image() {
    let s = stock();
    let (r, _) = create(&s, &tuned(&s), &opts()).unwrap();
    let mut other = s.clone();
    other[0x1C00] ^= 1; // an unpatched byte differs: different calibration
    fix(&mut other);
    let rep = apply(&r, &other, &ApplyOptions::default());
    assert!(rep.output.is_none());
    assert!(rep.refusal.unwrap().contains("unchanged-region hash"));
    assert!(rep.patches.is_empty(), "identity refuses before any patch");
}

#[test]
fn strict_refuses_when_original_bytes_differ_at_an_anchor() {
    let s = stock();
    let (r, _) = create(&s, &tuned(&s), &opts()).unwrap();
    let mut other = s.clone();
    other[0x1002] ^= 0xFF; // inside patch 0: excluded from the hash
    fix(&mut other);
    let rep = apply(&r, &other, &ApplyOptions::default());
    assert!(rep.output.is_none());
    assert!(matches!(
        rep.patches[0].status,
        PatchStatus::Mismatch { .. }
    ));
    assert!(matches!(
        rep.patches[1].status,
        PatchStatus::Anchored { .. }
    ));
    assert!(rep.refusal.unwrap().contains("nothing was applied"));
}

/// The stock image with 32 bytes inserted at 0x1400 (same total size).
fn shifted(s: &[u8]) -> Vec<u8> {
    let mut t = s[..0x1400].to_vec();
    t.extend_from_slice(&[0xA5; 32]);
    t.extend_from_slice(&s[0x1400..N - 32]);
    fix(&mut t);
    t
}

#[test]
fn relocation_moves_patches_only_in_explicit_mode() {
    let s = stock();
    let t = tuned(&s);
    let (r, _) = create(&s, &t, &opts()).unwrap();
    let target = shifted(&s);
    // Strict mode: different software layout -> identity refuses.
    assert!(apply(&r, &target, &ApplyOptions::default())
        .refusal
        .is_some());
    let rep = apply(&r, &target, &relocate());
    assert_eq!(rep.refusal, None, "{rep:?}");
    assert_eq!(rep.patches[0].status, PatchStatus::Anchored { at: 0x1000 });
    assert_eq!(
        rep.patches[1].status,
        PatchStatus::Relocated {
            from: 0x1800,
            to: 0x1820
        }
    );
    let out = rep.output.unwrap();
    assert_eq!(out[0x1820..0x1822], t[0x1800..0x1802]);
    assert_eq!(out[0x1000..0x1006], t[0x1000..0x1006]);
    assert!(rep.attestation.unwrap().verify_for(&out).is_ok());
}

#[test]
fn ambiguous_relocation_is_refused() {
    let s = stock();
    let (r, _) = create(&s, &tuned(&s), &opts()).unwrap();
    let mut target = shifted(&s);
    // Duplicate patch 1's context+original elsewhere: two equally good homes.
    let window = s[0x1800 - 8..0x1802 + 8].to_vec();
    target[0x0800..0x0800 + window.len()].copy_from_slice(&window);
    fix(&mut target);
    let rep = apply(&r, &target, &relocate());
    assert!(rep.output.is_none());
    assert_eq!(rep.patches[1].status, PatchStatus::Ambiguous { matches: 2 });
}

#[test]
fn relocate_mode_still_requires_the_hardware_identity() {
    let s = stock();
    let (r, _) = create(&s, &tuned(&s), &opts()).unwrap();
    let mut target = shifted(&s);
    target[0x40..0x4C].copy_from_slice(b" 0281011111 ");
    fix(&mut target);
    let rep = apply(&r, &target, &relocate());
    assert!(rep.refusal.unwrap().contains("part numbers"));
}

#[test]
fn missing_checksum_algorithm_refuses_unless_given() {
    let s = stock();
    let t = tuned(&s);
    // Family is Unknown and no algorithm named: recipe records none.
    let (r, warnings) = create(&s, &t, &CreateOptions::default()).unwrap();
    assert_eq!(r.checksum, None);
    assert!(!warnings.is_empty());
    // Without a checksum the CRC tail is just another patch.
    assert_eq!(r.patches.len(), 3);
    let rep = apply(&r, &s, &ApplyOptions::default());
    assert!(rep.output.is_none());
    assert!(rep.refusal.unwrap().contains("no checksum algorithm"));
    let rep = apply(
        &r,
        &s,
        &ApplyOptions {
            mode: ApplyMode::Strict,
            checksum: Some(ALGO.into()),
        },
    );
    assert_eq!(rep.output.as_deref(), Some(&t[..]));
}

#[test]
fn already_applied_and_partial_targets() {
    let s = stock();
    let t = tuned(&s);
    let (r, _) = create(&s, &t, &opts()).unwrap();
    // Applying to the tuned file is a verified no-op.
    let rep = apply(&r, &t, &ApplyOptions::default());
    assert_eq!(rep.refusal, None);
    assert_eq!(rep.output.as_deref(), Some(&t[..]));
    // Half-applied: refuse rather than mix two tunes.
    let mut half = s.clone();
    half[0x1000..0x1006].copy_from_slice(&t[0x1000..0x1006]);
    fix(&mut half);
    let rep = apply(&r, &half, &ApplyOptions::default());
    assert!(rep.refusal.unwrap().contains("refusing to mix"));
}

#[test]
fn patches_are_named_from_definition_maps() {
    let s = stock();
    let t = tuned(&s);
    let map = MapDef {
        name: "test map".into(),
        address: 0x0FFC,
        rows: 1,
        cols: 8,
        ..MapDef::default()
    };
    let o = CreateOptions {
        maps: vec![map],
        ..opts()
    };
    let (r, _) = create(&s, &t, &o).unwrap();
    assert_eq!(r.patches[0].maps, vec!["test map".to_string()]);
    assert!(r.patches[1].maps.is_empty());
}

#[test]
fn rejects_bad_files_and_inputs() {
    let s = stock();
    assert_eq!(create(&s, &s, &opts()).unwrap_err(), RecipeError::NoChanges);
    assert!(matches!(
        create(&s, &s[..100], &opts()).unwrap_err(),
        RecipeError::SizeMismatch(..)
    ));
    let (r, _) = create(&s, &tuned(&s), &opts()).unwrap();
    let json = r.to_json().replace("\"version\": 1", "\"version\": 99");
    assert!(Recipe::from_json(&json).is_err());
    let json = r.to_json().replace(FORMAT, "something-else");
    assert!(Recipe::from_json(&json).is_err());
    // Size mismatch at apply time is an identity refusal.
    let rep = apply(&r, &s[..N / 2], &ApplyOptions::default());
    assert!(rep.refusal.unwrap().contains("size"));
}

#[test]
fn an_embedder_lookup_resolves_ids_the_public_set_lacks() {
    // "host-crc" is not a public id: an embedding app maps it to its own
    // implementation (here the public CRC tail under another name).
    let lookup = |id: &str| (id == "host-crc").then(|| opentune_checksum::get(ALGO).unwrap());
    let s = stock();
    let t = tuned(&s);
    let o = CreateOptions {
        checksum: Some("host-crc".into()),
        ..CreateOptions::default()
    };
    assert!(matches!(
        create(&s, &t, &o),
        Err(RecipeError::UnknownChecksum(_))
    ));
    let (r, _) = create_with(&s, &t, &o, &lookup).unwrap();
    assert_eq!(r.checksum.as_deref(), Some("host-crc"));

    let rep = apply(&r, &s, &ApplyOptions::default());
    assert!(
        rep.refusal.is_some(),
        "public lookup must not know host-crc"
    );
    let rep = apply_with(&r, &s, &ApplyOptions::default(), &lookup);
    assert_eq!(rep.refusal, None, "{rep:?}");
    assert_eq!(rep.output.as_deref(), Some(&t[..]));
    assert_eq!(rep.attestation.unwrap().algorithm, "host-crc");
}
