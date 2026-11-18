//! The section parser and `#if` preprocessor.
use crate::command::CommandTemplate;
use crate::model::*;
use crate::text::{
    braced_inner, logical_lines, parse_number, quoted_inner, split_key_value, split_values,
    text_value,
};
use std::collections::{BTreeMap, BTreeSet};

/// Conditions to apply while parsing, on top of the file's own `#set` /
/// `#unset` lines (these win over the file).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IniOptions {
    pub set: BTreeSet<String>,
    pub unset: BTreeSet<String>,
}

impl IniOptions {
    pub fn with_set<I: IntoIterator<Item = S>, S: Into<String>>(set: I) -> Self {
        Self {
            set: set.into_iter().map(Into::into).collect(),
            unset: BTreeSet::new(),
        }
    }
}

struct Frame {
    parent_active: bool,
    active: bool,
    taken: bool,
}

#[derive(Default)]
struct PageLists {
    n_pages: Option<usize>,
    sizes: Vec<String>,
    identifier: Vec<String>,
    read: Vec<String>,
    value_write: Vec<String>,
    chunk_write: Vec<String>,
    burn: Vec<String>,
    crc32: Vec<String>,
}

struct State<'o> {
    ini: Ini,
    opts: &'o IniOptions,
    file_set: BTreeSet<String>,
    stack: Vec<Frame>,
    section: String,
    page: Option<usize>,
    last_offset: usize,
    lists: PageLists,
    skipped: BTreeMap<String, usize>,
    first_line: BTreeMap<String, usize>,
    menu_dialog: String,
}

const MODELLED: [&str; 9] = [
    "MegaTune",
    "TunerStudio",
    "Constants",
    "PcVariables",
    "OutputChannels",
    "TableEditor",
    "CurveEditor",
    "Menu",
    "SettingGroups",
];

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().trim_end_matches(';').to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Some(true),
        "off" | "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

fn parse_usize(s: &str) -> Option<usize> {
    parse_number(s.trim_end_matches(';'))
        .filter(|v| *v >= 0.0)
        .map(|v| v as usize)
}

fn parse_num(item: &str) -> Num {
    if let Some(e) = braced_inner(item) {
        return Num::Expr(e.to_owned());
    }
    match parse_number(item) {
        Some(v) => Num::Lit(v),
        None => Num::Expr(item.trim().to_owned()),
    }
}

/// `[16x16]` / `[ 8]` / `[16 x 16]` -> (cols, rows).
fn parse_shape(item: &str) -> Option<(usize, usize)> {
    let inner = item.trim().strip_prefix('[')?.strip_suffix(']')?;
    let mut it = inner.split(['x', 'X']);
    let a = it.next()?.trim().parse::<usize>().ok()?;
    match it.next() {
        None => Some((1, a)),
        Some(b) => Some((a, b.trim().parse::<usize>().ok()?)),
    }
}

/// `[lo:hi]` -> (lo, hi, label offset). Older MegaSquirt definitions write
/// `[4:7+1]`: bits 4..7, values shown counting from 1.
fn parse_bits(item: &str) -> Option<(u8, u8, u32)> {
    let inner = item.trim().strip_prefix('[')?.strip_suffix(']')?;
    let (a, b) = inner.split_once(':')?;
    let (b, off) = match b.split_once('+') {
        Some((h, o)) => (h, o.trim().parse::<u32>().ok()?),
        None => (b, 0),
    };
    let lo = a.trim().parse::<u8>().ok()?;
    let hi = b.trim().parse::<u8>().ok()?;
    (lo <= hi && hi < 64).then_some((lo, hi, off))
}

