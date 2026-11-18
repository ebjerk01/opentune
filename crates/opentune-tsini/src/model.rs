//! The parsed definition: pages, constants, output channels, editors, menus.
use crate::command::CommandTemplate;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Byte order of multi-byte values and of `%2o`/`%2c` command fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Endian {
    /// MegaSquirt's default when a definition does not say.
    #[default]
    Big,
    Little,
}

/// Storage type of a constant or channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DataType {
    U08,
    S08,
    U16,
    S16,
    U32,
    S32,
    F32,
    F64,
    /// `string, ASCII` constants.
    Ascii,
}

impl DataType {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_uppercase().as_str() {
            "U08" | "U8" => Self::U08,
            "S08" | "S8" => Self::S08,
            "U16" => Self::U16,
            "S16" => Self::S16,
            "U32" => Self::U32,
            "S32" => Self::S32,
            "F32" => Self::F32,
            "F64" => Self::F64,
            "ASCII" => Self::Ascii,
            _ => return None,
        })
    }

    /// Bytes per element (1 for ASCII characters).
    pub fn size(self) -> usize {
        match self {
            Self::U08 | Self::S08 | Self::Ascii => 1,
            Self::U16 | Self::S16 => 2,
            Self::U32 | Self::S32 | Self::F32 => 4,
            Self::F64 => 8,
        }
    }

    pub fn is_float(self) -> bool {
        matches!(self, Self::F32 | Self::F64)
    }

    pub fn is_signed(self) -> bool {
        matches!(self, Self::S08 | Self::S16 | Self::S32) || self.is_float()
    }

    /// Smallest and largest raw value (integers only; floats are unbounded).
    pub fn raw_range(self) -> (f64, f64) {
        match self {
            Self::U08 | Self::Ascii => (0.0, 255.0),
            Self::S08 => (-128.0, 127.0),
            Self::U16 => (0.0, 65535.0),
            Self::S16 => (-32768.0, 32767.0),
            Self::U32 => (0.0, 4_294_967_295.0),
            Self::S32 => (-2_147_483_648.0, 2_147_483_647.0),
            Self::F32 => (f32::MIN as f64, f32::MAX as f64),
            Self::F64 => (f64::MIN, f64::MAX),
        }
    }
}

/// A number or a `{ expression }` the definition gives instead.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Num {
    Lit(f64),
    Expr(String),
}

impl Num {
    pub fn lit(&self) -> Option<f64> {
        match self {
            Self::Lit(v) => Some(*v),
            Self::Expr(_) => None,
        }
    }
}

/// What shape a constant has.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum Kind {
    Scalar,
    /// `[cols x rows]`; a 1-D `[n]` list is `cols = 1, rows = n`. Memory is
    /// row-major: element `(x, y)` is at index `y * cols + x`.
    Array {
        cols: usize,
        rows: usize,
    },
    /// Bits `lo..=hi` of the storage word; `options[i]` labels value `i`.
    /// `label_offset` is the `+n` of the older `[4:7+1]` form: synthesised
    /// labels count from `n` (stored 0 shows as "1").
    Bits {
        lo: u8,
        hi: u8,
        options: Vec<String>,
        label_offset: u32,
    },
    String {
        len: usize,
    },
}

/// A tunable value (`[Constants]`) or a PC-side variable (`[PcVariables]`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Constant {
    pub name: String,
    /// 0-based page index; `None` for a PC variable (not stored in the ECU).
    pub page: Option<usize>,
    pub offset: usize,
    pub ty: DataType,
    pub kind: Kind,
    pub units: String,
    pub scale: Num,
    pub translate: Num,
    pub min: Option<Num>,
    pub max: Option<Num>,
    pub digits: Option<Num>,
    /// `noMsqSave`: TunerStudio does not store it in a tune file.
    pub no_msq_save: bool,
    /// 1-based line in the definition.
    pub line: usize,
}

impl Constant {
    /// Number of elements (1 for scalars, bits and strings).
    pub fn elements(&self) -> usize {
        match &self.kind {
            Kind::Array { cols, rows } => cols * rows,
            _ => 1,
        }
    }

    /// Bytes the constant occupies in its page.
    pub fn byte_len(&self) -> usize {
        match &self.kind {
            Kind::String { len } => *len,
            _ => self.elements() * self.ty.size(),
        }
    }

