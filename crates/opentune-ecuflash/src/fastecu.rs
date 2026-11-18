//! # FastECU definitions
//!
//! [FastECU](https://github.com/miikasyvanen/FastECU) (GPL-3.0, Subaru
//! ECU/TCU flashing and tuning) has **no definition format of its own**: it
//! reads the community EcuFlash and RomRaider XML definitions directly, plus
//! a converter that turns a WinOLS *MapPack* CSV export into a RomRaider
//! definition. This module imports those inputs **the way FastECU interprets
//! them** — the format study is credited in `docs/PRIOR-ART.md`; the code is a
//! fresh implementation, no FastECU source is copied.
//!
//! What FastECU adds on top of plain EcuFlash/RomRaider (and how each lands):
//!
//! | FastECU behaviour | Import |
//! |---|---|
//! | `startpos` / `interval` table attributes (hex): cell *i* lives at `address + (i·interval + startpos−1)·size` | `startpos` > 1 with `interval` 1 is a plain address shift → applied, clean. `interval` > 1 is interleaved storage → **refused** (`stride-noncontiguous`). |
//! | `Switch` / `Selectable` / `MultiSelectable` tables, `bloblist` storage | enumerated byte patterns, not numeric maps → **refused** (out of scope, `ObjectKind::Switch`). |
//! | `uint24` / `int24` storage | no exact OpenTune type → **refused**. |
//! | floats are always read big-endian, whatever `endian` says | the def's `endian="little"` on a float is overridden to big-endian → **warned** (`float-endian`). |
//! | `wrx02` flash method: an address past the (160 KiB) ROM is folded down by `0x8000` | applied with a **warning** (`address-folded`); refused if still outside the ROM. |
//! | inheritance: EcuFlash `<include>` by xmlid, RomRaider `<rom base=>` | resolved (same document first, then the caller's resolver); child attributes override the base per table and per axis. |
//! | MapPack CSV (`Name;…;DataOrg;Columns;Rows;Fieldvalues.*;AxisX.*;AxisY.*`) | imported directly. Unlike FastECU's converter, `Fieldvalues.Offset` is honoured and an unknown `DataOrg` is **refused** instead of silently becoming `uint8`. |
//!
//! Everything else follows the common discipline of the importers: each table
//! ends in a clean / warned / refused [`ObjectReport`]; non-linear equations
//! are imported pre-scaled with a warning; nothing is guessed silently.

use crate::{storage_to_type, Error, Result, RomId};
use opentune_core::ecudef::{AxisDef, DataType, Endian, MapDef, Scaling};
use opentune_xdf::math::{linear_from_equation, LinearFit};
use opentune_xdf::report::{self, kinds, ObjectKind, ObjectReport, Summary, Tracker};
use roxmltree::{Document, Node, NodeId};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

/// FastECU's ROM size for the `wrx02` (Denso HC16, 160 KiB) flash method.
const WRX02_ROM_SIZE: u64 = 160 * 1024;

/// Import options.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// `<rom>` to import, by `xmlid`. Default: the only rom, or the first rom
    /// that inherits from a base (a calibration, not a base definition).
    pub rom: Option<String>,
}

/// Which input FastECU-style input was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// EcuFlash XML.
    EcuFlash,
    /// RomRaider XML.
    RomRaider,
    /// WinOLS MapPack CSV (FastECU's converter input).
    MapPackCsv,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Source::EcuFlash => "ecuflash-xml",
            Source::RomRaider => "romraider-xml",
            Source::MapPackCsv => "mappack-csv",
        })
    }
}

/// Result of a FastECU import.
#[derive(Debug, Clone)]
pub struct FastecuImport {
    /// What was read.
    pub source: Source,
    /// Identity of the imported rom (empty for CSV).
    pub rom_id: RomId,
    /// The inheritance chain, most specific first (`xmlid`s).
    pub chain: Vec<String>,
    /// The imported maps.
    pub maps: Vec<MapDef>,
    /// Per-table outcome (clean / warned / refused + reasons).
    pub reports: Vec<ObjectReport>,
    /// File-level notes: rom selection, unresolved bases, assumed ROM size.
    pub warnings: Vec<String>,
}

impl FastecuImport {
    /// Counts and issue-kind histogram.
    pub fn summary(&self) -> Summary {
        report::summarize(&self.reports)
    }

    /// Why a strict import would fail (empty = clean).
    pub fn strict_failures(&self) -> Vec<String> {
        report::strict_failures(&self.reports)
    }

    /// Strict mode: `Err(Error::Strict)` unless every table imported cleanly.
    /// Switch/selection tables are refused-out-of-scope and do not count.
    pub fn into_strict(self) -> Result<Self> {
        let f = self.strict_failures();
        if f.is_empty() {
            Ok(self)
        } else {
            Err(Error::Strict(f))
        }
    }
}

