//! Import a **CCF / CCC parameter descriptor** (an XML table of car-configuration
//! parameters) that the *user* supplies, into a [`CodingDef`] the coding engine
//! can decode and edit.
//!
//! Ford, Volvo and JLR modules of the EUCD/CCF era keep the vehicle's
//! configuration as one byte string (the *Car Configuration File*, also called
//! the CCC / car configuration constants). Its parameter table is published
//! by no open source; OpenTune ships **no** such table. This importer reads a
//! table the user already has, in the element structure that the GPL-3.0
//! [ccf_parser](https://github.com/5555l/ccf_parser) (`modules/ccfdataload.py`
//! and `ccfparser.py` @ 479ac82) reads:
//!
//! ```xml
//! <anything>
//!   <block name="CCF">
//!     <group start="5" stop="5">
//!       <title><tm>Group title</tm></title>
//!       <parameter id="005_005" name="P_NAME" mask="0x30" type="ENUM">
//!         <parameter_title><tm>Readable title</tm></parameter_title>
//!         <select>
//!           <option value="0x00" name="OPT_A"><tm>Option A</tm></option>
//!           <option value="0x10" name="OPT_B"><tm>Option B</tm></option>
//!         </select>
//!       </parameter>
//!     </group>
//!   </block>
//! </anything>
//! ```
//!
//! * `id` = start byte and end byte (three decimal digits each, `_`
//!   separators ignored) of the parameter's bytes in the configuration string;
//! * `mask` applies to those bytes read as one **big-endian** number, and an
//!   option's `value` is the *masked, unshifted* value;
//! * `type` is `ENUM`, `BOOL`, `BIN` (a number) or `ASCII` (text).
//!
//! The bit-field engine addresses fields LSB-first inside one block, so the
//! importer converts each mask to a `(byte, bit, len)` field and **skips, with
//! a reason**, what it cannot express: ASCII text, non-contiguous masks, fields
//! that cross a byte boundary of the big-endian number, and option values
//! outside their mask. Nothing is guessed.
//!
//! Where the configuration string lives on the bus differs by platform and is
//! not in any open source, so the caller names the DID that holds it
//! ([`CcfOptions::did`]); byte offsets are taken relative to that DID's data.

use std::collections::HashSet;

use crate::{
    mask_field, Addressing, CodingDef, CodingError, CodingOption, CodingParam, ParamKind, Result,
};

/// How to build the definition from a descriptor.
#[derive(Debug, Clone)]
pub struct CcfOptions {
    /// The `<block name=…>` to import (ccf_parser uses `CCF` and `EUCD`).
    pub block: String,
    /// The UDS DID whose data is the configuration string (byte 0 = CCF byte 0).
    pub did: u16,
    /// The new definition's `id`.
    pub id: String,
    /// The new definition's display name.
    pub name: String,
    /// Whether writing needs SecurityAccess first (advisory, see
    /// [`CodingDef::requires_security`]).
    pub requires_security: bool,
}

/// A descriptor parameter the importer did not turn into a [`CodingParam`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// The parameter's `name` (or `id`).
    pub parameter: String,
    /// Why it was skipped.
    pub reason: String,
}

/// The result of [`import_descriptor`].
#[derive(Debug, Clone)]
pub struct CcfImport {
    /// The imported definition (validated).
    pub def: CodingDef,
    /// Parameters that could not be expressed, with the reason.
    pub skipped: Vec<Skipped>,
}

fn fmt_err(msg: impl Into<String>) -> CodingError {
    CodingError::Format(msg.into())
}

fn parse_hex(s: &str) -> Option<u64> {
    let t = s.trim();
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    if t.is_empty() {
        return None;
    }
    u64::from_str_radix(t, 16).ok()
}

/// The non-empty text of the first `<tm>` under the first `child` element.
fn tm_text(node: roxmltree::Node, child: &str) -> Option<String> {
    let c = node
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == child)?;
    tm_of(c)
}