    /// Number of option slots a bit field has (`2^(hi-lo+1)`).
    pub fn bit_slots(&self) -> usize {
        match &self.kind {
            Kind::Bits { lo, hi, .. } => 1usize << (hi.saturating_sub(*lo) as usize + 1).min(31),
            _ => 0,
        }
    }

    /// The label of bit value `i`; missing labels are synthesised as the
    /// number, as the definition format specifies.
    pub fn bit_label(&self, i: usize) -> String {
        match &self.kind {
            Kind::Bits {
                options,
                label_offset,
                ..
            } => options
                .get(i)
                .cloned()
                .unwrap_or_else(|| (i + *label_offset as usize).to_string()),
            _ => i.to_string(),
        }
    }

    pub fn digits_lit(&self) -> Option<usize> {
        self.digits
            .as_ref()
            .and_then(Num::lit)
            .map(|d| d.max(0.0) as usize)
    }
}

/// Per-page definition and commands.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct PageDef {
    /// 0-based index (the `.msq` `number`); the definition's `page = n` is n-1.
    pub index: usize,
    pub size: usize,
    pub identifier: Option<CommandTemplate>,
    pub read: Option<CommandTemplate>,
    pub chunk_write: Option<CommandTemplate>,
    pub value_write: Option<CommandTemplate>,
    pub burn: Option<CommandTemplate>,
    pub crc32: Option<CommandTemplate>,
}

/// Communication settings collected from `[MegaTune]`, `[TunerStudio]`,
/// `[Constants]` and `[OutputChannels]`.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Comms {
    pub query_command: Option<CommandTemplate>,
    pub version_info: Option<CommandTemplate>,
    pub och_get_command: Option<CommandTemplate>,
    pub och_block_size: Option<usize>,
    pub blocking_factor: Option<usize>,
    pub table_blocking_factor: Option<usize>,
    pub page_activation_delay_ms: Option<u64>,
    pub inter_write_delay_ms: Option<u64>,
    pub block_read_timeout_ms: Option<u64>,
    pub delay_after_port_open_ms: Option<u64>,
    pub write_blocks: Option<bool>,
    pub ts_write_blocks: Option<bool>,
    /// `messageEnvelopeFormat`, e.g. `msEnvelope_1.0` (CRC32 framing).
    pub message_envelope: Option<String>,
}

impl Comms {
    /// Whether the definition asks for the CRC32 "msEnvelope" framing.
    pub fn uses_envelope(&self) -> bool {
        self.message_envelope
            .as_deref()
            .is_some_and(|e| e.trim().to_ascii_lowercase().starts_with("msenvelope"))
    }
}

/// How a realtime channel is computed.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "class", rename_all = "snake_case")]
pub enum ChannelKind {
    Scalar {
        ty: DataType,
        offset: usize,
        scale: Num,
        translate: Num,
    },
    Bits {
        ty: DataType,
        offset: usize,
        lo: u8,
        hi: u8,
    },
    /// Arrays in the realtime block are listed but not decoded.
    Array {
        ty: DataType,
        offset: usize,
        len: usize,
    },
    /// `name = { expression } [, "units"]`.
    Expr { expr: String },
}

/// An `[OutputChannels]` entry.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OutputChannel {
    pub name: String,
    pub kind: ChannelKind,
    pub units: String,
    pub line: usize,
}

/// `xBins = constant[, channel]`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AxisRef {
    pub constant: String,
    /// Realtime channel that marks the operating point.
    pub channel: Option<String>,
}

/// A `[TableEditor]` 3-D table.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TableDef {
    pub id: String,
    pub map3d_id: String,
    pub title: String,
    /// 0-based page, as given on the `table =` line.
    pub page: Option<usize>,
    pub x_bins: Option<AxisRef>,
    pub y_bins: Option<AxisRef>,
    pub z_bins: Option<String>,
    pub xy_labels: Vec<String>,
    pub up_down_labels: Vec<String>,
    pub topic_help: Option<String>,
    pub line: usize,
}

/// A `[CurveEditor]` 2-D curve. Several `yBins` lines draw several lines.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CurveDef {
    pub id: String,
    pub title: String,
    pub column_labels: Vec<String>,
    pub x_bins: Option<AxisRef>,
    pub y_bins: Vec<AxisRef>,
    pub x_axis: Vec<String>,
    pub y_axis: Vec<String>,
    pub topic_help: Option<String>,
    pub line: usize,
}

