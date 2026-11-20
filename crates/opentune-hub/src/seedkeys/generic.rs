//! Generic, fully-transparent seed→key transforms for the Utilities seed/key
//! calculator. The public edition ships NO OEM / vehicle-specific seed/key
//! routines (those are not open), so this is a calculator the user drives:
//! they pick a transform and supply its parameters. It never pretends to
//! compute a real ECU's key from a bundled secret. An id that is not one of
//! these transforms is the front end's to resolve (a plugin id through
//! [`SeedKeys`](super::SeedKeys)), or an honest error.

use serde_json::{json, Value};

/// The note a front end attaches to a key computed by [`transform`].
pub const NOTE: &str = "Generic transform — NOT an OEM algorithm. The public edition bundles no vehicle-specific seed/key routines.";

/// The generic transforms, as the calculator lists them: `id`, `name`, the
/// `params` it takes (`mask`, `add`) and a one-line `desc`.
pub fn catalog() -> Value {
    json!([
        { "id": "identity", "name": "Identity (key = seed)", "params": [],
          "desc": "Echoes the seed. Some access levels accept the seed unchanged." },
        { "id": "xor-mask", "name": "XOR mask", "params": ["mask"],
          "desc": "key[i] = seed[i] XOR mask[i] (mask repeats if shorter)." },
        { "id": "add-const", "name": "Add constant (per byte)", "params": ["add"],
          "desc": "key[i] = seed[i] + k (mod 256)." },
        { "id": "xor-add", "name": "XOR then add (demo shape)", "params": ["mask", "add"],
          "desc": "key[i] = (seed[i] XOR m) + k (mod 256), m = first mask byte." },
        { "id": "byteswap", "name": "Reverse byte order", "params": [],
          "desc": "key = seed reversed." }
    ])
}

/// Apply the generic transform `algo` to `seed`, with the parsed `mask` bytes
/// (empty when not given) and `add` constant (0 when not given). `None` when
/// `algo` is not a generic transform id; `Some(Err(_))` when the transform is
/// missing a parameter it needs.
pub fn transform(algo: &str, seed: &[u8], mask: &[u8], add: u8) -> Option<Result<Vec<u8>, String>> {
    Some(Ok(match algo {
        "identity" => seed.to_vec(),
        "byteswap" => seed.iter().rev().copied().collect(),
        "add-const" => seed.iter().map(|&b| b.wrapping_add(add)).collect(),
        "xor-mask" => {
            if mask.is_empty() {
                return Some(Err("XOR mask needs a mask value".into()));
            }
            seed.iter()
                .enumerate()
                .map(|(i, &b)| b ^ mask[i % mask.len()])
                .collect()
        }
        "xor-add" => {
            let Some(&m) = mask.first() else {
                return Some(Err("XOR-add needs a mask byte".into()));
            };
            seed.iter().map(|&b| (b ^ m).wrapping_add(add)).collect()
        }
        _ => return None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(algo: &str, seed: &[u8], mask: &[u8], add: u8) -> Vec<u8> {
        transform(algo, seed, mask, add).unwrap().unwrap()
    }

    #[test]
    fn transforms() {
        assert_eq!(key("identity", &[1, 2], &[], 0), [1, 2]);
        assert_eq!(key("byteswap", &[1, 2], &[], 0), [2, 1]);
        // xor-mask: the mask repeats.
        assert_eq!(key("xor-mask", &[0xF0, 0xF0], &[0xFF], 0), [0x0F, 0x0F]);
        // add-const wraps mod 256.
        assert_eq!(key("add-const", &[0xFE], &[], 3), [0x01]);
        // xor-add: (0x0A ^ 0x0F) + 1 = 0x06, first mask byte only.
        assert_eq!(key("xor-add", &[0x0A], &[0x0F, 0xAA], 1), [0x06]);
    }

    #[test]
    fn missing_params_and_unknown_ids() {
        assert!(transform("xor-mask", &[0], &[], 0).unwrap().is_err());
        assert!(transform("xor-add", &[0], &[], 0).unwrap().is_err());
        assert!(transform("", &[0], &[], 0).is_none());
        assert!(transform("vag-edc17", &[0], &[], 0).is_none());
    }

    #[test]
    fn catalog_lists_every_transform() {
        let ids: Vec<_> = catalog()
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            ids,
            ["identity", "xor-mask", "add-const", "xor-add", "byteswap"]
        );
        for id in &ids {
            assert!(transform(id, &[1], &[1], 1).unwrap().is_ok(), "{id}");
        }
    }
}