fn tm_of(node: roxmltree::Node) -> Option<String> {
    node.children()
        .find(|n| n.is_element() && n.tag_name().name() == "tm")
        .and_then(|tm| tm.text())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// Parse `SSS_EEE[_…]` into (start byte, end byte).
fn parse_offsets(id: &str) -> Option<(usize, usize)> {
    let digits: String = id.chars().filter(|c| *c != '_').collect();
    let start = digits.get(0..3)?.parse().ok()?;
    let end = digits.get(3..6)?.parse().ok()?;
    Some((start, end))
}

/// Import the parameters of block `opts.block` from a descriptor XML.
pub fn import_descriptor(xml: &str, opts: &CcfOptions) -> Result<CcfImport> {
    let doc =
        roxmltree::Document::parse(xml).map_err(|e| fmt_err(format!("descriptor XML: {e}")))?;
    let blocks: Vec<roxmltree::Node> = doc
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "block")
        .collect();
    let block = blocks
        .iter()
        .find(|b| b.attribute("name") == Some(opts.block.as_str()))
        .ok_or_else(|| {
            let names: Vec<&str> = blocks.iter().filter_map(|b| b.attribute("name")).collect();
            fmt_err(format!(
                "no <block name=\"{}\"> in the descriptor (blocks found: {})",
                opts.block,
                if names.is_empty() {
                    "none".to_string()
                } else {
                    names.join(", ")
                }
            ))
        })?;

    let mut params = Vec::new();
    let mut skipped = Vec::new();
    let mut names = HashSet::new();
    for p in block
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "parameter")
    {
        let id = p.attribute("id").unwrap_or("");
        let pname = p.attribute("name").unwrap_or(id).to_string();
        let mut skip = |reason: String| {
            skipped.push(Skipped {
                parameter: pname.clone(),
                reason,
            })
        };
        let Some((start, end)) = parse_offsets(id) else {
            skip(format!("id `{id}` is not SSS_EEE byte offsets"));
            continue;
        };
        if end < start || end - start >= 8 {
            skip(format!("bytes {start}-{end}: need 1-8 bytes"));
            continue;
        }
        let ty = p
            .attribute("type")
            .unwrap_or("")
            .trim()
            .to_ascii_uppercase();
        if ty == "ASCII" {
            skip("ASCII text parameters are not bit fields".into());
            continue;
        }
        if !matches!(ty.as_str(), "ENUM" | "BOOL" | "BIN") {
            skip(format!("unknown type `{ty}`"));
            continue;
        }
        let Some(mask) = p.attribute("mask").and_then(parse_hex) else {
            skip("missing or non-hex mask".into());
            continue;
        };
        let width_bits = (end - start + 1) * 8;
        if width_bits < 64 && mask >> width_bits != 0 {
            skip(format!("mask 0x{mask:X} is wider than bytes {start}-{end}"));
            continue;
        }
        let Some((low, len)) = mask_field(mask) else {
            skip(format!("mask 0x{mask:X} is zero or not contiguous"));
            continue;
        };
        let bit = low % 8;
        if usize::from(bit) + usize::from(len) > 8 {
            skip(format!(
                "mask 0x{mask:X} crosses a byte of the big-endian value (bytes {start}-{end})"
            ));
            continue;
        }
        let byte = end - usize::from(low / 8);

        // Options: masked, unshifted values -> field values.
        let mut options = Vec::new();
        let mut seen = HashSet::new();
        let mut bad_option = None;
        for o in p
            .descendants()
            .filter(|n| n.is_element() && n.tag_name().name() == "option")
        {
            let Some(v) = o.attribute("value").and_then(parse_hex) else {
                bad_option = Some(format!(
                    "option `{}` has no hex value",
                    o.attribute("name").unwrap_or("?")
                ));
                break;
            };
            if v & !mask != 0 {
                bad_option = Some(format!("option value 0x{v:X} is outside mask 0x{mask:X}"));
                break;
            }
            let value = v >> low;
            if !seen.insert(value) {
                continue; // a repeated value keeps its first label
            }
            let label = tm_of(o)
                .or_else(|| o.attribute("name").map(str::to_string))
                .unwrap_or_else(|| format!("0x{v:X}"));
            options.push(CodingOption { value, label });
        }
        if let Some(reason) = bad_option {
            skip(reason);
            continue;
        }

        let kind = match ty.as_str() {
            "BIN" => ParamKind::Number,
            "BOOL" if options.is_empty() && len == 1 => ParamKind::Bool,
            "BOOL" if options.is_empty() => ParamKind::Number,
            _ => ParamKind::Enum,
        };
        if kind == ParamKind::Number {
            options.clear();
        }

        // Display name: the readable title, else the descriptor name; unique.
        let title = tm_text(p, "parameter_title")
            .or_else(|| p.parent_element().and_then(|g| tm_text(g, "title")));
        let mut display = title.clone().unwrap_or_else(|| pname.clone());
        if !names.insert(display.clone()) {
            display = format!("{display} ({pname})");
            let mut n = 2;
            while !names.insert(display.clone()) {
                display = format!("{} ({pname}) #{n}", title.as_deref().unwrap_or(&pname));
                n += 1;
            }
        }
        params.push(CodingParam {
            name: display,
            block: 0,
            byte,
            bit,
            len,
            kind,
            options,
            note: format!(
                "{} bytes {start}-{end}, mask 0x{mask:X}, type {ty}, descriptor name `{pname}`",
                opts.block
            ),
        });
    }

    let def = CodingDef {
        id: opts.id.clone(),
        name: opts.name.clone(),
        module: format!(
            "Imported from a user-supplied CCF/CCC descriptor (block `{}`); labels are the user's, not shipped by OpenTune",
            opts.block
        ),
        requires_security: opts.requires_security,
        addressing: Addressing::Did {
            dids: vec![opts.did],
        },
        params,
    };
    def.validate()?;
    Ok(CcfImport { def, skipped })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, set, Coding, CodingBlock};

    /// A synthetic descriptor in the documented structure (invented names).
    const XML: &str = r#"<?xml version="1.0"?>