/// Repair common hand-editing slips TunerStudio tolerates, returning a note
/// when something was changed:
/// - an empty field where the offset belongs (`bits, U08, , 3876, ...`),
/// - an offset fused with its bit range (`188[0:0]`),
/// - a missing comma after a quoted string (`"sparks"  1.0`).
fn normalize_items(items: &[String], has_offset: bool) -> (Vec<String>, Option<String>) {
    let mut out: Vec<String> = Vec::with_capacity(items.len() + 2);
    let mut notes = Vec::new();
    let mut queue: std::collections::VecDeque<String> = items.iter().cloned().collect();
    // Re-examine the parts of a split item, in order.
    fn requeue(q: &mut std::collections::VecDeque<String>, parts: Vec<String>) {
        for p in parts.into_iter().rev() {
            q.push_front(p);
        }
    }
    while let Some(it) = queue.pop_front() {
        let pos = out.len();
        if has_offset && pos == 2 && it.is_empty() && !queue.is_empty() {
            notes.push("empty field before the offset ignored");
            continue;
        }
        // `U08    11` / `164   "mins"`: a missing comma after the type or
        // the offset.
        let plain = !it.starts_with(['"', '{', '[']);
        if plain && (pos == 1 || (has_offset && pos == 2)) && it.split_whitespace().count() > 1 {
            let (head, tail) = it.split_once(char::is_whitespace).unwrap_or((&it, ""));
            requeue(&mut queue, vec![head.to_owned(), tail.trim().to_owned()]);
            notes.push("missing comma after the type/offset");
            continue;
        }
        // `[0:3]  "label"`: a missing comma after a bit range or shape.
        if it.starts_with('[') {
            if let Some(close) = it.find(']') {
                let rest = it[close + 1..].trim();
                if !rest.is_empty() {
                    requeue(&mut queue, vec![it[..=close].to_owned(), rest.to_owned()]);
                    notes.push("missing comma after a bracket");
                    continue;
                }
            }
        }
        // `188[0:0]`: an offset fused with its bit range.
        if has_offset && pos == 2 {
            if let Some(b) = it.find('[') {
                if b > 0 && it.ends_with(']') {
                    requeue(
                        &mut queue,
                        vec![it[..b].trim().to_owned(), it[b..].to_owned()],
                    );
                    notes.push("offset and bit range split");
                    continue;
                }
            }
        }
        // `"sparks"  1.0`: a missing comma after a quoted string.
        if let Some(inner) = it.strip_prefix('"') {
            if let Some(close) = inner.find('"') {
                let end = close + 2;
                let rest = it[end..].trim();
                if !rest.is_empty() && !inner[..close].ends_with('\\') {
                    let mut parts = vec![it[..end].to_owned()];
                    parts.extend(rest.split_whitespace().map(str::to_owned));
                    out.extend(parts);
                    notes.push("missing comma after a quoted string");
                    continue;
                }
            }
        }
        out.push(it);
    }
    notes.dedup();
    let note = (!notes.is_empty()).then(|| notes.join("; "));
    (out, note)
}

/// `xBins = a, b`, tolerating a missing comma (`a b`).
fn axis_items(items: &[String]) -> Vec<String> {
    match items {
        [one] if one.split_whitespace().count() == 2 => {
            one.split_whitespace().map(str::to_owned).collect()
        }
        _ => items.to_vec(),
    }
}

fn command(item: Option<&String>) -> Result<Option<CommandTemplate>, String> {
    let Some(item) = item else { return Ok(None) };
    let inner = quoted_inner(item).unwrap_or(item.as_str());
    if inner.is_empty() {
        return Ok(None);
    }
    CommandTemplate::parse(inner).map(Some)
}

impl<'o> State<'o> {
    fn warn(&mut self, line: usize, kind: WarnKind, msg: impl Into<String>) {
        self.ini.warnings.push(Warning {
            line,
            section: self.section.clone(),
            kind,
            message: msg.into(),
        });
    }

    fn active(&self) -> bool {
        self.stack.last().map_or(true, |f| f.active)
    }

    fn is_set(&self, name: &str) -> bool {
        if self.opts.set.contains(name) {
            return true;
        }
        if self.opts.unset.contains(name) {
            return false;
        }
        self.file_set.contains(name)
    }

    fn eval_cond(&self, cond: &str) -> bool {
        let cond = cond.trim();
        cond.split("||").any(|alt| {
            alt.split("&&").all(|atom| {
                let a = atom.trim();
                let a = a
                    .strip_prefix("defined(")
                    .and_then(|x| x.strip_suffix(')'))
                    .unwrap_or(a)
                    .trim();
                match a.strip_prefix('!') {
                    Some(n) => !self.is_set(n.trim()),
                    None => self.is_set(a),
                }
            })
        })
    }

    /// Expand `$name` references to `#define` lists, recursively.
    fn expand(&self, items: &[String], depth: usize) -> Vec<String> {
        let mut out = Vec::new();
        for it in items {
            match it.strip_prefix('$') {
                Some(name) if depth < 16 => match self.ini.defines.get(name.trim()) {
                    Some(list) => out.extend(self.expand(list, depth + 1)),
                    None => out.push(it.clone()),
                },
                _ => out.push(it.clone()),
            }
        }
        out
    }