/// Index a directory of definitions (recursively, `*.xml`) by rom `xmlid`, for
/// resolving `<include>` / `base=` like FastECU's definition directory.
pub fn index_base_dir(dir: &Path) -> std::io::Result<HashMap<String, PathBuf>> {
    let mut out = HashMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let p = e?.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("xml")) {
                let raw = std::fs::read(&p)?;
                let src = String::from_utf8_lossy(&raw);
                if let Ok(doc) = Document::parse(&src) {
                    for r in doc
                        .descendants()
                        .filter(|n| n.is_element() && n.tag_name().name() == "rom")
                    {
                        let id = xmlid(r);
                        if !id.is_empty() {
                            out.entry(id).or_insert_with(|| p.clone());
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Load a base definition's source from an [`index_base_dir`] index.
pub fn load_base(index: &HashMap<String, PathBuf>, id: &str) -> Option<String> {
    let p = index.get(id)?;
    std::fs::read(p)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Import a FastECU definition: EcuFlash / RomRaider XML (dialect
/// auto-detected) or a WinOLS MapPack CSV. `resolve(xmlid)` supplies the
/// source of a base definition not present in `src` (return `None` when
/// unavailable; the missing base is reported).
pub fn parse_fastecu<F>(src: &str, opts: &Options, resolve: F) -> Result<FastecuImport>
where
    F: Fn(&str) -> Option<String>,
{
    let head = src.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']);
    if !head.starts_with('<') {
        return parse_mappack_csv(src);
    }
    parse_xml(src, opts, resolve)
}

// ---------------------------------------------------------------------------
// XML (EcuFlash / RomRaider)
// ---------------------------------------------------------------------------

fn xmlid(rom: Node) -> String {
    rom.children()
        .find(|c| c.is_element() && c.tag_name().name() == "romid")
        .map(|r| crate::child_text(r, "xmlid"))
        .unwrap_or_default()
}

/// Base ids of a rom: RomRaider `base=` + EcuFlash `<include>`s, in order.
fn base_ids(rom: Node) -> Vec<String> {
    let mut v: Vec<String> = rom
        .attribute("base")
        .map(str::to_string)
        .into_iter()
        .collect();
    v.extend(
        rom.children()
            .filter(|c| c.is_element() && c.tag_name().name() == "include")
            .filter_map(|c| c.text())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty()),
    );
    v
}

fn roms_of<'a, 'i>(doc: &'a Document<'i>) -> Vec<Node<'a, 'i>> {
    doc.descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "rom")
        .collect()
}

fn parse_xml<F>(src: &str, opts: &Options, resolve: F) -> Result<FastecuImport>
where
    F: Fn(&str) -> Option<String>,
{
    let main = Document::parse(src).map_err(|e| Error::Xml(e.to_string()))?;
    let main_roms = roms_of(&main);
    if main_roms.is_empty() {
        return Err(Error::NoRom);
    }
    let mut warnings = Vec::new();

    // Pick the rom.
    let selected = match &opts.rom {
        Some(id) => *main_roms
            .iter()
            .find(|r| xmlid(**r) == *id)
            .ok_or_else(|| Error::RomNotFound(id.clone()))?,
        None if main_roms.len() == 1 => main_roms[0],
        None => {
            let r = main_roms
                .iter()
                .copied()
                .find(|r| !base_ids(*r).is_empty())
                .unwrap_or(main_roms[0]);
            warnings.push(format!(
                "{} <rom> blocks in file; imported `{}` (choose another with --rom)",
                main_roms.len(),
                xmlid(r)
            ));
            r
        }
    };

    // Phase 1: fetch the sources of bases not in this document.
    let mut extras: Vec<(String, String)> = Vec::new();
    {
        let mut queue: VecDeque<String> = base_ids(selected).into();
        let mut seen: HashSet<String> = HashSet::from([xmlid(selected)]);
        while let Some(id) = queue.pop_front() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(r) = main_roms.iter().find(|r| xmlid(**r) == id) {
                queue.extend(base_ids(*r));
                continue;
            }
            match resolve(&id) {
                Some(s) => match Document::parse(&s) {
                    Ok(d) => {
                        let roms = roms_of(&d);
                        if let Some(r) = roms.iter().find(|r| xmlid(**r) == id).or(roms.first()) {
                            queue.extend(base_ids(*r));
                        }
                        drop(roms);
                        drop(d);
                        extras.push((id, s));
                    }
                    Err(e) => warnings.push(format!("base definition `{id}` does not parse: {e}")),
                },
                None => warnings.push(format!(
                    "base definition `{id}` not found; tables, scalings and axes only defined \
                     there are missing (pass --base-dir)"
                )),
            }
        }
    }

    // Phase 2: the chain of roms, most specific first.
    let extra_docs: Vec<(String, Document)> = extras
        .iter()
        .filter_map(|(id, s)| Document::parse(s).ok().map(|d| (id.clone(), d)))
        .collect();
    let lookup = |id: &str| -> Option<(usize, Node)> {
        if let Some(r) = main_roms.iter().find(|r| xmlid(**r) == id) {
            return Some((0, *r));
        }
        extra_docs.iter().enumerate().find_map(|(i, (eid, d))| {
            let roms = roms_of(d);
            roms.iter()
                .copied()
                .find(|r| xmlid(*r) == id)
                .or_else(|| (eid == id).then(|| roms.first().copied()).flatten())
                .map(|r| (i + 1, r))
        })
    };
    let mut chain: Vec<(usize, Node)> = Vec::new();
    let mut visited: HashSet<(usize, NodeId)> = HashSet::new();
    let mut stack = vec![(0usize, selected)];
    while let Some((di, r)) = stack.pop() {
        if !visited.insert((di, r.id())) {
            continue;
        }
        chain.push((di, r));
        for b in base_ids(r).iter().rev() {
            if let Some(n) = lookup(b) {
                stack.push(n);
            }
        }
    }

    let mut docs: Vec<&Document> = vec![&main];
    docs.extend(extra_docs.iter().map(|(_, d)| d));
    let used_docs: Vec<usize> = {
        let mut v: Vec<usize> = chain.iter().map(|(d, _)| *d).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let dialect = if used_docs
        .iter()
        .any(|d| crate::detect_dialect(docs[*d]) == crate::Dialect::RomRaider)
    {
        Source::RomRaider
    } else {
        Source::EcuFlash
    };

    // Named scalings: document-level <scalingbase>s, then each rom's
    // <scaling name=> children base-first so the most specific wins.
    let mut scalings: HashMap<String, Node> = HashMap::new();
    for d in &used_docs {
        for s in docs[*d]
            .descendants()
            .filter(|n| n.is_element() && n.tag_name().name() == "scalingbase")
        {
            if let Some(name) = s.attribute("name") {
                scalings.insert(name.to_string(), s);
            }
        }
    }
    for (_, rom) in chain.iter().rev() {
        for s in rom
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "scaling")
        {
            if let Some(name) = s.attribute("name") {
                scalings.insert(name.to_string(), s);
            }
        }
    }

    // Tables: layers per name, most specific first; order base-first.
    let mut order: Vec<String> = Vec::new();
    let mut layers: HashMap<String, Vec<Node>> = HashMap::new();
    let mut in_selected: HashSet<String> = HashSet::new();
    for (i, (_, rom)) in chain.iter().enumerate().rev() {
        for t in rom
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "table")
        {
            let Some(name) = t.attribute("name") else {
                continue;
            };
            if i == 0 {
                in_selected.insert(name.to_string());
            }
            let e = layers.entry(name.to_string()).or_insert_with(|| {
                order.push(name.to_string());
                Vec::new()
            });
            e.insert(0, t);
        }
    }

    // ROM identity: the selected rom, filling blanks from its bases.
    let mut rom_id = crate::parse_romid(selected);
    for (_, r) in chain.iter().skip(1) {
        let b = crate::parse_romid(*r);
        if rom_id.flashmethod.is_empty() {
            rom_id.flashmethod = b.flashmethod;
        }
        if rom_id.memmodel.is_empty() {
            rom_id.memmodel = b.memmodel;
        }
    }
    let filesize = chain
        .iter()
        .map(|(_, r)| {
            r.children()
                .find(|c| c.is_element() && c.tag_name().name() == "romid")
                .map(|x| crate::child_text(x, "filesize"))
                .unwrap_or_default()
        })
        .find_map(|s| parse_filesize(&s));
    let wrx02 = rom_id.flashmethod.eq_ignore_ascii_case("wrx02");
    let rom_size = filesize.or(wrx02.then_some(WRX02_ROM_SIZE));
    if wrx02 && filesize.is_none() {
        warnings.push(format!(
            "flash method wrx02: FastECU's 160 KiB ROM size assumed for its address fold \
             (0x{WRX02_ROM_SIZE:X})"
        ));
    }
    let env = Env {
        big_endian_cpu: big_endian_memmodel(&rom_id.memmodel),
        scalings,
        rom_size,
        wrx02,
        wrx02_assumed: wrx02 && filesize.is_none(),
    };

    let mut maps = Vec::new();
    let mut reports = Vec::new();
    let mut absent = 0usize;
    for name in &order {
        let l = Layers(layers[name].clone());
        // A base-definition table the selected rom never mentions, with no
        // address anywhere in the chain, is simply not part of this ROM
        // (bases are supersets) — FastECU does not show it either.
        if !in_selected.contains(name)
            && l.attr("address")
                .or_else(|| l.attr("storageaddress"))
                .is_none()
        {
            absent += 1;
            continue;
        }
        let (m, r) = build_table(name, &l, &env);
        maps.extend(m);
        reports.push(r);
    }
    if absent > 0 {
        warnings.push(format!(
            "{absent} base-definition table(s) are not present in this rom (no address); \
             not listed"
        ));
    }

    Ok(FastecuImport {
        source: dialect,
        rom_id,
        chain: chain.iter().map(|(_, r)| xmlid(*r)).collect(),
        maps,
        reports,
        warnings,
    })
}

/// Memory models of big-endian CPUs (Renesas SuperH, Motorola 68HC16,
/// Mitsubishi M32R, PowerPC).
pub(crate) fn big_endian_memmodel(m: &str) -> bool {
    let m = m.trim().to_ascii_uppercase();
    m.starts_with("SH")
        || m.contains("HC16")
        || m.starts_with("M32R")
        || m.starts_with("MPC")
        || m.starts_with("PPC")
}

/// `"160kb"`, `"1mb"`, `"0x28000"`, `"163840"` → bytes.
fn parse_filesize(s: &str) -> Option<u64> {
    let t = s.trim().to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    if let Some(n) = t.strip_suffix("kb") {
        return n.trim().parse::<u64>().ok().map(|v| v * 1024);
    }
    if let Some(n) = t.strip_suffix("mb") {
        return n.trim().parse::<u64>().ok().map(|v| v * 1024 * 1024);
    }
    if let Some(h) = t.strip_prefix("0x") {
        return u64::from_str_radix(h, 16).ok();
    }
    t.parse().ok()
}

struct Env<'a, 'i> {
    /// The rom's `memmodel` is a big-endian CPU.
    big_endian_cpu: bool,
    scalings: HashMap<String, Node<'a, 'i>>,
    rom_size: Option<u64>,
    wrx02: bool,
    wrx02_assumed: bool,
}

/// The definitions of one table (or axis) across the inheritance chain, most
/// specific first. An attribute comes from the first layer that has it.
#[derive(Clone)]
struct Layers<'a, 'i>(Vec<Node<'a, 'i>>);

impl<'a, 'i> Layers<'a, 'i> {
    fn attr(&self, k: &str) -> Option<&'a str> {
        self.0.iter().find_map(|n| n.attribute(k))
    }

    /// Child elements named `tag` of the first layer that has any.
    fn children(&self, tag: &str) -> Vec<Node<'a, 'i>> {
        for n in &self.0 {
            let v: Vec<Node> = n
                .children()
                .filter(|c| c.is_element() && c.tag_name().name() == tag)
                .collect();
            if !v.is_empty() {
                return v;
            }
        }
        Vec::new()
    }

    /// Axis sub-tables by role, each merged across layers. A child axis with
    /// no `type` takes its role from the same-named axis in another layer.
    ///
    /// EcuFlash calibrations name their axis overrides positionally (`<table
    /// name="X" address=..>`, `name="Y"`), so an untyped child axis is matched
    /// by name to a typed axis, then `X`/`Y` to the typed axis of that
    /// orientation, then by position among the typed axes.
    fn axes(&self) -> Vec<(Role, Layers<'a, 'i>)> {
        let mut named_role: HashMap<&str, Role> = HashMap::new();
        let mut typed: Vec<Role> = Vec::new();
        for n in &self.0 {
            for a in n
                .children()
                .filter(|c| c.is_element() && c.tag_name().name() == "table")
            {
                if let Some(r) = a.attribute("type").and_then(Role::of) {
                    if let Some(nm) = a.attribute("name") {
                        named_role.entry(nm).or_insert(r);
                    }
                    if !typed.contains(&r) {
                        typed.push(r);
                    }
                }
            }
        }
        let mut out: Vec<(Role, Layers)> = Vec::new();
        for n in &self.0 {
            let mut untyped = 0usize;
            for a in n
                .children()
                .filter(|c| c.is_element() && c.tag_name().name() == "table")
            {
                let role = a.attribute("type").and_then(Role::of).or_else(|| {
                    let nm = a.attribute("name").unwrap_or("");
                    let pos = untyped;
                    untyped += 1;
                    named_role.get(nm).copied().or_else(|| {
                        if nm.eq_ignore_ascii_case("x") {
                            typed.iter().copied().find(|r| r.is_x()).or(Some(Role::X))
                        } else if nm.eq_ignore_ascii_case("y") {
                            typed.iter().copied().find(|r| !r.is_x()).or(Some(Role::Y))
                        } else {
                            typed.get(pos).copied()
                        }
                    })
                });
                let Some(role) = role else { continue };
                match out.iter_mut().find(|(r, _)| *r == role) {
                    Some((_, l)) => l.0.push(a),
                    None => out.push((role, Layers(vec![a]))),
                }
            }
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    X,
    Y,
    StaticX,
    StaticY,
}

impl Role {
    fn of(ty: &str) -> Option<Role> {
        let t = ty.to_ascii_lowercase();
        let st = t.contains("static");
        if t.contains("x axis") {
            Some(if st { Role::StaticX } else { Role::X })
        } else if t.contains("y axis") {
            Some(if st { Role::StaticY } else { Role::Y })
        } else {
            None
        }
    }
    fn is_static(self) -> bool {
        matches!(self, Role::StaticX | Role::StaticY)
    }
    fn is_x(self) -> bool {
        matches!(self, Role::X | Role::StaticX)
    }
}

type Problem = (&'static str, String);

/// The storage + scaling a table or axis resolves to across its layers.
struct Resolved {
    storage: Option<String>,
    endian: Option<String>,
    expr: Option<String>,
    units: String,
    min: Option<f64>,
    max: Option<f64>,
    missing_ref: Option<String>,
}

fn num(n: Option<&str>) -> Option<f64> {
    n.and_then(|v| v.trim().parse().ok())
}

fn resolve(l: &Layers, env: &Env) -> Resolved {
    let mut r = Resolved {
        storage: l.attr("storagetype").map(str::to_string),
        endian: l.attr("endian").map(str::to_string),
        expr: None,
        units: String::new(),
        min: None,
        max: None,
        missing_ref: None,
    };
    // The scaling node: a RomRaider child <scaling> (inline or base=), or a
    // named scaling referenced by `scaling=` (EcuFlash).
    let child = l.children("scaling").into_iter().next();
    let named = |name: &str, r: &mut Resolved| -> Option<Node> {
        let n = env.scalings.get(name).copied();
        if n.is_none() {
            r.missing_ref = Some(name.to_string());
        }
        n
    };
    let node = match child {
        Some(c) => match c.attribute("base") {
            Some(b) => named(b, &mut r).map(|n| (n, Some(c))),
            None => Some((c, None)),
        },
        None => l
            .attr("scaling")
            .and_then(|s| named(s, &mut r))
            .map(|n| (n, None)),
    };
    if let Some((n, overlay)) = node {
        let pick = |k: &str| {
            overlay
                .and_then(|o| o.attribute(k))
                .or_else(|| n.attribute(k))
        };
        if r.storage.is_none() {
            r.storage = pick("storagetype").map(str::to_string);
        }
        if r.endian.is_none() {
            r.endian = pick("endian").map(str::to_string);
        }
        r.expr = pick("toexpr")
            .or_else(|| pick("expression"))
            .map(str::to_string);
        r.units = pick("units").unwrap_or("").to_string();
        r.min = num(pick("min"));
        r.max = num(pick("max"));
    }
    r
}

/// Storage string → type, refusing anything inexact.
fn storage_type(s: Option<&str>, r: &Resolved) -> std::result::Result<DataType, Problem> {
    let Some(s) = s else {
        return Err(match &r.missing_ref {
            Some(n) => (
                kinds::REFERENCE_MISSING,
                format!(
                    "scaling `{n}` not found (unresolved base definition?); storage type unknown"
                ),
            ),
            None => (kinds::STORAGE_UNKNOWN, "no storagetype".into()),
        });
    };
    match s.trim().to_ascii_lowercase().as_str() {
        "uint24" | "int24" => Err((
            kinds::ELEMENT_SIZE_UNSUPPORTED,
            format!("`{s}` (24-bit) cells cannot be stored exactly"),
        )),
        _ => {
            storage_to_type(s).ok_or((kinds::STORAGE_UNKNOWN, format!("unknown storagetype `{s}`")))
        }
    }
}

/// Byte order of a cell. FastECU reads floats big-endian whatever `endian`
/// says; on a big-endian CPU (`memmodel` SH*/HC16/M32R/PowerPC — every Subaru
/// ROM FastECU supports) that is simply the CPU's order, so the override is
/// clean. On any other or unknown memory model it is a warning.
fn endian_for(dt: DataType, endian: Option<&str>, env: &Env, warns: &mut Vec<Problem>) -> Endian {
    let little = endian
        .map(|e| matches!(e.trim().to_ascii_lowercase().as_str(), "little" | "lsb"))
        .unwrap_or(false);
    if dt == DataType::F32 && little {
        if env.big_endian_cpu {
            return Endian::Big;
        }
        warns.push((
            kinds::FLOAT_ENDIAN,
            "float declared endian=\"little\"; FastECU reads floats big-endian, so big-endian \
             is used — verify against a known image"
                .into(),
        ));
        return Endian::Big;
    }
    if little {
        Endian::Little
    } else {
        Endian::Big
    }
}

fn fit(expr: Option<&str>) -> std::result::Result<Scaling, String> {
    let Some(e) = expr else {
        return Ok(Scaling::default());
    };
    match linear_from_equation(&e.replace('x', "X")) {
        LinearFit::Linear { factor, offset } => Ok(Scaling { factor, offset }),
        _ => Err(e.trim().to_string()),
    }
}

/// FastECU parses `address` / `startpos` / `interval` as hex (`0x` optional).
fn hex(s: Option<&str>) -> Option<std::result::Result<u32, String>> {
    s.map(|v| crate::parse_u32_hex(v).ok_or_else(|| v.to_string()))
}

/// Placement of `cells` cells of `dt`: address, `startpos` shift, `interval`
/// (refused when > 1), `wrx02` fold, ROM bounds.
fn place(
    l: &Layers,
    dt: DataType,
    cells: u64,
    env: &Env,
    warns: &mut Vec<Problem>,
) -> std::result::Result<u32, Problem> {
    let addr = match hex(l.attr("address").or_else(|| l.attr("storageaddress"))) {
        Some(Ok(a)) => a,
        Some(Err(v)) => return Err((kinds::ADDRESS_MISSING, format!("unparsable address `{v}`"))),
        None => return Err((kinds::ADDRESS_MISSING, "no address".into())),
    };
    let get = |k: &str| -> std::result::Result<u32, Problem> {
        match hex(l.attr(k)) {
            None => Ok(1),
            Some(Ok(0)) => Ok(1),
            Some(Ok(v)) => Ok(v),
            Some(Err(v)) => Err((kinds::STRIDE_NONCONTIGUOUS, format!("unparsable {k} `{v}`"))),
        }
    };
    let (startpos, interval) = (get("startpos")?, get("interval")?);
    let size = u64::from(dt.byte_len());
    if interval > 1 && cells > 1 {
        return Err((
            kinds::STRIDE_NONCONTIGUOUS,
            format!(
                "interval {interval}: cells are interleaved every {} bytes; not representable",
                u64::from(interval) * size
            ),
        ));
    }
    let mut a = u64::from(addr) + u64::from(startpos - 1) * size;
    let len = cells * size;
    if let Some(rom) = env.rom_size {
        if env.wrx02 && a + len > rom && a >= 0x8000 {
            warns.push((
                kinds::ADDRESS_FOLDED,
                format!(
                    "wrx02: address 0x{a:X} is past the {}-byte ROM; folded by 0x8000 to 0x{:X} \
                     as FastECU does{}",
                    rom,
                    a - 0x8000,
                    if env.wrx02_assumed {
                        " (ROM size assumed)"
                    } else {
                        ""
                    }
                ),
            ));
            a -= 0x8000;
        }
        if a + len > rom {
            return Err((
                kinds::ADDRESS_OUTSIDE_IMAGE,
                format!(
                    "data 0x{a:X}..0x{:X} lies outside the {rom}-byte ROM",
                    a + len
                ),
            ));
        }
    }
    u32::try_from(a).map_err(|_| (kinds::ADDRESS_MISSING, format!("address 0x{a:X} overflows")))
}

fn is_switch(l: &Layers, r: &Resolved) -> bool {
    let ty = l.attr("type").unwrap_or("").to_ascii_lowercase();
    matches!(
        ty.as_str(),
        "switch" | "selectable" | "multiselectable" | "bloblist"
    ) || r
        .storage
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("bloblist"))
}

fn elements(l: &Layers) -> Option<u32> {
    l.attr("elements")
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| *v > 0)
}

fn static_values(l: &Layers) -> (Vec<f64>, Vec<String>) {
    let mut vals = Vec::new();
    let mut text = Vec::new();
    for d in l.children("data") {
        let t = d.text().unwrap_or("").trim();
        if t.is_empty() {
            continue;
        }
        match t.parse::<f64>() {
            Ok(v) => vals.push(v),
            Err(_) => text.push(t.to_string()),
        }
    }
    (vals, text)
}

fn build_axis(role: Role, l: &Layers, dim: u32, env: &Env, tr: &mut Tracker) -> Option<AxisDef> {
    if dim <= 1 {
        return None;
    }
    let id = if role.is_x() { "x" } else { "y" };
    let name = l.attr("name").unwrap_or(id).to_string();
    let res = resolve(l, env);
    let base = AxisDef {
        name: name.clone(),
        unit: res.units.clone(),
        count: dim,
        ..Default::default()
    };
    if role.is_static() {
        let (vals, text) = static_values(l);
        if !text.is_empty() {
            tr.warn(
                kinds::AXIS_CATEGORICAL,
                format!(
                    "{id}-axis `{name}` has text breakpoints ({}); dropped",
                    text.join(", ")
                ),
            );
            return Some(base);
        }
        if vals.len() as u32 != dim {
            tr.warn(
                kinds::AXIS_COUNT_MISMATCH,
                format!(
                    "{id}-axis `{name}` has {} static value(s) for a dimension of {dim}; dropped",
                    vals.len()
                ),
            );
            return Some(base);
        }
        return Some(AxisDef {
            values: vals,
            ..base
        });
    }
    if let Some(e) = elements(l).filter(|e| *e != dim) {
        tr.warn(
            kinds::AXIS_COUNT_MISMATCH,
            format!(
                "{id}-axis `{name}` declares {e} elements for a dimension of {dim}; using {dim}"
            ),
        );
    }
    if l.attr("address")
        .or_else(|| l.attr("storageaddress"))
        .is_none()
    {
        tr.warn(
            kinds::AXIS_UNRESOLVED,
            format!("{id}-axis `{name}` has no address; breakpoints unknown"),
        );
        return Some(base);
    }
    let mut warns = Vec::new();
    let placed = storage_type(res.storage.as_deref(), &res).and_then(|dt| {
        let endian = endian_for(dt, res.endian.as_deref(), env, &mut warns);
        place(l, dt, u64::from(dim), env, &mut warns).map(|a| (dt, endian, a))
    });
    for (k, m) in warns {
        tr.warn(k, format!("{id}-axis `{name}`: {m}"));
    }
    match placed {
        Ok((dt, endian, address)) => {
            let scaling = fit(res.expr.as_deref()).unwrap_or_else(|eq| {
                tr.warn(
                    kinds::AXIS_MATH_NONLINEAR,
                    format!("{id}-axis `{name}`: non-linear equation `{eq}`; kept raw"),
                );
                Scaling::default()
            });
            Some(AxisDef {
                address: Some(address),
                data_type: dt,
                endian,
                scaling,
                ..base
            })
        }
        Err((_, m)) => {
            tr.warn(
                kinds::AXIS_STORAGE,
                format!("{id}-axis `{name}` storage unusable ({m}); breakpoint address dropped"),
            );
            Some(base)
        }
    }
}

fn build_table(name: &str, l: &Layers, env: &Env) -> (Option<MapDef>, ObjectReport) {
    let mut tr = Tracker::new();
    let res = resolve(l, env);
    if is_switch(l, &res) {
        tr.refuse(
            kinds::UNSUPPORTED_OBJECT,
            "switch/selection table (enumerated byte patterns) — not a numeric map; not \
             imported as a writable map",
        );
        return (None, tr.finish(name, ObjectKind::Switch));
    }
    let axes = l.axes();
    let axis = |pred: fn(Role) -> bool| axes.iter().find(|(r, _)| pred(*r));
    let x = axis(|r| r == Role::X || r == Role::StaticX);
    let y = axis(|r| r == Role::Y || r == Role::StaticY);
    let ty = l.attr("type").unwrap_or("").to_ascii_uppercase();
    let size = |k: &str| {
        l.attr(k)
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|v| *v > 0)
    };
    let axis_len = |a: Option<&(Role, Layers)>| {
        a.and_then(|(r, l)| {
            if r.is_static() {
                let (v, t) = static_values(l);
                Some((v.len() + t.len()) as u32).filter(|n| *n > 0)
            } else {
                elements(l)
            }
        })
    };
    let kind = match ty.as_str() {
        "1D" => 1,
        "2D" => 2,
        "3D" => 3,
        _ => match (x.is_some(), y.is_some()) {
            (true, true) => 3,
            (false, false) => 1,
            _ => 2,
        },
    };
    // OpenTune convention: a curve is 1×N along x, whichever axis the source
    // used (FastECU does the same for 2D tables).
    let (rows, cols, xa, ya) = match kind {
        1 => (1, 1, None, None),
        2 => {
            let a = x.or(y);
            let n = axis_len(a).or(size("sizey")).or(size("sizex")).unwrap_or(1);
            (1, n, a, None)
        }
        _ => (
            size("sizey").or(axis_len(y)).unwrap_or(1),
            size("sizex").or(axis_len(x)).unwrap_or(1),
            x,
            y,
        ),
    };

    let mut warns = Vec::new();
    let placed = storage_type(res.storage.as_deref(), &res).and_then(|dt| {
        let endian = endian_for(dt, res.endian.as_deref(), env, &mut warns);
        place(l, dt, u64::from(rows) * u64::from(cols), env, &mut warns).map(|a| (dt, endian, a))
    });
    for (k, m) in warns {
        tr.warn(k, m);
    }
    let (dt, endian, address) = match placed {
        Ok(v) => v,
        Err((k, m)) => {
            tr.refuse(k, m);
            return (None, tr.finish(name, ObjectKind::Table));
        }
    };
    let scaling = fit(res.expr.as_deref()).unwrap_or_else(|eq| {
        tr.warn(
            kinds::MATH_NONLINEAR,
            format!("non-linear equation `{eq}` — imported pre-scaled (factor 1, offset 0)"),
        );
        Scaling::default()
    });
    let x_axis = xa.and_then(|(r, al)| build_axis(Role::X.max_static(*r), al, cols, env, &mut tr));
    let y_axis = ya.and_then(|(r, al)| build_axis(Role::Y.max_static(*r), al, rows, env, &mut tr));

    let mut notes: Vec<String> = Vec::new();
    if let Some(cat) = l.attr("category") {
        notes.push(format!("category: {cat}"));
    }
    if let Some(d) = l
        .children("description")
        .first()
        .and_then(|d| d.text())
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        notes.push(d.to_string());
    }
    if let Some(sp) = hex(l.attr("startpos"))
        .and_then(|r| r.ok())
        .filter(|v| *v > 1)
    {
        notes.push(format!("FastECU startpos {sp:#X} applied to the address"));
    }
    notes.extend(tr.note_lines());
    let map = MapDef {
        name: name.to_string(),
        notes: notes.join("; "),
        address,
        data_type: dt,
        endian,
        rows,
        cols,
        scaling,
        unit: res.units,
        x_axis,
        y_axis,
        min: res.min,
        max: res.max,
    };
    (Some(map), tr.finish(name, ObjectKind::Table))
}

