//! TunerStudio-compatible ECU definitions and tunes for the open aftermarket
//! ECUs (Speeduino, rusEFI, MegaSquirt).
//!
//! - [`parse_ini`] / [`load_ini`] read an INI ECU definition (the format the
//!   open firmware projects publish their `.ini` files in) into an [`Ini`]:
//!   pages and their commands, constants, PC variables, realtime output
//!   channels, table/curve editors, menus, setting groups. `#if`/`#elif`/
//!   `#else`/`#endif` follow the conditions in [`IniOptions`] and the file's own
//!   `#set`/`#unset`. Nothing is fatal: lines that cannot be used become
//!   [`Warning`]s.
//! - [`Tune`] holds the page bytes of a tune and maps constants by name to and
//!   from user values ([`value`]).
//! - [`msq`] reads and writes the `.msq` XML tune format and converts it to and
//!   from a [`Tune`] through a definition, refusing a signature mismatch
//!   unless forced.
//! - [`channels`] decodes the realtime output-channel block, including simple
//!   `{ expression }` channels ([`expr`]); anything the evaluator does not
//!   support is reported per channel, not guessed.
//! - [`present`] holds a tune editor's presentation helpers: values as text,
//!   per-constant diffs, UI value parsing and JSON summaries.
//!
//! The serial protocol that moves pages and realtime data lives in
//! `opentune-tsproto`.
use thiserror::Error;

pub mod channels;
pub mod command;
pub mod expr;
pub mod model;
pub mod msq;
mod parse;
pub mod present;
pub mod text;
pub mod tune;
pub mod value;

pub use channels::{decode_channels, ChannelSample};
pub use command::{CmdPart, CommandTemplate, Matched, RenderCtx};
pub use expr::{Expr, ExprError};
pub use model::*;
pub use msq::{Msq, MsqEntry, MsqLoadOptions, MsqMeta, MsqPage, MsqReport};
pub use parse::IniOptions;
pub use tune::Tune;
pub use value::{Scaling, Value};

/// Errors from loading files or converting values.
#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("XML: {0}")]
    Xml(String),
    #[error(transparent)]
    Expr(#[from] ExprError),
    #[error("{0}")]
    Value(String),
    #[error("'{0}' is not defined in the INI")]
    Unknown(String),
    #[error(
        "tune signature {msq:?} does not match the INI signature {ini:?}; \
         load the INI that matches the firmware, or force it if you are sure"
    )]
    SignatureMismatch { msq: String, ini: String },
    #[error("{0}")]
    Format(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Parse a definition from text. Never fails; see [`Ini::warnings`].
pub fn parse_ini(text: &str, opts: &IniOptions) -> Ini {
    parse::parse(text, opts)
}

/// Read and parse a definition file (UTF-8 or ISO-8859-1). Fails only when
/// the file cannot be read or contains no section at all.
pub fn load_ini(path: &std::path::Path, opts: &IniOptions) -> Result<Ini> {
    let bytes = std::fs::read(path)?;
    let ini = parse_ini(&text::decode(&bytes), opts);
    if ini.sections.is_empty() {
        return Err(Error::Format(format!(
            "{}: no [Section] headers; not a TunerStudio INI definition",
            path.display()
        )));
    }
    Ok(ini)
}

/// Normalise a signature for comparison: trailing NULs and whitespace dropped.
pub fn normalize_signature(s: &str) -> &str {
    s.trim_end_matches(['\0', ' ', '\r', '\n']).trim_start()
}
