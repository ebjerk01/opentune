//! The aftermarket-ECU panel's OFFLINE state (port of `TsState` in the
//! webview app's `feat_ts.rs`): the INI with its `#set` conditions, the
//! working tune the panel edits, and the tune last read from the ECU (the
//! base of a write plan). Pure: no serial port, no adapter. Presentation and
//! value parsing come from `opentune_tsini::present`, the write plan from
//! `opentune_tsproto::plan_write`.
//!
//! Edits are validated: a value outside the INI's limits, an out-of-range bit
//! option or a value the storage type cannot hold is refused, never clamped.

use opentune_tsini::present::{diff_rows, fmt_value, parse_value};
use opentune_tsini::value::range_issues;
use opentune_tsini::{
    load_ini, AxisRef, Constant, Ini, IniOptions, Kind, Msq, MsqLoadOptions, MsqMeta, MsqReport,
    Tune, Value as TsValue, WarnKind,
};
use opentune_tsproto::plan_write;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What a loaded INI defines (the panel's summary).
#[derive(Clone, Debug)]
pub(crate) struct IniInfo {
    pub path: PathBuf,
    pub signature: Option<String>,
    pub envelope: bool,
    /// `(size, burnable)` per page.
    pub pages: Vec<(usize, bool)>,
    pub constants: usize,
    pub pc_variables: usize,
    pub channels: usize,
    /// `(id, title)`.
    pub tables: Vec<(String, String)>,
    pub curves: Vec<(String, String)>,
    pub conditions: Vec<String>,
    /// `(invalid, reference, ignored)` warning counts.
    pub warnings: (usize, usize, usize),
    pub first_invalid: Vec<String>,
}

impl IniInfo {
    fn of(ini: &Ini, path: &Path) -> Self {
        let n = |k: WarnKind| ini.warnings_of(k).count();
        Self {
            path: path.to_path_buf(),
            signature: ini.signature.clone(),
            envelope: ini.comms.uses_envelope(),
            pages: ini
                .pages
                .iter()
                .map(|p| (p.size, p.burn.is_some()))
                .collect(),
            constants: ini.constants.len(),
            pc_variables: ini.pc_variables.len(),
            channels: ini.output_channels.len(),
            tables: ini
                .tables
                .iter()
                .map(|t| (t.id.clone(), t.title.clone()))
                .collect(),
            curves: ini
                .curves
                .iter()
                .map(|c| (c.id.clone(), c.title.clone()))
                .collect(),
            conditions: ini.conditions.iter().cloned().collect(),
            warnings: (
                n(WarnKind::Invalid),
                n(WarnKind::Reference),
                n(WarnKind::Ignored),
            ),
            first_invalid: ini
                .warnings_of(WarnKind::Invalid)
                .take(20)
                .map(|w| format!("line {} [{}]: {}", w.line, w.section, w.message))
                .collect(),
        }
    }
}

/// The shape of a constant, for its editor.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Class {
    Scalar,
    Bits(Vec<String>),
    String(usize),
    Array { cols: usize, rows: usize },
}

