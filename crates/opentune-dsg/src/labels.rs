//! The hook for measuring-group meanings. They are data, not code: each
//! `kwp_measuring_block` entry in a controller definition's `live[]` names one
//! `(group, cell)`. `ctrldefs/vag-dq250-02e.json` has none yet — the owner
//! parks the DSG group meanings until a real capture exists — so every cell
//! reads as unconfirmed until entries are added there (or in a user's own
//! definition passed to the CLI).
use opentune_ctrldef::{ControllerDef, LiveSource};
use serde::Serialize;
use std::collections::BTreeMap;

/// What a definition says a cell is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CellMeaning {
    /// Channel name from the definition.
    pub name: String,
    /// Unit the definition gives (may differ from the formula's unit).
    pub unit: String,
    /// The definition's notes (source, confidence).
    pub notes: String,
    /// Where the meaning came from (definition id).
    pub source: String,
}

/// Meanings by `(group, 0-based cell)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupLabels {
    by_cell: BTreeMap<(u8, u8), CellMeaning>,
}

impl GroupLabels {
    /// Every `kwp_measuring_block` live channel of `def`.
    pub fn from_ctrldef(def: &ControllerDef) -> Self {
        let mut l = Self::default();
        for ch in &def.live {
            if let LiveSource::KwpMeasuringBlock { group, cell } = ch.source {
                l.insert(
                    group,
                    cell,
                    CellMeaning {
                        name: ch.name.clone(),
                        unit: ch.unit.clone(),
                        notes: ch.notes.clone(),
                        source: def.id.clone(),
                    },
                );
            }
        }
        l
    }

    pub fn insert(&mut self, group: u8, cell: u8, meaning: CellMeaning) {
        self.by_cell.insert((group, cell), meaning);
    }

    pub fn get(&self, group: u8, cell: u8) -> Option<&CellMeaning> {
        self.by_cell.get(&(group, cell))
    }

    pub fn len(&self) -> usize {
        self.by_cell.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_cell.is_empty()
    }
}