    fn directive(&mut self, no: usize, text: &str) {
        let body = text[1..].trim_start();
        let (word, rest) = match body.find(|c: char| c.is_whitespace()) {
            Some(i) => (&body[..i], body[i..].trim()),
            None => (body, ""),
        };
        match word {
            "if" | "ifdef" | "ifndef" => {
                let parent = self.active();
                let mut v = self.eval_cond(rest);
                if word == "ifndef" {
                    v = !v;
                }
                self.stack.push(Frame {
                    parent_active: parent,
                    active: parent && v,
                    taken: v,
                });
            }
            "elif" => {
                let v = self.eval_cond(rest);
                match self.stack.last_mut() {
                    Some(f) => {
                        let take = !f.taken && v;
                        f.active = f.parent_active && take;
                        f.taken |= take;
                    }
                    None => self.warn(no, WarnKind::Invalid, "#elif without #if"),
                }
            }
            "else" => match self.stack.last_mut() {
                Some(f) => {
                    f.active = f.parent_active && !f.taken;
                    f.taken = true;
                }
                None => self.warn(no, WarnKind::Invalid, "#else without #if"),
            },
            "endif" => {
                if self.stack.pop().is_none() {
                    self.warn(no, WarnKind::Invalid, "#endif without #if");
                }
            }
            _ if !self.active() => {}
            "set" => {
                self.file_set.insert(rest.to_owned());
            }
            "unset" => {
                self.file_set.remove(rest);
            }
            "define" => match split_key_value(rest) {
                Some((name, value)) => {
                    let items = self.expand(&split_values(value), 0);
                    self.ini.defines.insert(name.to_owned(), items);
                }
                None => self.warn(no, WarnKind::Invalid, format!("bad #define: {rest}")),
            },
            other => self.warn(
                no,
                WarnKind::Ignored,
                format!("directive #{other} not supported; ignored"),
            ),
        }
    }