/// One constant of the working tune.
#[derive(Clone, Debug)]
pub(crate) struct ValueRow {
    pub name: String,
    pub class: Class,
    pub units: String,
    pub page: Option<usize>,
    pub value: Option<TsValue>,
    /// The value as text, or why it cannot be read.
    pub display: Result<String, String>,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

fn class_of(c: &Constant) -> Class {
    match &c.kind {
        Kind::Scalar => Class::Scalar,
        Kind::Bits { .. } => Class::Bits(
            (0..c.bit_slots().min(opentune_tsini::present::MAX_OPTIONS))
                .map(|i| c.bit_label(i))
                .collect(),
        ),
        Kind::String { len } => Class::String(*len),
        Kind::Array { cols, rows } => Class::Array {
            cols: *cols,
            rows: *rows,
        },
    }
}

fn row(c: &Constant, v: opentune_tsini::Result<TsValue>) -> ValueRow {
    let (value, display) = match v {
        Ok(v) => (Some(v.clone()), Ok(fmt_value(c, &v))),
        Err(e) => (None, Err(e.to_string())),
    };
    ValueRow {
        name: c.name.clone(),
        class: class_of(c),
        units: c.units.clone(),
        page: c.page,
        value,
        display,
        min: c.min.as_ref().and_then(|n| n.lit()),
        max: c.max.as_ref().and_then(|n| n.lit()),
    }
}

/// A table / curve axis of the working tune.
#[derive(Clone, Debug, Default)]
pub(crate) struct Axis {
    pub name: String,
    pub channel: Option<String>,
    pub units: String,
    pub values: Vec<f64>,
    pub error: Option<String>,
}

fn axis(ini: &Ini, tune: &Tune, a: Option<&AxisRef>) -> Option<Axis> {
    let a = a?;
    let mut out = Axis {
        name: a.constant.clone(),
        channel: a.channel.clone(),
        units: ini
            .constant(&a.constant)
            .map(|c| c.units.clone())
            .unwrap_or_default(),
        ..Axis::default()
    };
    match tune.get(ini, &a.constant) {
        Ok(TsValue::Array(v)) => out.values = v,
        Ok(other) => out.error = Some(format!("not an array: {other:?}")),
        Err(e) => out.error = Some(e.to_string()),
    }
    Some(out)
}

/// A `[TableEditor]` table: `rows` (y bins) x `cols` (x bins) cells.
#[derive(Clone, Debug)]
pub(crate) struct TableView {
    pub title: String,
    pub z: String,
    pub units: String,
    pub digits: Option<usize>,
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<Vec<f64>>,
    pub x: Option<Axis>,
    pub y: Option<Axis>,
    pub help: Option<String>,
}

/// A `[CurveEditor]` curve: the x axis and every y line.
#[derive(Clone, Debug)]
pub(crate) struct CurveView {
    pub title: String,
    pub x: Option<Axis>,
    pub ys: Vec<Axis>,
}

/// What loading a `.msq` did.
#[derive(Clone, Debug)]
pub(crate) struct MsqLoad {
    pub signature: Option<String>,
    pub entries: usize,
    pub report: MsqReport,
    pub conditions_added: Vec<String>,
    /// The bytes the tune does not mention kept the ECU's values (else
    /// blank).
    pub base_ecu: bool,
}

/// One page of a write plan.
#[derive(Clone, Debug)]
pub(crate) struct PagePlan {
    /// 1-based.
    pub page: usize,
    /// `(start, end)` byte ranges.
    pub ranges: Vec<(usize, usize)>,
    pub changed: usize,
    pub burnable: bool,
}

/// A tune as `.msq` XML: `(xml, entries, skipped (name, why))`.
pub(crate) type MsqXml = (String, usize, Vec<(String, String)>);

/// What a write of the working tune would change on the ECU.
#[derive(Clone, Debug)]
pub(crate) struct PlanView {
    pub changed_bytes: usize,
    pub pages: Vec<PagePlan>,
    /// `(constant, change)`.
    pub constants: Vec<(String, String)>,
}

impl PlanView {
    pub(crate) fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }
}