/// A `[Menu]` entry.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MenuItem {
    /// Dialog / table / curve id, or `std_separator`.
    pub target: String,
    pub label: String,
    pub condition: Option<String>,
}

/// A top-level `menu = "&Title"` with its sub-menus.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Menu {
    pub dialog: String,
    pub title: String,
    pub items: Vec<MenuItem>,
}

/// A `[SettingGroups]` group: a flag, or a choice between options.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SettingGroup {
    pub name: String,
    pub label: String,
    /// `(condition name, label)`; `DEFAULT` means "none of the others".
    pub options: Vec<(String, String)>,
}

/// How serious a [`Warning`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WarnKind {
    /// A key or section OpenTune does not model; skipped on purpose.
    Ignored,
    /// A line in a modelled section that could not be parsed.
    Invalid,
    /// Parsed, but refers to something missing or inconsistent (a table's
    /// bins, a constant outside its page, a duplicate name).
    Reference,
}

/// A line the parser skipped or did not understand. Parsing continues.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Warning {
    pub line: usize,
    pub section: String,
    pub kind: WarnKind,
    pub message: String,
}

/// Where a name resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Constant(usize),
    Pc(usize),
}

/// A parsed TunerStudio-format ECU definition.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Ini {
    pub signature: Option<String>,
    pub ini_spec_version: Option<String>,
    pub mt_version: Option<String>,
    pub endianness: Endian,
    pub pages: Vec<PageDef>,
    pub comms: Comms,
    pub constants: Vec<Constant>,
    pub pc_variables: Vec<Constant>,
    pub output_channels: Vec<OutputChannel>,
    pub tables: Vec<TableDef>,
    pub curves: Vec<CurveDef>,
    pub menus: Vec<Menu>,
    pub setting_groups: Vec<SettingGroup>,
    /// `#define` lists, `$name` references expanded.
    pub defines: BTreeMap<String, Vec<String>>,
    /// Conditions (`#set` names) active after parsing.
    pub conditions: BTreeSet<String>,
    /// Every section header seen, in order of first appearance.
    pub sections: Vec<String>,
    pub warnings: Vec<Warning>,
    #[serde(skip)]
    index: HashMap<String, Slot>,
}

impl Ini {
    pub(crate) fn rebuild_index(&mut self) {
        self.index.clear();
        for (i, c) in self.constants.iter().enumerate() {
            self.index
                .entry(c.name.clone())
                .or_insert(Slot::Constant(i));
        }
        for (i, c) in self.pc_variables.iter().enumerate() {
            self.index.entry(c.name.clone()).or_insert(Slot::Pc(i));
        }
    }

    /// A constant or PC variable by name.
    pub fn constant(&self, name: &str) -> Option<&Constant> {
        match self.index.get(name)? {
            Slot::Constant(i) => self.constants.get(*i),
            Slot::Pc(i) => self.pc_variables.get(*i),
        }
    }

    /// Constants stored in page `page` (0-based), in definition order.
    pub fn page_constants(&self, page: usize) -> impl Iterator<Item = &Constant> {
        self.constants.iter().filter(move |c| c.page == Some(page))
    }

    pub fn output_channel(&self, name: &str) -> Option<&OutputChannel> {
        self.output_channels.iter().find(|c| c.name == name)
    }

    pub fn table(&self, id: &str) -> Option<&TableDef> {
        self.tables.iter().find(|t| t.id == id || t.map3d_id == id)
    }

    pub fn curve(&self, id: &str) -> Option<&CurveDef> {
        self.curves.iter().find(|c| c.id == id)
    }

    /// Total bytes of all pages.
    pub fn total_page_bytes(&self) -> usize {
        self.pages.iter().map(|p| p.size).sum()
    }

    /// Warnings of one kind.
    pub fn warnings_of(&self, kind: WarnKind) -> impl Iterator<Item = &Warning> {
        self.warnings.iter().filter(move |w| w.kind == kind)
    }

    /// True when no modelled line failed to parse ([`WarnKind::Invalid`]).
    pub fn is_clean(&self) -> bool {
        self.warnings_of(WarnKind::Invalid).next().is_none()
    }
}