<descriptor>
  <block name="OTHER"><group start="0" stop="0">
    <parameter id="000_000" name="IGNORED" mask="0x01" type="BOOL"/>
  </group></block>
  <block name="CCF">
    <group start="2" stop="2">
      <title><tm>Lighting</tm></title>
      <parameter id="002_002" name="P_LAMP" mask="0x30" type="ENUM">
        <parameter_title><tm>Lamp mode</tm></parameter_title>
        <select>
          <option value="0x00" name="OFF"><tm>Off</tm></option>
          <option value="0x10" name="ON"><tm>On</tm></option>
          <option value="0x20" name="AUTO"><tm></tm></option>
        </select>
      </parameter>
      <parameter id="002_002" name="P_FLAG" mask="01" type="BOOL"/>
    </group>
    <group start="3" stop="4">
      <parameter id="003_004" name="P_LEVEL" mask="0x0F00" type="BIN"/>
      <parameter id="003_004" name="P_LOWBYTE" mask="0x00FF" type="BIN"/>
      <parameter id="003_004" name="P_WIDE" mask="0x0FF0" type="BIN"/>
      <parameter id="003_004" name="P_HOLE" mask="0x0005" type="BIN"/>
    </group>
    <group start="5" stop="9">
      <parameter id="005_009" name="P_TEXT" mask="0xFF" type="ASCII"/>
      <parameter id="005_005" name="P_BADOPT" mask="0x03" type="ENUM">
        <select><option value="0x04" name="X"><tm>X</tm></option></select>
      </parameter>
    </group>
  </block>
</descriptor>"#;

    fn opts() -> CcfOptions {
        CcfOptions {
            block: "CCF".into(),
            did: 0x1234,
            id: "user-ccf".into(),
            name: "User CCF".into(),
            requires_security: true,
        }
    }

    #[test]
    fn imports_expressible_parameters_and_reports_the_rest() {
        let imp = import_descriptor(XML, &opts()).unwrap();
        let def = &imp.def;
        assert_eq!(def.addressing.dids(), &[0x1234]);
        assert!(def.requires_security);
        let names: Vec<&str> = def.params.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["Lamp mode", "Lighting", "P_LEVEL", "P_LOWBYTE"]);
        let lamp = def.param("Lamp mode").unwrap();
        assert_eq!((lamp.byte, lamp.bit, lamp.len), (2, 4, 2));
        assert_eq!(lamp.kind, ParamKind::Enum);
        assert_eq!(
            lamp.options
                .iter()
                .map(|o| (o.value, o.label.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "Off"), (1, "On"), (2, "AUTO")] // empty <tm> -> name
        );
        // BOOL without options, 1 bit; title falls back to the group title.
        let flag = def.param("Lighting").unwrap();
        assert_eq!(
            (flag.byte, flag.bit, flag.len, flag.kind),
            (2, 0, 1, ParamKind::Bool)
        );
        // Big-endian 2-byte value: mask 0x0F00 is the low nibble of byte 3,
        // mask 0x00FF is all of byte 4.
        let level = def.param("P_LEVEL").unwrap();
        assert_eq!((level.byte, level.bit, level.len), (3, 0, 4));
        let low = def.param("P_LOWBYTE").unwrap();
        assert_eq!((low.byte, low.bit, low.len), (4, 0, 8));
        let skipped: Vec<&str> = imp.skipped.iter().map(|s| s.parameter.as_str()).collect();
        assert_eq!(skipped, vec!["P_WIDE", "P_HOLE", "P_TEXT", "P_BADOPT"]);
    }

    #[test]
    fn imported_def_decodes_and_edits_synthetic_bytes() {
        let def = import_descriptor(XML, &opts()).unwrap().def;
        let coding = Coding {
            blocks: vec![CodingBlock {
                did: 0x1234,
                bytes: vec![0, 0, 0b0010_0001, 0x07, 0x5A, 0],
            }],
        };
        let d = decode(&def, &coding).unwrap();
        let by = |n: &str| d.iter().find(|p| p.name == n).unwrap().clone();
        assert_eq!(by("Lamp mode").label, "AUTO");
        assert_eq!(by("Lighting").label, "on");
        assert_eq!(by("P_LEVEL").value, 7);
        assert_eq!(by("P_LOWBYTE").value, 0x5A);
        let edited = set(&def, &coding, "Lamp mode", 1).unwrap();
        assert_eq!(edited.blocks[0].bytes[2], 0b0001_0001);
    }

    #[test]
    fn missing_block_and_bad_xml_are_errors() {
        let mut o = opts();
        o.block = "EUCD".into();
        let e = import_descriptor(XML, &o).unwrap_err().to_string();
        assert!(e.contains("OTHER, CCF"), "{e}");
        assert!(import_descriptor("<x", &opts()).is_err());
    }
}