    fn line(&mut self, no: usize, text: &str) {
        if text.starts_with('#') {
            self.directive(no, text);
            return;
        }
        if !self.active() {
            return;
        }
        if let Some(name) = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
            self.section = name.trim().to_owned();
            if !self.ini.sections.contains(&self.section) {
                self.ini.sections.push(self.section.clone());
                self.first_line.insert(self.section.clone(), no);
            }
            return;
        }
        if !MODELLED.contains(&self.section.as_str()) {
            *self.skipped.entry(self.section.clone()).or_default() += 1;
            return;
        }
        let Some((key, value)) = split_key_value(text) else {
            self.warn(
                no,
                WarnKind::Invalid,
                format!("not a key = value line: {text}"),
            );
            return;
        };
        let (key, value) = (key.to_owned(), value.to_owned());
        match self.section.as_str() {
            "MegaTune" | "TunerStudio" => self.header_key(no, &key, &value),
            "Constants" => self.constants_key(no, &key, &value),
            "PcVariables" => self.pc_key(no, &key, &value),
            "OutputChannels" => self.och_key(no, &key, &value),
            "TableEditor" => self.table_key(no, &key, &value),
            "CurveEditor" => self.curve_key(no, &key, &value),
            "Menu" => self.menu_key(no, &key, &value),
            "SettingGroups" => self.setting_key(no, &key, &value),
            _ => {}
        }
    }

    fn header_key(&mut self, no: usize, key: &str, value: &str) {
        let first = split_values(value).into_iter().next().unwrap_or_default();
        match key {
            "signature" => {
                self.ini.signature = Some(text_value(&first));
            }
            "queryCommand" => self.set_cmd(no, &first, |c, t| c.query_command = t),
            "versionInfo" => self.set_cmd(no, &first, |c, t| c.version_info = t),
            "MTversion" => self.ini.mt_version = Some(text_value(&first)),
            "iniSpecVersion" => self.ini.ini_spec_version = Some(text_value(&first)),
            "messageEnvelopeFormat" => {
                self.ini.comms.message_envelope = Some(text_value(&first));
            }
            _ => self.warn(no, WarnKind::Ignored, format!("key '{key}' not modelled")),
        }
    }

    fn set_cmd(&mut self, no: usize, item: &str, put: fn(&mut Comms, Option<CommandTemplate>)) {
        match command(Some(&item.to_owned())) {
            Ok(t) => put(&mut self.ini.comms, t),
            Err(e) => self.warn(no, WarnKind::Invalid, e),
        }
    }

    fn constants_key(&mut self, no: usize, key: &str, value: &str) {
        let items = split_values(value);
        let first = items.first().cloned().unwrap_or_default();
        let ms = |v: &str| parse_number(v.trim_end_matches(';')).map(|x| x.max(0.0) as u64);
        match key {
            "page" => match parse_usize(&first) {
                Some(p) if p >= 1 => {
                    self.page = Some(p - 1);
                    self.last_offset = 0;
                }
                _ => self.warn(no, WarnKind::Invalid, format!("bad page number '{first}'")),
            },
            "endianness" => {
                self.ini.endianness = match first.to_ascii_lowercase().as_str() {
                    "little" => Endian::Little,
                    "big" => Endian::Big,
                    other => {
                        self.warn(
                            no,
                            WarnKind::Invalid,
                            format!("unknown endianness '{other}'"),
                        );
                        self.ini.endianness
                    }
                }
            }
            "nPages" => self.lists.n_pages = parse_usize(&first),
            "pageSize" => self.lists.sizes = items,
            "pageIdentifier" => self.lists.identifier = items,
            "pageReadCommand" => self.lists.read = items,
            "pageValueWrite" => self.lists.value_write = items,
            "pageChunkWrite" => self.lists.chunk_write = items,
            "burnCommand" => self.lists.burn = items,
            "crc32CheckCommand" => self.lists.crc32 = items,
            "blockingFactor" => self.ini.comms.blocking_factor = parse_usize(&first),
            "tableBlockingFactor" => self.ini.comms.table_blocking_factor = parse_usize(&first),
            "pageActivationDelay" => self.ini.comms.page_activation_delay_ms = ms(&first),
            "interWriteDelay" => self.ini.comms.inter_write_delay_ms = ms(&first),
            "blockReadTimeout" => self.ini.comms.block_read_timeout_ms = ms(&first),
            "delayAfterPortOpen" => self.ini.comms.delay_after_port_open_ms = ms(&first),
            "writeBlocks" => self.ini.comms.write_blocks = parse_bool(&first),
            "tsWriteBlocks" => self.ini.comms.ts_write_blocks = parse_bool(&first),
            "messageEnvelopeFormat" => {
                self.ini.comms.message_envelope = Some(text_value(&first));
            }
            _ if is_constant_class(&first) => {
                let page = self.page;
                if page.is_none() {
                    self.warn(
                        no,
                        WarnKind::Invalid,
                        format!("constant '{key}' before any 'page ='"),
                    );
                    return;
                }
                let (items, note) = normalize_items(&items, true);
                if let Some(n) = note {
                    self.warn(no, WarnKind::Ignored, format!("{key}: tolerated: {n}"));
                }
                match self.constant(no, key, &items, true, page) {
                    Ok(c) => {
                        self.last_offset = c.offset;
                        self.push_constant(no, c, false);
                    }
                    Err(e) => self.warn(no, WarnKind::Invalid, format!("{key}: {e}")),
                }
            }
            _ => self.warn(no, WarnKind::Ignored, format!("key '{key}' not modelled")),
        }
    }

    fn push_constant(&mut self, no: usize, c: Constant, pc: bool) {
        let list = if pc {
            &self.ini.pc_variables
        } else {
            &self.ini.constants
        };
        if list.iter().any(|x| x.name == c.name) {
            let n = c.name.clone();
            self.warn(
                no,
                WarnKind::Reference,
                format!("duplicate '{n}'; first definition kept"),
            );
            return;
        }
        if pc {
            self.ini.pc_variables.push(c);
        } else {
            self.ini.constants.push(c);
        }
    }

    fn pc_key(&mut self, no: usize, key: &str, value: &str) {
        let items = split_values(value);
        let first = items.first().cloned().unwrap_or_default();
        if !is_constant_class(&first) {
            self.warn(
                no,
                WarnKind::Ignored,
                format!("PC variable '{key}' form not modelled"),
            );
            return;
        }
        let (items, note) = normalize_items(&items, false);
        if let Some(n) = note {
            self.warn(no, WarnKind::Ignored, format!("{key}: tolerated: {n}"));
        }
        match self.constant(no, key, &items, false, None) {
            Ok(c) => self.push_constant(no, c, true),
            Err(e) => self.warn(no, WarnKind::Invalid, format!("{key}: {e}")),
        }
    }

    /// Parse `class, type, [offset,] ...` into a [`Constant`].
    fn constant(
        &self,
        no: usize,
        name: &str,
        items: &[String],
        has_offset: bool,
        page: Option<usize>,
    ) -> Result<Constant, String> {
        let class = items[0].to_ascii_lowercase();
        let ty = items
            .get(1)
            .and_then(|t| DataType::parse(t))
            .ok_or_else(|| format!("unknown type '{}'", items.get(1).map_or("", |s| s)))?;
        let mut i = 2;
        let offset = if has_offset {
            let o = items.get(2).ok_or("missing offset")?;
            i = 3;
            if o.eq_ignore_ascii_case("lastOffset") {
                self.last_offset
            } else {
                parse_usize(o).ok_or_else(|| format!("bad offset '{o}'"))?
            }
        } else {
            0
        };
        let mut c = Constant {
            name: name.to_owned(),
            page,
            offset,
            ty,
            kind: Kind::Scalar,
            units: String::new(),
            scale: Num::Lit(1.0),
            translate: Num::Lit(0.0),
            min: None,
            max: None,
            digits: None,
            no_msq_save: items.iter().any(|s| s.eq_ignore_ascii_case("noMsqSave")),
            line: no,
        };
        let rest = &items[i.min(items.len())..];
        let numeric = |c: &mut Constant, r: &[String]| {
            if let Some(u) = r.first() {
                c.units = text_value(u);
            }
            if let Some(s) = r.get(1) {
                c.scale = parse_num(s);
            }
            if let Some(s) = r.get(2) {
                c.translate = parse_num(s);
            }
            c.min = r.get(3).map(|s| parse_num(s));
            c.max = r.get(4).map(|s| parse_num(s));
            c.digits = r.get(5).map(|s| parse_num(s));
        };
        match class.as_str() {
            "scalar" => {
                if ty == DataType::Ascii {
                    return Err("ASCII scalar".into());
                }
                numeric(&mut c, rest);
            }
            "array" => {
                let shape = rest.first().ok_or("missing shape")?;
                let (cols, rows) =
                    parse_shape(shape).ok_or_else(|| format!("bad shape '{shape}'"))?;
                if cols == 0 || rows == 0 {
                    return Err(format!("empty shape '{shape}'"));
                }
                c.kind = Kind::Array { cols, rows };
                numeric(&mut c, &rest[1..]);
            }
            "bits" => {
                let spec = rest.first().ok_or("missing [lo:hi]")?;
                let (lo, hi, label_offset) =
                    parse_bits(spec).ok_or_else(|| format!("bad bit range '{spec}'"))?;
                // PC variables are not stored in the ECU; TunerStudio lets
                // their bit ranges exceed the nominal type (MS3 `portLabels`).
                if page.is_some() && ((hi as usize) >= ty.size() * 8 || ty.is_float()) {
                    return Err(format!("bit range {spec} does not fit {ty:?}"));
                }
                let opts: Vec<String> = self
                    .expand(&rest[1..], 0)
                    .iter()
                    .filter(|s| !s.eq_ignore_ascii_case("noMsqSave") && braced_inner(s).is_none())
                    .map(|s| text_value(s))
                    .collect();
                c.kind = Kind::Bits {
                    lo,
                    hi,
                    options: opts,
                    label_offset,
                };
            }
            "string" => {
                let len = rest
                    .first()
                    .and_then(|s| parse_usize(s))
                    .ok_or("missing string length")?;
                c.kind = Kind::String { len };
            }
            other => return Err(format!("unknown class '{other}'")),
        }
        Ok(c)
    }

    fn och_key(&mut self, no: usize, key: &str, value: &str) {
        let items = split_values(value);
        let first = items.first().cloned().unwrap_or_default();
        match key {
            "ochGetCommand" => return self.set_cmd(no, &first, |c, t| c.och_get_command = t),
            "ochBlockSize" => {
                self.ini.comms.och_block_size = parse_usize(&first);
                return;
            }
            _ => {}
        }
        let kind = if let Some(e) = braced_inner(&first) {
            Ok(ChannelKind::Expr { expr: e.to_owned() })
        } else if is_constant_class(&first) {
            self.channel(&items)
        } else {
            self.warn(no, WarnKind::Ignored, format!("key '{key}' not modelled"));
            return;
        };
        match kind {
            Ok(kind) => {
                let units = match &kind {
                    ChannelKind::Expr { .. } => items.get(1).map(|u| text_value(u)),
                    _ => items
                        .get(3)
                        .filter(|u| quoted_inner(u).is_some())
                        .map(|u| text_value(u)),
                }
                .unwrap_or_default();
                if self.ini.output_channels.iter().any(|c| c.name == key) {
                    self.warn(
                        no,
                        WarnKind::Reference,
                        format!("duplicate channel '{key}'; first kept"),
                    );
                    return;
                }
                self.ini.output_channels.push(OutputChannel {
                    name: key.to_owned(),
                    kind,
                    units,
                    line: no,
                });
            }
            Err(e) => self.warn(no, WarnKind::Invalid, format!("{key}: {e}")),
        }
    }

    fn channel(&self, items: &[String]) -> Result<ChannelKind, String> {
        let class = items[0].to_ascii_lowercase();
        let ty = items
            .get(1)
            .and_then(|t| DataType::parse(t))
            .ok_or("unknown type")?;
        let offset = items
            .get(2)
            .and_then(|o| parse_usize(o))
            .ok_or("bad offset")?;
        Ok(match class.as_str() {
            "scalar" => ChannelKind::Scalar {
                ty,
                offset,
                scale: items.get(4).map_or(Num::Lit(1.0), |s| parse_num(s)),
                translate: items.get(5).map_or(Num::Lit(0.0), |s| parse_num(s)),
            },
            "bits" => {
                let (lo, hi, _) = items
                    .get(3)
                    .and_then(|s| parse_bits(s))
                    .ok_or("bad bit range")?;
                ChannelKind::Bits { ty, offset, lo, hi }
            }
            "array" => {
                let (c, r) = items
                    .get(3)
                    .and_then(|s| parse_shape(s))
                    .ok_or("bad shape")?;
                ChannelKind::Array {
                    ty,
                    offset,
                    len: c * r,
                }
            }
            other => return Err(format!("channel class '{other}' not modelled")),
        })
    }

    fn table_key(&mut self, no: usize, key: &str, value: &str) {
        let items = split_values(value);
        let axis = |items: &[String]| {
            items.first().map(|c| AxisRef {
                constant: c.clone(),
                channel: items.get(1).cloned().filter(|s| !s.is_empty()),
            })
        };
        if key == "table" {
            if items.len() < 3 {
                self.warn(no, WarnKind::Invalid, "table needs id, map3d id and title");
                return;
            }
            self.ini.tables.push(TableDef {
                id: items[0].clone(),
                map3d_id: items[1].clone(),
                title: text_value(&items[2]),
                page: items
                    .get(3)
                    .and_then(|p| parse_usize(p))
                    .and_then(|p| p.checked_sub(1)),
                x_bins: None,
                y_bins: None,
                z_bins: None,
                xy_labels: Vec::new(),
                up_down_labels: Vec::new(),
                topic_help: None,
                line: no,
            });
            return;
        }
        let Some(t) = self.ini.tables.last_mut() else {
            self.warn(no, WarnKind::Invalid, format!("'{key}' before any table"));
            return;
        };
        match key {
            "xBins" => t.x_bins = axis(&axis_items(&items)),
            "yBins" => t.y_bins = axis(&axis_items(&items)),
            "zBins" => t.z_bins = items.first().cloned(),
            "xyLabels" => t.xy_labels = items.iter().map(|s| text_value(s)).collect(),
            "upDownLabel" => t.up_down_labels = items.iter().map(|s| text_value(s)).collect(),
            "topicHelp" => t.topic_help = items.first().map(|s| text_value(s)),
            _ => {} // presentation (gridHeight, gridOrient, ...)
        }
    }

    fn curve_key(&mut self, no: usize, key: &str, value: &str) {
        let items = split_values(value);
        if key == "curve" {
            if items.is_empty() {
                self.warn(no, WarnKind::Invalid, "curve needs an id");
                return;
            }
            self.ini.curves.push(CurveDef {
                id: items[0].clone(),
                title: items.get(1).map(|s| text_value(s)).unwrap_or_default(),
                column_labels: Vec::new(),
                x_bins: None,
                y_bins: Vec::new(),
                x_axis: Vec::new(),
                y_axis: Vec::new(),
                topic_help: None,
                line: no,
            });
            return;
        }
        let Some(c) = self.ini.curves.last_mut() else {
            self.warn(no, WarnKind::Invalid, format!("'{key}' before any curve"));
            return;
        };
        let ax = axis_items(&items);
        let axis = ax.first().map(|k| AxisRef {
            constant: k.clone(),
            channel: ax.get(1).cloned().filter(|s| !s.is_empty()),
        });
        match key {
            "columnLabel" => c.column_labels = items.iter().map(|s| text_value(s)).collect(),
            "xBins" => c.x_bins = axis,
            "yBins" => c.y_bins.extend(axis),
            "xAxis" => c.x_axis = items,
            "yAxis" => c.y_axis = items,
            "topicHelp" => c.topic_help = items.first().map(|s| text_value(s)),
            _ => {}
        }
    }

    fn menu_key(&mut self, no: usize, key: &str, value: &str) {
        let items = split_values(value);
        match key {
            "menuDialog" => {
                self.menu_dialog = items.first().cloned().unwrap_or_default();
            }
            "menu" => {
                let title = items.first().map(|s| text_value(s)).unwrap_or_default();
                self.ini.menus.push(Menu {
                    dialog: self.menu_dialog.clone(),
                    title,
                    items: Vec::new(),
                });
            }
            "subMenu" | "groupChildMenu" | "groupMenu" => {
                let Some(m) = self.ini.menus.last_mut() else {
                    self.warn(no, WarnKind::Invalid, format!("'{key}' before any menu"));
                    return;
                };
                let (target, label) = if key == "groupMenu" {
                    (
                        String::new(),
                        items.first().map(|s| text_value(s)).unwrap_or_default(),
                    )
                } else {
                    (
                        items.first().cloned().unwrap_or_default(),
                        items.get(1).map(|s| text_value(s)).unwrap_or_default(),
                    )
                };
                let condition = items
                    .iter()
                    .skip(1)
                    .find_map(|s| braced_inner(s))
                    .map(str::to_owned);
                m.items.push(MenuItem {
                    target,
                    label,
                    condition,
                });
            }
            _ => self.warn(no, WarnKind::Ignored, format!("key '{key}' not modelled")),
        }
    }

    fn setting_key(&mut self, no: usize, key: &str, value: &str) {
        let items = split_values(value);
        let name = items.first().cloned().unwrap_or_default();
        let label = items.get(1).map(|s| text_value(s)).unwrap_or_default();
        match key {
            "settingGroup" => self.ini.setting_groups.push(SettingGroup {
                name,
                label,
                options: Vec::new(),
            }),
            "settingOption" => match self.ini.setting_groups.last_mut() {
                Some(g) => g.options.push((name, label)),
                None => self.warn(no, WarnKind::Invalid, "settingOption before settingGroup"),
            },
            _ => self.warn(no, WarnKind::Ignored, format!("key '{key}' not modelled")),
        }
    }

    fn finish(mut self) -> Ini {
        if !self.stack.is_empty() {
            let n = self.stack.len();
            self.warn(
                0,
                WarnKind::Invalid,
                format!("{n} unterminated #if block(s)"),
            );
        }
        for (sec, n) in std::mem::take(&mut self.skipped) {
            let line = self.first_line.get(&sec).copied().unwrap_or(0);
            let what = if sec.is_empty() {
                "lines before the first section".to_owned()
            } else {
                format!("section [{sec}]")
            };
            self.ini.warnings.push(Warning {
                line,
                section: sec,
                kind: WarnKind::Ignored,
                message: format!("{what} not modelled; {n} line(s) skipped"),
            });
        }
        self.build_pages();
        self.check_references();
        let mut conds: BTreeSet<String> = self.file_set.clone();
        conds.extend(self.opts.set.iter().cloned());
        for u in &self.opts.unset {
            conds.remove(u);
        }
        self.ini.conditions = conds;
        self.ini.rebuild_index();
        self.ini
    }

    fn build_pages(&mut self) {
        let l = std::mem::take(&mut self.lists);
        let n = l
            .n_pages
            .unwrap_or(if l.sizes.is_empty() { 0 } else { l.sizes.len() });
        let pick = |v: &Vec<String>, i: usize| -> Option<String> {
            if v.len() == 1 {
                v.first().cloned()
            } else {
                v.get(i).cloned()
            }
        };
        for i in 0..n {
            let mut p = PageDef {
                index: i,
                ..PageDef::default()
            };
            match pick(&l.sizes, i).as_deref().and_then(parse_usize) {
                Some(s) => p.size = s,
                None => self.warn(0, WarnKind::Invalid, format!("page {} has no size", i + 1)),
            }
            let mut errs = Vec::new();
            let mut cmd = |v: &Vec<String>| match command(pick(v, i).as_ref()) {
                Ok(t) => t,
                Err(e) => {
                    errs.push(e);
                    None
                }
            };
            p.identifier = cmd(&l.identifier);
            p.read = cmd(&l.read);
            p.value_write = cmd(&l.value_write);
            p.chunk_write = cmd(&l.chunk_write);
            p.burn = cmd(&l.burn);
            p.crc32 = cmd(&l.crc32);
            for e in errs {
                self.warn(0, WarnKind::Invalid, format!("page {}: {e}", i + 1));
            }
            self.ini.pages.push(p);
        }
    }

    fn check_references(&mut self) {
        let mut warns = Vec::new();
        for c in &self.ini.constants {
            let Some(pg) = c.page else { continue };
            match self.ini.pages.get(pg) {
                None => warns.push((
                    c.line,
                    format!("'{}' is on page {} which is not declared", c.name, pg + 1),
                )),
                Some(p) if c.offset + c.byte_len() > p.size => warns.push((
                    c.line,
                    format!(
                        "'{}' ends at byte {} beyond page {} size {}",
                        c.name,
                        c.offset + c.byte_len(),
                        pg + 1,
                        p.size
                    ),
                )),
                _ => {}
            }
        }
        let len_of = |ini: &Ini, n: &str| ini.constant(n).map(Constant::elements);
        // The index is needed for lookups below.
        self.ini.rebuild_index();
        for t in &self.ini.tables {
            let x = t
                .x_bins
                .as_ref()
                .map(|a| (a.constant.as_str(), len_of(&self.ini, &a.constant)));
            let y = t
                .y_bins
                .as_ref()
                .map(|a| (a.constant.as_str(), len_of(&self.ini, &a.constant)));
            let z = t.z_bins.as_deref().map(|n| (n, self.ini.constant(n)));
            for (name, found) in [
                x.map(|(n, l)| (n, l.is_some())),
                y.map(|(n, l)| (n, l.is_some())),
            ]
            .into_iter()
            .flatten()
            {
                if !found {
                    warns.push((
                        t.line,
                        format!("table '{}': bins '{name}' not defined", t.id),
                    ));
                }
            }
            match z {
                Some((name, None)) => {
                    warns.push((t.line, format!("table '{}': z '{name}' not defined", t.id)))
                }
                Some((name, Some(zc))) => {
                    if let Kind::Array { cols, rows } = zc.kind {
                        let xl = x.and_then(|(_, l)| l);
                        let yl = y.and_then(|(_, l)| l);
                        if xl.is_some_and(|l| l != cols) || yl.is_some_and(|l| l != rows) {
                            warns.push((
                                t.line,
                                format!(
                                    "table '{}': '{name}' is [{cols}x{rows}] but bins are {xl:?} x {yl:?}",
                                    t.id
                                ),
                            ));
                        }
                    }
                }
                None => warns.push((t.line, format!("table '{}' has no zBins", t.id))),
            }
        }
        for c in &self.ini.curves {
            for a in c.x_bins.iter().chain(c.y_bins.iter()) {
                if self.ini.constant(&a.constant).is_none() {
                    warns.push((
                        c.line,
                        format!("curve '{}': '{}' not defined", c.id, a.constant),
                    ));
                }
            }
        }
        let sec = std::mem::take(&mut self.section);
        for (line, m) in warns {
            self.warn(line, WarnKind::Reference, m);
        }
        self.section = sec;
    }
}

fn is_constant_class(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "scalar" | "bits" | "array" | "string"
    )
}

/// Parse a definition's text. Never fails: problems become [`Ini::warnings`].
pub fn parse(src: &str, opts: &IniOptions) -> Ini {
    let mut st = State {
        ini: Ini::default(),
        opts,
        file_set: BTreeSet::new(),
        stack: Vec::new(),
        section: String::new(),
        page: None,
        last_offset: 0,
        lists: PageLists::default(),
        skipped: BTreeMap::new(),
        first_line: BTreeMap::new(),
        menu_dialog: String::new(),
    };
    for l in logical_lines(src) {
        st.line(l.no, &l.text);
    }
    st.finish()
}