/// `(name, change)` rows of a diff.
pub(crate) fn diff_pairs(ini: &Ini, a: &Tune, b: &Tune) -> Vec<(String, String)> {
    diff_rows(ini, a, b)
        .into_iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap_or_default().to_string(),
                r["change"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn options(set: &BTreeSet<String>, unset: &BTreeSet<String>) -> IniOptions {
    let mut o = IniOptions {
        set: set.clone(),
        unset: unset.clone(),
    };
    for u in unset {
        o.set.remove(u);
    }
    o
}

fn meta(comment: &str) -> MsqMeta {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    MsqMeta {
        author: format!("OpenTune {}", env!("CARGO_PKG_VERSION")),
        comment: comment.into(),
        write_date: format!("{secs} (Unix time)"),
        firmware_info: String::new(),
    }
}

/// `tune` as `.msq` XML: `(xml, entries, skipped (name, why))`.
pub(crate) fn tune_xml(ini: &Ini, tune: &Tune, comment: &str) -> MsqXml {
    let (m, skipped) = Msq::from_tune(ini, tune, &meta(comment));
    (m.to_xml(), m.entries().count(), skipped)
}

#[derive(Default)]
pub(crate) struct TsModel {
    pub ini_path: Option<PathBuf>,
    pub set: BTreeSet<String>,
    pub unset: BTreeSet<String>,
    pub ini: Option<Arc<Ini>>,
    /// The tune the panel edits (from a `.msq`, the ECU or blank).
    pub working: Option<Tune>,
    pub working_src: String,
    /// The tune last read from the ECU.
    pub ecu: Option<Tune>,
}

impl TsModel {
    pub(crate) fn ini(&self) -> Result<Arc<Ini>, String> {
        self.ini
            .clone()
            .ok_or_else(|| "load an INI definition first".into())
    }

    fn working(&self) -> Result<&Tune, String> {
        self.working
            .as_ref()
            .ok_or_else(|| "no working tune".into())
    }

    pub(crate) fn info(&self) -> Option<IniInfo> {
        Some(IniInfo::of(self.ini.as_ref()?, self.ini_path.as_ref()?))
    }

    /// Load an INI: the working tune becomes blank and the ECU tune is
    /// forgotten (the caller also drops any connection: its signature check
    /// was against the old INI).
    pub(crate) fn load_ini(
        &mut self,
        path: &Path,
        set: BTreeSet<String>,
        unset: BTreeSet<String>,
    ) -> Result<IniInfo, String> {
        let ini = load_ini(path, &options(&set, &unset))
            .map_err(|e| format!("load INI {}: {e}", path.display()))?;
        let info = IniInfo::of(&ini, path);
        *self = TsModel {
            working: Some(Tune::blank(&ini)),
            working_src: "blank".into(),
            ini: Some(Arc::new(ini)),
            ini_path: Some(path.to_path_buf()),
            set,
            unset,
            ecu: None,
        };
        Ok(info)
    }

    /// Load a `.msq` as the working tune. Its saved settings become INI
    /// conditions (the INI is re-read with them when they add any); bytes
    /// the tune does not mention keep the ECU's values when an ECU tune was
    /// read. Returns whether the INI was re-read (the caller then drops a
    /// connection checked against the old one).
    pub(crate) fn load_msq(&mut self, path: &Path, force: bool) -> Result<MsqLoad, String> {
        let msq = Msq::load(path).map_err(|e| format!("read tune {}: {e}", path.display()))?;
        let mut ini = self.ini()?;
        let extra: BTreeSet<String> = msq
            .settings_conditions()
            .into_iter()
            .filter(|c| !self.unset.contains(c) && !ini.conditions.contains(c))
            .collect();
        let mut added = Vec::new();
        if !extra.is_empty() {
            let ini_path = self.ini_path.clone().ok_or("INI path unknown")?;
            let set: BTreeSet<String> = self.set.union(&extra).cloned().collect();
            let new =
                load_ini(&ini_path, &options(&set, &self.unset)).map_err(|e| e.to_string())?;
            added = extra.into_iter().collect();
            if let Some(t) = &self.ecu {
                if Tune::from_pages(&new, t.pages.clone()).is_err() {
                    self.ecu = None;
                }
            }
            ini = Arc::new(new);
            self.ini = Some(ini.clone());
        }
        let base = self.ecu.clone().unwrap_or_else(|| Tune::blank(&ini));
        let (tune, report) = msq
            .to_tune(
                &ini,
                base,
                MsqLoadOptions {
                    force_signature: force,
                },
            )
            .map_err(|e| e.to_string())?;
        self.working = Some(tune);
        self.working_src = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(MsqLoad {
            signature: msq.signature().map(str::to_string),
            entries: msq.entries().count(),
            report,
            conditions_added: added,
            base_ecu: self.ecu.is_some(),
        })
    }

    /// The working tune as `.msq` XML (for the panel's save).
    pub(crate) fn working_xml(&self) -> Result<MsqXml, String> {
        let ini = self.ini()?;
        Ok(tune_xml(&ini, self.working()?, "saved by OpenTune"))
    }

    /// Diff the working tune against a `.msq` file (file -> working tune).
    pub(crate) fn diff_msq(
        &self,
        path: &Path,
        force: bool,
    ) -> Result<(Vec<(String, String)>, MsqReport), String> {
        let msq = Msq::load(path).map_err(|e| format!("read tune {}: {e}", path.display()))?;
        let ini = self.ini()?;
        let working = self.working()?;
        let (other, rep) = msq
            .to_tune(
                &ini,
                Tune::blank(&ini),
                MsqLoadOptions {
                    force_signature: force,
                },
            )
            .map_err(|e| e.to_string())?;
        Ok((diff_pairs(&ini, &other, working), rep))
    }

    /// Every non-array constant and PC variable of the working tune.
    pub(crate) fn values(&self) -> Result<Vec<ValueRow>, String> {
        let ini = self.ini()?;
        let tune = self.working()?;
        Ok(ini
            .constants
            .iter()
            .chain(&ini.pc_variables)
            .filter(|c| !matches!(c.kind, Kind::Array { .. }))
            .map(|c| row(c, tune.get(&ini, &c.name)))
            .collect())
    }

    /// One constant of the working tune.
    pub(crate) fn get(&self, name: &str) -> Result<ValueRow, String> {
        let ini = self.ini()?;
        let c = ini
            .constant(name)
            .ok_or_else(|| format!("'{name}' is not defined in the INI"))?;
        Ok(row(c, self.working()?.get(&ini, name)))
    }

    /// Set a constant of the working tune (offline). Refuses, never clamps,
    /// a value outside the INI's limits or one the storage cannot hold.
    pub(crate) fn set_value(
        &mut self,
        name: &str,
        value: &serde_json::Value,
    ) -> Result<ValueRow, String> {
        let ini = self.ini()?;
        let c = ini
            .constant(name)
            .ok_or_else(|| format!("'{name}' is not defined in the INI"))?;
        let v = parse_value(c, value)?;
        if let Some(first) = range_issues(c, &v).first() {
            return Err(format!("refused: {first} (the INI's limits)"));
        }
        let tune = self.working.as_mut().ok_or("no working tune")?;
        tune.set(&ini, name, &v)
            .map_err(|e| format!("refused: {e}"))?;
        Ok(row(c, tune.get(&ini, name)))
    }

    /// Set one cell of an array constant (row-major index). The NEW value is
    /// checked against the INI's limits (refused, never clamped); the other
    /// cells are written back unchanged.
    pub(crate) fn set_cell(&mut self, name: &str, index: usize, x: f64) -> Result<(), String> {
        let ini = self.ini()?;
        let c = ini
            .constant(name)
            .ok_or_else(|| format!("'{name}' is not defined in the INI"))?;
        let mut cells = match self.working()?.get(&ini, name).map_err(|e| e.to_string())? {
            TsValue::Array(v) => v,
            _ => return Err(format!("'{name}' is not an array")),
        };
        if let Some(first) = range_issues(c, &TsValue::Scalar(x)).first() {
            return Err(format!("refused: {first} (the INI's limits)"));
        }
        let n = cells.len();
        *cells
            .get_mut(index)
            .ok_or_else(|| format!("cell {index} is outside '{name}' ({n} cells)"))? = x;
        let v = parse_value(c, &serde_json::json!(cells))?;
        let tune = self.working.as_mut().ok_or("no working tune")?;
        tune.set(&ini, name, &v)
            .map_err(|e| format!("refused: {e}"))
    }

    pub(crate) fn table(&self, id: &str) -> Result<TableView, String> {
        let ini = self.ini()?;
        let tune = self.working()?;
        let t = ini.table(id).ok_or_else(|| format!("no table '{id}'"))?;
        let z = t
            .z_bins
            .as_deref()
            .ok_or_else(|| format!("table '{id}' has no zBins"))?;
        let zc = ini
            .constant(z)
            .ok_or_else(|| format!("table '{id}': zBins '{z}' is not defined"))?;
        let Kind::Array { cols, rows } = zc.kind else {
            return Err(format!("table '{id}': '{z}' is not an array"));
        };
        let cells = match tune.get(&ini, z).map_err(|e| e.to_string())? {
            TsValue::Array(v) => v.chunks(cols.max(1)).map(<[f64]>::to_vec).collect(),
            _ => return Err(format!("'{z}' is not an array")),
        };
        Ok(TableView {
            title: t.title.clone(),
            z: z.to_string(),
            units: zc.units.clone(),
            digits: zc.digits_lit(),
            cols,
            rows,
            cells,
            x: axis(&ini, tune, t.x_bins.as_ref()),
            y: axis(&ini, tune, t.y_bins.as_ref()),
            help: t.topic_help.clone(),
        })
    }

    pub(crate) fn curve(&self, id: &str) -> Result<CurveView, String> {
        let ini = self.ini()?;
        let tune = self.working()?;
        let c = ini.curve(id).ok_or_else(|| format!("no curve '{id}'"))?;
        Ok(CurveView {
            title: c.title.clone(),
            x: axis(&ini, tune, c.x_bins.as_ref()),
            ys: c
                .y_bins
                .iter()
                .filter_map(|y| axis(&ini, tune, Some(y)))
                .collect(),
        })
    }

    /// What a write of the working tune would change on the ECU (offline:
    /// against the tune last read from the ECU).
    pub(crate) fn plan(&self) -> Result<PlanView, String> {
        let ini = self.ini()?;
        let ecu = self
            .ecu
            .as_ref()
            .ok_or("read the ECU's tune first (Read from ECU): the plan compares against it")?;
        let working = self.working()?;
        let p = plan_write(ecu, working);
        Ok(PlanView {
            changed_bytes: p.changed_bytes(),
            pages: p
                .pages
                .iter()
                .map(|c| PagePlan {
                    page: c.page + 1,
                    ranges: c.ranges.clone(),
                    changed: c.changed_bytes,
                    burnable: ini.pages.get(c.page).is_some_and(|d| d.burn.is_some()),
                })
                .collect(),
            constants: diff_pairs(&ini, ecu, working),
        })
    }

    /// A tune read from the ECU becomes the ECU tune and the working tune.
    pub(crate) fn set_ecu_tune(&mut self, t: Tune) {
        self.ecu = Some(t.clone());
        self.working = Some(t);
        self.working_src = "ECU".into();
    }
}