impl Role {
    /// Keep `self`'s orientation but the static-ness of `src`.
    fn max_static(self, src: Role) -> Role {
        match (self.is_x(), src.is_static()) {
            (true, true) => Role::StaticX,
            (true, false) => Role::X,
            (false, true) => Role::StaticY,
            (false, false) => Role::Y,
        }
    }
}

// ---------------------------------------------------------------------------
// WinOLS MapPack CSV (FastECU's converter input)
// ---------------------------------------------------------------------------

/// Split one `;`-separated line, honouring `"` quoting.
fn split_csv(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ';' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// WinOLS `DataOrg` → (type, endian).
fn data_org(s: &str) -> Option<(DataType, Endian)> {
    Some(match s.trim() {
        "eByte" => (DataType::U8, Endian::Big),
        "eHiLo" => (DataType::U16, Endian::Big),
        "eLoHi" => (DataType::U16, Endian::Little),
        "eHiLoHiLo" => (DataType::U32, Endian::Big),
        "eLoHiLoHi" => (DataType::U32, Endian::Little),
        "eFloatHiLo" => (DataType::F32, Endian::Big),
        "eFloatLoHi" => (DataType::F32, Endian::Little),
        _ => return None,
    })
}

fn signed(dt: DataType) -> DataType {
    match dt {
        DataType::U8 => DataType::S8,
        DataType::U16 => DataType::S16,
        DataType::U32 => DataType::S32,
        other => other,
    }
}

fn csv_hex(s: &str) -> Option<u32> {
    let t = s.trim().trim_start_matches('$');
    crate::parse_u32_hex(t)
}

fn csv_num(s: &str) -> Option<f64> {
    s.trim().replace(',', ".").parse().ok()
}

fn parse_mappack_csv(src: &str) -> Result<FastecuImport> {
    let mut lines = src.lines().filter(|l| !l.trim().is_empty());
    let header = lines
        .next()
        .ok_or_else(|| Error::Csv("empty file".into()))?;
    let titles: Vec<String> = split_csv(header)
        .into_iter()
        .map(|t| t.trim().to_string())
        .collect();
    let col = |name: &str| titles.iter().position(|t| t.eq_ignore_ascii_case(name));
    for req in ["Name", "Fieldvalues.StartAddr", "DataOrg"] {
        if col(req).is_none() {
            return Err(Error::Csv(format!(
                "not a WinOLS MapPack CSV: missing column `{req}`"
            )));
        }
    }
    // A signedness column is not part of the layout FastECU documents; use
    // one when present, otherwise every map is warned.
    let signed_col = |prefix: &str| {
        titles.iter().position(|t| {
            let t = t.to_ascii_lowercase();
            t.starts_with(&prefix.to_ascii_lowercase()) && t.ends_with("signed")
        })
    };
    let (field_signed, x_signed, y_signed) = (
        signed_col("Fieldvalues.").or_else(|| signed_col("")),
        signed_col("AxisX."),
        signed_col("AxisY."),
    );

    let mut maps = Vec::new();
    let mut reports = Vec::new();
    for (lineno, line) in lines.enumerate() {
        let f = split_csv(line);
        let get = |name: &str| {
            col(name)
                .and_then(|i| f.get(i))
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
        };
        let truthy = |i: Option<usize>| {
            i.and_then(|i| f.get(i))
                .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        };
        let name = get("Name")
            .map(str::to_string)
            .unwrap_or_else(|| format!("row_{}", lineno + 2));
        let mut tr = Tracker::new();
        let refuse = |mut tr: Tracker, k: &'static str, m: String| {
            tr.refuse(k, m);
            tr.finish(name.clone(), ObjectKind::Table)
        };
        let Some((dt, endian)) = get("DataOrg").and_then(data_org) else {
            reports.push(refuse(
                tr,
                kinds::STORAGE_UNKNOWN,
                format!("unknown DataOrg `{}`", get("DataOrg").unwrap_or("")),
            ));
            continue;
        };
        let Some(address) = get("Fieldvalues.StartAddr").and_then(csv_hex) else {
            reports.push(refuse(
                tr,
                kinds::ADDRESS_MISSING,
                "no (valid) Fieldvalues.StartAddr".into(),
            ));
            continue;
        };
        let dim = |k: &str| {
            get(k)
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(1)
        };
        let (cols, rows) = (dim("Columns"), dim("Rows"));
        let dt = match truthy(field_signed) {
            Some(true) => signed(dt),
            Some(false) => dt,
            None => {
                if dt != DataType::F32 {
                    tr.warn(
                        kinds::SIGNEDNESS_UNKNOWN,
                        "MapPack CSV has no signedness column; cells imported unsigned",
                    );
                }
                dt
            }
        };
        let factor = get("Fieldvalues.Factor").and_then(csv_num).unwrap_or(1.0);
        let offset = get("Fieldvalues.Offset").and_then(csv_num).unwrap_or(0.0);
        let unit = get("Fieldvalues.Unit").unwrap_or("").to_string();

        let axis = |p: &str, dim: u32, sc: Option<usize>, tr: &mut Tracker| -> Option<AxisDef> {
            if dim <= 1 {
                return None;
            }
            let id = if p == "AxisX" { "x" } else { "y" };
            let g = |k: &str| get(&format!("{p}.{k}"));
            let mut a = AxisDef {
                name: g("Name").unwrap_or(id).to_string(),
                unit: g("Unit").unwrap_or("").to_string(),
                count: dim,
                ..Default::default()
            };
            let stored = g("DataHeader")
                .and_then(|h| h.parse::<i64>().ok())
                .map(|h| h > 0)
                .unwrap_or(true);
            match (
                stored,
                g("DataAddr").and_then(csv_hex),
                g("DataOrg").and_then(data_org),
            ) {
                (true, Some(addr), Some((adt, aend))) => {
                    let adt = match truthy(sc) {
                        Some(true) => signed(adt),
                        Some(false) => adt,
                        None => {
                            if adt != DataType::F32 {
                                tr.warn(
                                    kinds::SIGNEDNESS_UNKNOWN,
                                    format!("{id}-axis signedness not recorded; imported unsigned"),
                                );
                            }
                            adt
                        }
                    };
                    a.address = Some(addr);
                    a.data_type = adt;
                    a.endian = aend;
                    a.scaling = Scaling {
                        factor: g("Factor").and_then(csv_num).unwrap_or(1.0),
                        offset: g("Offset").and_then(csv_num).unwrap_or(0.0),
                    };
                }
                (true, Some(_), None) => tr.warn(
                    kinds::AXIS_STORAGE,
                    format!(
                        "{id}-axis DataOrg `{}` unknown; breakpoint address dropped",
                        g("DataOrg").unwrap_or("")
                    ),
                ),
                _ => tr.warn(
                    kinds::AXIS_UNRESOLVED,
                    format!("{id}-axis has no stored data address; breakpoints unknown"),
                ),
            }
            Some(a)
        };
        let x_axis = axis("AxisX", cols, x_signed, &mut tr);
        let y_axis = axis("AxisY", rows, y_signed, &mut tr);

        let mut notes = Vec::new();
        if let Some(c) = get("FolderName") {
            notes.push(format!("category: {c}"));
        }
        if let Some(c) = get("Comment") {
            notes.push(c.to_string());
        }
        notes.extend(tr.note_lines());
        maps.push(MapDef {
            name: name.clone(),
            notes: notes.join("; "),
            address,
            data_type: dt,
            endian,
            rows,
            cols,
            scaling: Scaling { factor, offset },
            unit,
            x_axis,
            y_axis,
            min: None,
            max: None,
        });
        reports.push(tr.finish(name, ObjectKind::Table));
    }
    Ok(FastecuImport {
        source: Source::MapPackCsv,
        rom_id: RomId::default(),
        chain: Vec::new(),
        maps,
        reports,
        warnings: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentune_xdf::Status;

    fn none(_: &str) -> Option<String> {
        None
    }

    fn rep<'a>(imp: &'a FastecuImport, name: &str) -> &'a ObjectReport {
        imp.reports
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("{name}: {:?}", imp.reports))
    }

    fn kinds_of(r: &ObjectReport) -> Vec<&'static str> {
        r.issues.iter().map(|i| i.kind).collect()
    }

    // EcuFlash dialect, FastECU extensions on top.
    const EF: &str = r#"<rom>
      <romid><xmlid>TESTEF01</xmlid><make>Subaru</make><flashmethod>sti05</flashmethod><memmodel>SH7058</memmodel></romid>
      <scaling name="RPM" units="rpm" toexpr="x*0.25" storagetype="uint16" endian="big"/>
      <scaling name="Pct" units="%" toexpr="x*100/255" storagetype="uint8" endian="big"/>
      <scaling name="F" units="g/s" toexpr="x" storagetype="float" endian="little"/>
      <scaling name="Sq" units="?" toexpr="x*x" storagetype="uint8"/>
      <scaling name="Tri" units="?" toexpr="x" storagetype="uint24"/>
      <scaling name="Sw" storagetype="bloblist"><data name="on" value="01"/><data name="off" value="00"/></scaling>
      <table name="Fuel" type="3D" category="Fueling" address="0x1000" scaling="Pct">
        <table name="RPM" type="X Axis" address="2000" elements="4" scaling="RPM"/>
        <table name="Load" type="Y Axis" address="0x2100" elements="3" scaling="RPM"/>
      </table>
      <table name="Shifted" type="2D" address="0x3000" scaling="Pct" startpos="3">
        <table name="RPM" type="X Axis" address="0x3100" elements="4" scaling="RPM"/>
      </table>
      <table name="Interleaved" type="2D" address="0x3200" scaling="Pct" interval="2">
        <table name="RPM" type="Static X Axis" elements="2"><data>1</data><data>2</data></table>
      </table>
      <table name="Airflow" type="1D" address="0x4000" scaling="F"/>
      <table name="Squared" type="1D" address="0x4100" scaling="Sq"/>
      <table name="Wide" type="1D" address="0x4200" scaling="Tri"/>
      <table name="Toggle" type="Switch" address="0x4300" scaling="Sw"/>
      <table name="Lost" type="1D" address="0x4400" scaling="Nope"/>
    </rom>"#;

    #[test]
    fn ecuflash_tables_with_fastecu_semantics() {
        let imp = parse_fastecu(EF, &Options::default(), none).unwrap();
        assert_eq!(imp.source, Source::EcuFlash);
        assert_eq!(imp.rom_id.xmlid, "TESTEF01");

        // Plain 3D map, bare-hex axis address (FastECU reads hex).
        let fuel = imp.maps.iter().find(|m| m.name == "Fuel").unwrap();
        assert_eq!((fuel.rows, fuel.cols, fuel.address), (3, 4, 0x1000));
        let x = fuel.x_axis.as_ref().unwrap();
        assert_eq!(x.address, Some(0x2000));
        assert!((x.scaling.factor - 0.25).abs() < 1e-12);
        assert!(fuel.notes.contains("category: Fueling"));
        assert_eq!(rep(&imp, "Fuel").status, Status::Clean);

        // startpos 3 with interval 1: address shifted by 2 cells, clean.
        let s = imp.maps.iter().find(|m| m.name == "Shifted").unwrap();
        assert_eq!((s.rows, s.cols, s.address), (1, 4, 0x3002));
        assert_eq!(rep(&imp, "Shifted").status, Status::Clean);

        // interval 2: interleaved -> refused.
        let r = rep(&imp, "Interleaved");
        assert_eq!(r.status, Status::Refused);
        assert_eq!(kinds_of(r), vec![kinds::STRIDE_NONCONTIGUOUS]);

        // float + endian=little -> FastECU reads big-endian; on an SH7058
        // (big-endian CPU) that is the CPU's own order: clean.
        let a = imp.maps.iter().find(|m| m.name == "Airflow").unwrap();
        assert_eq!((a.data_type, a.endian), (DataType::F32, Endian::Big));
        assert_eq!(rep(&imp, "Airflow").status, Status::Clean);
        // On another memory model the override is a warning.
        let tc = EF.replace("<memmodel>SH7058", "<memmodel>TC1796");
        let other = parse_fastecu(&tc, &Options::default(), none).unwrap();
        assert_eq!(kinds_of(rep(&other, "Airflow")), vec![kinds::FLOAT_ENDIAN]);

        // non-linear -> pre-scaled + warned.
        let sq = imp.maps.iter().find(|m| m.name == "Squared").unwrap();
        assert_eq!(sq.scaling, Scaling::default());
        assert_eq!(kinds_of(rep(&imp, "Squared")), vec![kinds::MATH_NONLINEAR]);

        // 24-bit, switch, missing scaling -> refused, with reasons.
        assert_eq!(
            kinds_of(rep(&imp, "Wide")),
            vec![kinds::ELEMENT_SIZE_UNSUPPORTED]
        );
        let t = rep(&imp, "Toggle");
        assert_eq!((t.object, t.status), (ObjectKind::Switch, Status::Refused));
        assert_eq!(kinds_of(rep(&imp, "Lost")), vec![kinds::REFERENCE_MISSING]);
        assert!(!imp
            .maps
            .iter()
            .any(|m| ["Wide", "Toggle", "Lost", "Interleaved"].contains(&m.name.as_str())));

        // Strict: the warned/refused tables fail it; the switch alone would not.
        let f = imp.strict_failures();
        assert!(f.iter().any(|l| l.contains("Squared")));
        assert!(!f.iter().any(|l| l.contains("Toggle")));
        assert!(matches!(imp.into_strict(), Err(Error::Strict(_))));
    }

    // RomRaider: two roms in one file, child inherits from the base by name
    // and only supplies addresses.
    const RR: &str = r#"<roms>
      <scalingbase name="rpm" units="RPM" expression="x" to_byte="x"/>
      <rom>
        <romid><xmlid>BASE1</xmlid></romid>
        <table type="3D" name="Boost" category="Boost" storagetype="uint16" endian="big" sizex="3" sizey="2">
          <scaling units="bar" expression="x*0.001" to_byte="x/0.001"/>
          <table type="X Axis" name="Throttle" storagetype="uint8"><scaling units="%" expression="x/2"/></table>
          <table type="Y Axis" name="Engine Speed" storagetype="uint16"><scaling base="rpm"/></table>
        </table>
        <table type="2D" name="Comp" storagetype="uint8" sizey="3">
          <scaling units="deg" expression="x-40"/>
          <table type="Static Y Axis" name="Gear" sizey="3"><data>1</data><data>2</data><data>3</data></table>
        </table>
        <table type="Switch" name="Flag" sizey="2"><state name="on" data="01 02"/><state name="off" data="00 00"/></table>
      </rom>
      <rom base="BASE1">
        <romid><xmlid>CAL1</xmlid><flashmethod>wrx02</flashmethod></romid>
        <table name="Boost" storageaddress="0x2E000">
          <table type="X Axis" name="Throttle" storageaddress="0x2D000"/>
          <table type="Y Axis" name="Engine Speed" storageaddress="0x1100"/>
        </table>
        <table name="Comp" storageaddress="0x1200"/>
        <table name="Flag" storageaddress="0x1300"/>
      </rom>
    </roms>"#;

    #[test]
    fn romraider_inheritance_and_wrx02_fold() {
        let imp = parse_fastecu(RR, &Options::default(), none).unwrap();
        assert_eq!(imp.source, Source::RomRaider);
        assert_eq!(imp.rom_id.xmlid, "CAL1", "the calibration, not the base");
        assert_eq!(imp.chain, vec!["CAL1", "BASE1"]);
        assert!(imp.warnings.iter().any(|w| w.contains("wrx02")));

        let b = imp.maps.iter().find(|m| m.name == "Boost").unwrap();
        // 0x2E000 is past the 160 KiB wrx02 ROM: folded by 0x8000.
        assert_eq!((b.rows, b.cols, b.address), (2, 3, 0x26000));
        assert_eq!(b.data_type, DataType::U16);
        assert!((b.scaling.factor - 0.001).abs() < 1e-12);
        let x = b.x_axis.as_ref().unwrap();
        assert_eq!((x.address, x.data_type), (Some(0x25000), DataType::U8));
        assert!((x.scaling.factor - 0.5).abs() < 1e-12);
        assert_eq!(b.y_axis.as_ref().unwrap().address, Some(0x1100));
        assert!(kinds_of(rep(&imp, "Boost"))
            .iter()
            .all(|k| *k == kinds::ADDRESS_FOLDED));

        // 2D with a static axis: a 1x3 curve with inline breakpoints.
        let c = imp.maps.iter().find(|m| m.name == "Comp").unwrap();
        assert_eq!((c.rows, c.cols, c.address), (1, 3, 0x1200));
        assert_eq!(c.x_axis.as_ref().unwrap().values, vec![1.0, 2.0, 3.0]);
        assert!((c.scaling.offset + 40.0).abs() < 1e-12);
        assert_eq!(rep(&imp, "Comp").status, Status::Clean);

        assert_eq!(rep(&imp, "Flag").object, ObjectKind::Switch);

        // A base table the calibration never mentions is not part of its ROM.
        let extra = RR.replace(
            "<table type=\"Switch\"",
            "<table type=\"1D\" name=\"OnlyInBase\" storagetype=\"uint8\"/><table type=\"Switch\"",
        );
        let imp = parse_fastecu(&extra, &Options::default(), none).unwrap();
        assert!(!imp.reports.iter().any(|r| r.name == "OnlyInBase"));
        assert!(imp
            .warnings
            .iter()
            .any(|w| w.contains("not present in this rom")));

        // Selecting the base explicitly imports its (address-less) tables as refusals.
        let base = parse_fastecu(
            RR,
            &Options {
                rom: Some("BASE1".into()),
            },
            none,
        )
        .unwrap();
        assert_eq!(kinds_of(rep(&base, "Comp")), vec![kinds::ADDRESS_MISSING]);
        assert!(matches!(
            parse_fastecu(
                RR,
                &Options {
                    rom: Some("NOPE".into())
                },
                none
            ),
            Err(Error::RomNotFound(_))
        ));
    }

    #[test]
    fn external_base_via_resolver_and_missing_base_reported() {
        let base = r#"<rom><romid><xmlid>16BITBASE</xmlid><flashmethod>wrx02</flashmethod></romid>
            <scaling name="Deg" units="deg" toexpr="x*0.5" storagetype="uint8"/>
            <table name="Timing" type="3D" scaling="Deg">
              <table name="RPM" type="X Axis" elements="2" scaling="Deg"/>
              <table name="Load" type="Y Axis" elements="2" scaling="Deg"/>
            </table></rom>"#;
        let child = r#"<rom><romid><xmlid>A4SHC01G</xmlid><filesize>192kb</filesize></romid>
            <include>16BITBASE</include>
            <table name="Timing" address="2e219">
              <table name="RPM" address="2e000"/><table name="Load" address="2e100"/>
            </table></rom>"#;
        let imp = parse_fastecu(child, &Options::default(), |id| {
            (id == "16BITBASE").then(|| base.to_string())
        })
        .unwrap();
        assert_eq!(imp.chain, vec!["A4SHC01G", "16BITBASE"]);
        let t = &imp.maps[0];
        // Declared 192 KiB ROM: 0x2E219 fits, so no fold.
        assert_eq!((t.rows, t.cols, t.address), (2, 2, 0x2E219));
        // Axes matched to the base by name (child omits `type`).
        assert_eq!(t.x_axis.as_ref().unwrap().address, Some(0x2E000));
        assert_eq!(t.y_axis.as_ref().unwrap().address, Some(0x2E100));
        assert_eq!(rep(&imp, "Timing").status, Status::Clean);

        // EcuFlash positional axis overrides (`name="X"`/`"Y"`) and a child
        // `elements` override.
        let positional = child
            .replace(
                r#"name="RPM" address="2e000""#,
                r#"name="X" address="2e000""#,
            )
            .replace(
                r#"name="Load" address="2e100""#,
                r#"name="Y" address="2e100" elements="3""#,
            );
        let imp = parse_fastecu(&positional, &Options::default(), |id| {
            (id == "16BITBASE").then(|| base.to_string())
        })
        .unwrap();
        let t = &imp.maps[0];
        assert_eq!((t.rows, t.cols), (3, 2));
        assert_eq!(t.x_axis.as_ref().unwrap().address, Some(0x2E000));
        assert_eq!(t.y_axis.as_ref().unwrap().address, Some(0x2E100));

        let imp = parse_fastecu(child, &Options::default(), none).unwrap();
        assert!(imp
            .warnings
            .iter()
            .any(|w| w.contains("16BITBASE") && w.contains("not found")));
        assert_eq!(kinds_of(rep(&imp, "Timing")), vec![kinds::STORAGE_UNKNOWN]);
    }

    #[test]
    fn data_outside_declared_rom_is_refused() {
        let x = r#"<rom><romid><xmlid>S</xmlid><filesize>0x100</filesize></romid>
            <scaling name="B" toexpr="x" storagetype="uint16"/>
            <table name="Edge" type="1D" address="0xFF" scaling="B"/></rom>"#;
        let imp = parse_fastecu(x, &Options::default(), none).unwrap();
        assert_eq!(
            kinds_of(rep(&imp, "Edge")),
            vec![kinds::ADDRESS_OUTSIDE_IMAGE]
        );
    }

    const CSV: &str = "Name;FolderName;DataOrg;Columns;Rows;Fieldvalues.Name;Fieldvalues.Unit;Fieldvalues.Factor;Fieldvalues.Offset;Fieldvalues.StartAddr;AxisX.Name;AxisX.DataOrg;AxisX.Unit;AxisX.Factor;AxisX.DataHeader;AxisX.DataAddr;AxisY.Name;AxisY.DataOrg;AxisY.Unit;AxisY.Factor;AxisY.DataHeader;AxisY.DataAddr;Comment\n\
        Boost target;Boost;eHiLo;8;6;Boost;hPa;0,1;-100;$1C000;RPM;eHiLo;rpm;1;1;$1BF00;Load;eByte;%;0,5;1;$1BF20;\"desired; boost\"\n\
        Odd;Misc;eTriple;1;1;x;y;1;0;$100;;;;;;;;;;;;;\n\
        LE map;Misc;eLoHi;4;1;x;y;1;0;$200;n;eLoHi;u;1;0;;;;;;;;\n";

    #[test]
    fn mappack_csv() {
        let imp = parse_fastecu(CSV, &Options::default(), none).unwrap();
        assert_eq!(imp.source, Source::MapPackCsv);
        let m = &imp.maps[0];
        assert_eq!(
            (m.name.as_str(), m.rows, m.cols, m.address),
            ("Boost target", 6, 8, 0x1C000)
        );
        assert_eq!((m.data_type, m.endian), (DataType::U16, Endian::Big));
        // Offset honoured (FastECU's converter drops it); comma decimals.
        assert!((m.scaling.factor - 0.1).abs() < 1e-12);
        assert!((m.scaling.offset + 100.0).abs() < 1e-12);
        assert_eq!(m.unit, "hPa");
        assert!(
            m.notes.contains("desired; boost"),
            "quoted ; kept: {}",
            m.notes
        );
        let y = m.y_axis.as_ref().unwrap();
        assert_eq!((y.address, y.data_type), (Some(0x1BF20), DataType::U8));
        assert!((y.scaling.factor - 0.5).abs() < 1e-12);
        // No signedness column: warned (map + both axes).
        assert!(kinds_of(rep(&imp, "Boost target"))
            .iter()
            .all(|k| *k == kinds::SIGNEDNESS_UNKNOWN));

        // Unknown DataOrg is refused, not silently uint8.
        assert_eq!(kinds_of(rep(&imp, "Odd")), vec![kinds::STORAGE_UNKNOWN]);
        // DataHeader 0 -> no stored axis -> unresolved, warned.
        let le = imp.maps.iter().find(|m| m.name == "LE map").unwrap();
        assert_eq!(le.endian, Endian::Little);
        assert!(kinds_of(rep(&imp, "LE map")).contains(&kinds::AXIS_UNRESOLVED));

        // With a signedness column the warning goes away.
        let signed_csv = "Name;DataOrg;Fieldvalues.StartAddr;Fieldvalues.Signed\nK;eHiLo;$10;1\n";
        let imp = parse_fastecu(signed_csv, &Options::default(), none).unwrap();
        assert_eq!(imp.maps[0].data_type, DataType::S16);
        assert_eq!(rep(&imp, "K").status, Status::Clean);

        assert!(matches!(
            parse_fastecu("a;b\n1;2\n", &Options::default(), none),
            Err(Error::Csv(_))
        ));
    }

    #[test]
    fn csv_split_quotes() {
        assert_eq!(split_csv("a;\"b;c\";d"), vec!["a", "b;c", "d"]);
        assert_eq!(split_csv("\"x\"\"y\""), vec!["x\"y"]);
    }

    #[test]
    fn filesize_forms() {
        assert_eq!(parse_filesize("160kb"), Some(163840));
        assert_eq!(parse_filesize("1MB"), Some(1 << 20));
        assert_eq!(parse_filesize("0x28000"), Some(0x28000));
        assert_eq!(parse_filesize(""), None);
    }
}
