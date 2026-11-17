//! Utility panels: VIN / part-number decoder, DTC lookup and the seed/key
//! calculator (generic transforms, or a plugin-supplied algorithm, labelled).

use crate::{
    common::{hexs, parse_hex_bytes, parse_u32},
    image::part_breakdown,
    seedkeys::{self, SeedKeys},
};
use serde_json::{json, Value};

#[tauri::command]
pub fn vin_decode(vin: String) -> Result<Value, String> {
    let v = opentune_vin::decode_vin(&vin).map_err(|e| format!("{e:?}"))?;
    Ok(json!({
        "vin": v.vin,
        "wmi": v.wmi,
        "vds": v.vds,
        "vis": v.vis,
        "manufacturer": v.wmi_info.manufacturer,
        "country": v.wmi_info.country,
        "check_digit": v.check_digit.to_string(),
        "check_digit_valid": v.check_digit_valid,
        "model_year": {
            "code": v.model_year.code.to_string(),
            "candidates": v.model_year.candidates,
            "resolved": v.model_year.resolved,
            "ambiguous": v.model_year.resolved.is_none(),
        },
        "plant_code": v.plant_code.to_string(),
        "serial": v.serial,
        "vds_descriptor": v.vds_descriptor,
        "flags": v.flags,
    }))
}

#[tauri::command]
pub fn part_decode(part: String) -> Result<Value, String> {
    if part.trim().is_empty() {
        return Err("enter a part number".into());
    }
    Ok(part_breakdown(&opentune_vin::parse_part_number(&part)))
}

#[tauri::command]
pub fn dtc_lookup(code: String) -> Result<Value, String> {
    let d = opentune_dtc::lookup(&code)
        .ok_or("not a recognisable DTC — it must start with P, C, B or U (e.g. P0101)")?;
    let system = match d.system {
        opentune_dtc::System::Powertrain => "Powertrain (engine / transmission / emissions)",
        opentune_dtc::System::Chassis => "Chassis (ABS / steering / suspension)",
        opentune_dtc::System::Body => "Body (airbags / lighting / HVAC / comfort)",
        opentune_dtc::System::Network => "Network / communication (CAN bus / modules)",
    };
    Ok(json!({
        "code": d.code,
        "description": d.description,
        "system": system,
        "system_letter": d.system.letter().to_string(),
    }))
}

/// Generic, fully-transparent seed→key transforms. The public edition ships NO
/// OEM / vehicle-specific seed/key routines (those are not open), so this is a
/// calculator the user drives: they pick a transform and supply its parameters.
/// It never pretends to compute a real ECU's key from a bundled secret. (A
/// plugin build may add options for its own [`SeedKeys`] ids from its UI
/// overlay; [`seedkey_compute`] resolves those through the plugin.)
#[tauri::command]
pub fn seedkey_algos() -> Value {
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

/// Compute a key from a seed with a user-selected generic transform, or with a
/// plugin-supplied algorithm ([`SeedKeys`]) when `algo` is not one of the
/// generic ids. Honest by construction: no OEM algorithm is bundled, so an id
/// no plugin knows is a hard error rather than a fabricated key, and a plugin
/// result is labelled as such (`source: "plugin"`). `level` is the
/// request-seed access level passed to a plugin algorithm (default `0x01`).
#[tauri::command]
pub fn seedkey_compute(
    seed: String,
    algo: String,
    mask: Option<String>,
    add: Option<String>,
    level: Option<u8>,
    seedkeys: tauri::State<'_, SeedKeys>,
) -> Result<Value, String> {
    compute_seedkey(&seed, &algo, mask, add, level, &seedkeys)
}

/// [`seedkey_compute`] without the Tauri state wrapper.
fn compute_seedkey(
    seed: &str,
    algo: &str,
    mask: Option<String>,
    add: Option<String>,
    level: Option<u8>,
    seedkeys: &SeedKeys,
) -> Result<Value, String> {
    let s = parse_hex_bytes(seed)?;
    let add_val: u8 = match add.as_deref().map(str::trim).filter(|x| !x.is_empty()) {
        Some(x) => parse_u32(x)? as u8,
        None => 0,
    };
    let mask_bytes = match mask.as_deref().map(str::trim).filter(|x| !x.is_empty()) {
        Some(x) => parse_hex_bytes(x)?,
        None => Vec::new(),
    };

    let key: Vec<u8> = match algo {
        "identity" => s.clone(),
        "byteswap" => s.iter().rev().copied().collect(),
        "add-const" => s.iter().map(|&b| b.wrapping_add(add_val)).collect(),
        "xor-mask" => {
            if mask_bytes.is_empty() {
                return Err("XOR mask needs a mask value".into());
            }
            s.iter()
                .enumerate()
                .map(|(i, &b)| b ^ mask_bytes[i % mask_bytes.len()])
                .collect()
        }
        "xor-add" => {
            let m = *mask_bytes.first().ok_or("XOR-add needs a mask byte")?;
            s.iter().map(|&b| (b ^ m).wrapping_add(add_val)).collect()
        }
        "" => return Err("pick a transform first".into()),
        other => {
            let Some(handler) = seedkeys.get(other) else {
                return Err(format!(
                    "no seed/key algorithm '{other}' is available — the public edition ships no OEM routines; pick a generic transform or supply one"
                ));
            };
            let level = level.unwrap_or(0x01);
            let key = seedkeys::compute(handler.as_ref(), &s, level)?;
            return Ok(json!({
                "key": hexs(&key),
                "algo": other,
                "level": level,
                "source": "plugin",
                "note": format!(
                    "Computed by a plugin-supplied algorithm ('{}'), not by OpenTune. The plugin owns this algorithm and its correctness; OpenTune bundles no vehicle-specific seed/key routines.",
                    handler.id()
                ),
            }));
        }
    };

    Ok(json!({
        "key": hexs(&key),
        "algo": algo,
        "source": "generic",
        "note": "Generic transform — NOT an OEM algorithm. The public edition bundles no vehicle-specific seed/key routines.",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The calculator as the public app runs it (no plugin resolvers).
    fn sk(
        seed: String,
        algo: String,
        mask: Option<String>,
        add: Option<String>,
    ) -> Result<Value, String> {
        compute_seedkey(&seed, &algo, mask, add, None, &SeedKeys::default())
    }

    #[test]
    fn seedkey_transforms() {
        // identity
        let r = sk("0102".into(), "identity".into(), None, None).unwrap();
        assert_eq!(r["key"], "01 02");
        // byteswap
        let r = sk("0102".into(), "byteswap".into(), None, None).unwrap();
        assert_eq!(r["key"], "02 01");
        // xor-mask (mask repeats)
        let r = sk("F0F0".into(), "xor-mask".into(), Some("FF".into()), None).unwrap();
        assert_eq!(r["key"], "0F 0F");
        // add-const mod 256 wraps
        let r = sk("FE".into(), "add-const".into(), None, Some("3".into())).unwrap();
        assert_eq!(r["key"], "01");
        // xor-add
        let r = sk(
            "0A".into(),
            "xor-add".into(),
            Some("0F".into()),
            Some("0x01".into()),
        )
        .unwrap();
        assert_eq!(r["key"], "06"); // (0x0A ^ 0x0F) + 1 = 0x05 + 1 = 0x06
    }

    #[test]
    fn seedkey_calculator_uses_a_plugin_resolver_and_labels_it() {
        use crate::seedkeys::testkeys::{first, second, vcar};
        let keys = SeedKeys::new(vec![first, second, vcar]);
        // A plugin id resolves (first resolver wins) and is labelled as such.
        let r = compute_seedkey("0001", "shared", None, None, Some(0x03), &keys).unwrap();
        assert_eq!(r["key"], "13 12"); // seed ^ 0x10 ^ level 0x03
        assert_eq!(r["source"], "plugin");
        assert_eq!(r["level"], 3);
        let note = r["note"].as_str().unwrap();
        assert!(
            note.contains("plugin-supplied") && note.contains("'first'"),
            "{note}"
        );
        // The virtual car's test key, default level 0x01.
        let r = compute_seedkey("1234", "vcar-test", None, None, None, &keys).unwrap();
        assert_eq!(r["key"], "48 6E"); // XOR 0x5A
                                       // Generic ids are never routed to a plugin, and are labelled generic.
        let r = compute_seedkey("0102", "identity", None, None, None, &keys).unwrap();
        assert_eq!(
            (r["key"].as_str(), r["source"].as_str()),
            (Some("01 02"), Some("generic"))
        );
        // An id no plugin knows stays an honest error.
        let e = compute_seedkey("00", "oem-x", None, None, None, &keys).unwrap_err();
        assert!(e.contains("'oem-x'"), "{e}");
    }

    #[test]
    fn seedkey_unknown_algo_is_honest_error() {
        assert!(sk("00".into(), "vag-edc17".into(), None, None).is_err());
        assert!(sk("00".into(), "".into(), None, None).is_err());
        // xor-mask without a mask must error, not fabricate.
        assert!(sk("00".into(), "xor-mask".into(), None, None).is_err());
    }
}
