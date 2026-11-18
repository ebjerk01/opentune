//! The clap definitions of the public command tree.
use crate::{candump, haldex_cmd, mapfind_cmd, obd_cmd, recipe_cmd, ts_cmd};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "opentune",
    version,
    about = "Open ECU reading/flashing/calibration"
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// List the interface drivers this build knows about.
    Drivers,
    /// Open a driver and print ECU identity + battery voltage.
    Info(ConnectArgs),
    /// Read a memory region to a file.
    Read {
        #[command(flatten)]
        conn: ConnectArgs,
        /// Start address, hex (e.g. 0x0).
        #[arg(long)]
        addr: String,
        /// Length in bytes, hex or decimal.
        #[arg(long)]
        len: String,
        /// Output file (not needed with --plan).
        #[arg(long, required_unless_present = "plan")]
        out: Option<PathBuf>,
        /// Dry run: print the exact lines the read would write to the adapter
        /// and send nothing. Fails with "driver cannot plan" for a driver
        /// without a dry run.
        #[arg(long)]
        plan: bool,
    },
    /// Byte-diff two images and summarise changed regions.
    Diff { a: PathBuf, b: PathBuf },
    /// Verify or correct checksums in an image.
    Checksum {
        #[command(subcommand)]
        action: ChecksumAction,
    },
    /// Write a region through the safety orchestrator (backup → write → verify),
    /// or with --uds-module program a control module over UDS from a container.
    Flash {
        #[command(flatten)]
        conn: ConnectArgs,
        /// Address to write at (hex).
        #[arg(long, required_unless_present = "container")]
        addr: Option<String>,
        /// File of the exact bytes to write at `addr`.
        #[arg(long, required_unless_present = "container")]
        data: Option<PathBuf>,
        /// Minimum battery mV gate (0 disables).
        #[arg(long, default_value_t = 12_000)]
        min_mv: u32,
        /// Directory to archive the pre-flash backup into (the safety net).
        #[arg(long, default_value = "vault")]
        vault: PathBuf,
        /// Resumable flash: a checkpoint/state file. Its presence enables the
        /// chunked, resume-after-interruption path and writes a session
        /// transcript. Safe to re-run with the same file after an interruption.
        #[arg(long)]
        resume: Option<PathBuf>,
        /// Directory for the session transcript (.log + .jsonl); defaults to the
        /// vault directory. Only used with --resume.
        #[arg(long)]
        transcript: Option<PathBuf>,
        /// Write chunk size in bytes for the resumable path (0 = default).
        #[arg(long, default_value_t = 0)]
        chunk_size: usize,
        #[command(flatten)]
        module: Box<crate::commands::module_flash::ModuleFlashArgs>,
    },
    /// Full read via a driver, archived to the backup vault with ECU id.
    Dump {
        #[command(flatten)]
        conn: ConnectArgs,
        /// Start address (hex), default 0.
        #[arg(long, default_value = "0x0")]
        addr: String,
        /// Length to read (hex or decimal).
        #[arg(long)]
        len: String,
        /// Vault directory to archive the dump into.
        #[arg(long, default_value = "backups")]
        vault: PathBuf,
        /// Dry run: print the exact lines the read would write to the adapter
        /// and send nothing (no identity read, nothing archived).
        #[arg(long)]
        plan: bool,
    },
    /// Find candidate calibration maps: in ONE image (axis/header heuristics,
    /// ranked with evidence), or by clustering the diff of a stock/tuned pair.
    Findmaps {
        /// Image to scan (the stock image in two-file diff mode).
        stock: PathBuf,
        /// Modified image: switches to the two-file diff mode.
        tuned: Option<PathBuf>,
        #[command(flatten)]
        single: mapfind_cmd::SingleArgs,
        /// Merge changed bytes closer than this many bytes into one region.
        #[arg(long, default_value_t = 16)]
        gap: usize,
        /// Minimum region length (bytes) to report.
        #[arg(long, default_value_t = 2)]
        min_len: usize,
        /// Optionally write the candidate regions as JSON.
        #[arg(long)]
        emit: Option<PathBuf>,
    },
    /// Portable tune recipes: create from a stock/tuned pair, apply to a
    /// matching image (checksum-fixed + attested), show.
    Recipe {
        #[command(subcommand)]
        action: recipe_cmd::RecipeAction,
    },
    /// Work with ECU-definition files (list, validate).
    Ecudefs {
        #[command(subcommand)]
        action: EcudefsAction,
    },
    /// Read the connected ECU's identity and match it to a definition/catalog.
    Identify {
        #[command(flatten)]
        conn: ConnectArgs,
        /// Directory of ECU definitions to match against.
        #[arg(long, default_value = "ecudefs")]
        ecudefs: PathBuf,
        /// Optional catalog directory (harvested vehicle→ECU data).
        #[arg(long)]
        catalog: Option<PathBuf>,
    },
    /// Control-unit reference database (ECU / TCU / AWD): list, show, validate.
    Db {
        #[command(subcommand)]
        action: DbAction,
    },
    /// Look up a diagnostic trouble code (e.g. P0101).
    Dtc { code: String },
    /// Decode a 17-character VIN (WMI, country, year, check digit).
    Vin { vin: String },
    /// Structurally parse an ECU part number (VAG or Bosch).
    DecodePart { part: String },
    /// Compare two calibrations map-by-map through an ECU definition.
    Compare {
        /// Directory of ECU definitions.
        #[arg(long, default_value = "ecudefs")]
        ecudefs: PathBuf,
        /// Definition id (see `ecudefs`) whose maps describe the images.
        #[arg(long)]
        def: String,
        /// Original / stock image.
        stock: PathBuf,
        /// Modified / tuned image.
        tuned: PathBuf,
        /// Show the full before/after grid for just this map (by name substring).
        #[arg(long)]
        map: Option<String>,
    },
    /// Import a TunerPro XDF definition and list (or merge) its maps.
    ImportXdf {
        /// Path to the .xdf file (modern XML or legacy text format).
        file: PathBuf,
        /// Optional ECU-definition JSON to merge the imported maps into
        /// (existing maps with the same name are left untouched).
        #[arg(long)]
        into: Option<PathBuf>,
        /// Write the imported maps, constants and per-object report as JSON.
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
        /// Refuse the whole file if any table/function/constant is warned or refused.
        #[arg(long)]
        strict: bool,
    },
    /// Import a FastECU definition (EcuFlash / RomRaider XML with FastECU's
    /// extensions, or a WinOLS MapPack CSV) and list (or merge) its maps.
    ImportFastecu {
        /// Path to the definition (.xml or MapPack .csv).
        file: PathBuf,
        /// `<rom>` to import by xmlid (default: the first non-base rom).
        #[arg(long)]
        rom: Option<String>,
        /// Directory searched (recursively) for included / base definitions by xmlid.
        #[arg(long)]
        base_dir: Option<PathBuf>,
        /// Optional ECU-definition JSON to merge the imported maps into.
        #[arg(long)]
        into: Option<PathBuf>,
        /// Write the imported maps and per-object report as JSON.
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
        /// Refuse the whole file if any table is warned or refused.
        #[arg(long)]
        strict: bool,
    },
    /// Import an EcuFlash / RomRaider XML ROM definition and list (or merge) its maps.
    ImportEcuflash {
        /// Path to the EcuFlash/RomRaider .xml definition.
        file: PathBuf,
        /// Optional ECU-definition JSON to merge the imported maps into
        /// (existing maps with the same name are left untouched).
        #[arg(long)]
        into: Option<PathBuf>,
    },
    /// Import a WinOLS `.ols` project and list (or merge) its map definitions.
    ImportOls {
        /// Path to the `.ols` project file.
        file: PathBuf,
        /// Saved version index to extract as the ROM image (default 0).
        #[arg(long, default_value_t = 0)]
        version: u32,
        /// Optional ECU-definition JSON to merge the imported maps into
        /// (existing maps with the same name are left untouched).
        #[arg(long)]
        into: Option<PathBuf>,
    },
    /// Load definition pack(s) from a directory and report their contents.
    Packs {
        /// A pack directory (contains pack.json), or a root holding several packs.
        #[arg(default_value = "packs")]
        dir: PathBuf,
    },
    /// List archived reads in a backup vault directory.
    Vault {
        /// Vault directory to scan (default: ./vault).
        #[arg(default_value = "vault")]
        dir: PathBuf,
        /// Re-read each .bin and check its CRC32 against the .meta sidecar.
        #[arg(long)]
        verify: bool,
    },
    /// Import an ASAM A2L description and list (or merge) its calibration maps.
    ImportA2l {
        /// Path to the .a2l file.
        file: PathBuf,
        /// Optional ECU-definition JSON to merge the imported maps into
        /// (existing maps with the same name are left untouched).
        #[arg(long)]
        into: Option<PathBuf>,
        /// Base to subtract from each A2L CPU/logical address to get the bin
        /// file offset (hex ok, e.g. 0xA0000000 for TriCore). A2L addresses are
        /// logical, not file offsets, and — unlike XDF — the file carries no
        /// base, so supply the ECU's memory-map base here.
        #[arg(long, default_value = "0")]
        base: String,
    },
    /// Convert an ASAM A2L description straight into a TunerPro XDF.
    ///
    /// Composes the A2L importer and the XDF exporter: it parses the A2L
    /// (lossy UTF-8, like `import-a2l`), rebases every CPU/logical address to a
    /// flash-file offset by subtracting `--base` with **checked** subtraction
    /// (an address below the base is reported, never silently wrapped),
    /// optionally keeps only maps whose name contains `--only`, writes the XDF
    /// to `--out` (or stdout) and always prints a conversion-loss report so a
    /// seemingly complete conversion never hides a silent loss.
    #[command(name = "a2l2xdf")]
    A2l2Xdf {
        /// Path to the input `.a2l` file.
        file: PathBuf,
        /// Output `.xdf` path (defaults to stdout; the loss report always goes
        /// to stderr so it never mixes into a piped XDF).
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
        /// Base to subtract from each A2L CPU/logical address to get the bin
        /// file offset (hex ok, e.g. 0xA0000000 for TriCore). A2L addresses are
        /// logical, not file offsets; supply the ECU's memory-map base here.
        #[arg(long, default_value = "0")]
        base: String,
        /// Keep only maps whose name contains this substring (case-insensitive).
        #[arg(long)]
        only: Option<String>,
    },
    /// Export an ECU definition's maps as a TunerPro XDF (modern XML).
    ExportXdf {
        /// Directory of ECU definitions.
        #[arg(long, default_value = "ecudefs")]
        ecudefs: PathBuf,
        /// Definition id whose maps to export.
        #[arg(long)]
        def: String,
        /// Output .xdf path (defaults to stdout).
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
    },
    /// Export an ECU definition's maps as an ASAM A2L (MCD-2 MC).
    ExportA2l {
        /// Directory of ECU definitions.
        #[arg(long, default_value = "ecudefs")]
        ecudefs: PathBuf,
        /// Definition id whose maps to export.
        #[arg(long)]
        def: String,
        /// Output .a2l path (defaults to stdout).
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
    },
    /// Scan a directory of ECU dumps offline and emit identification JSON.
    Scan {
        /// Directory of dumps (scanned recursively), or a single image file.
        dir: PathBuf,
        /// Print a human-readable recognition summary instead of JSON.
        #[arg(long)]
        human: bool,
        /// Catalog directory.
        #[arg(long, default_value = "catalog")]
        catalog: PathBuf,
        /// ECU-definition directory.
        #[arg(long, default_value = "ecudefs")]
        ecudefs: PathBuf,
    },
    /// Decode a VAG/VW-Group flash container (FRF / ODX / ODX-F / SGO).
    Container {
        #[command(subcommand)]
        action: ContainerAction,
    },
    /// Parse a Mercedes CBF diagnostic-description file (DTCs, services, variants).
    Cbf {
        /// Path to the .cbf file.
        file: PathBuf,
        /// Emit the full parsed model as pretty JSON.
        #[arg(long, conflicts_with = "dtcs")]
        json: bool,
        /// Emit one `code<TAB>description` per line (the flat DTC pool).
        #[arg(long)]
        dtcs: bool,
    },
    /// Module coding / configuration over UDS (read, decode, edit, write).
    ///
    /// Driven by a data-defined coding-label set (`--def`). With the default
    /// `mock` driver everything runs offline against a JSON coding store
    /// (`--store`); no hardware is touched and no on-car behaviour is implied.
    Coding {
        #[command(subcommand)]
        action: CodingAction,
    },
    /// Read adaptation channels (value / routine / actuator) from a module.
    ///
    /// Data-defined by `--def` (an `AdaptationDef`). With the default `mock`
    /// driver it runs offline against a `--store` JSON; no hardware is touched.
    Adaptation {
        /// Adaptation definition (`AdaptationDef`) JSON file.
        #[arg(long)]
        def: PathBuf,
        /// Driver id (see `drivers`); must expose a diagnostic transport.
        #[arg(long, default_value = "mock")]
        driver: String,
        /// For the `mock` driver: JSON UDS store backing the offline transport.
        #[arg(long)]
        store: Option<PathBuf>,
        /// Per-request UDS timeout, milliseconds.
        #[arg(long, default_value_t = 1000)]
        timeout_ms: u64,
        #[command(subcommand)]
        action: AdaptAction,
    },
    /// Scan every module in a vehicle map: presence, identity and stored DTCs.
    ///
    /// Data-defined by `--modules` (a `VehicleModules` map). Read-only. With the
    /// default `mock` driver it runs offline against a `--store` JSON.
    Autoscan {
        /// Vehicle module map (`VehicleModules`) JSON file.
        #[arg(long)]
        modules: PathBuf,
        /// Driver id (see `drivers`); must expose a diagnostic transport.
        #[arg(long, default_value = "mock")]
        driver: String,
        /// For the `mock` driver: JSON UDS store backing the offline transport.
        #[arg(long)]
        store: Option<PathBuf>,
        /// Per-request UDS timeout, milliseconds.
        #[arg(long, default_value_t = 1000)]
        timeout_ms: u64,
    },
    /// Plan or run a service-function script (DPF regen, service reset, …).
    ///
    /// Data-defined by `--script` (a `ServiceScript`). OpenTune ships no OEM
    /// seed/key: a script that needs SecurityAccess requires a caller-supplied
    /// key provider (`--seedkey`, resolved by the embedding build — the public
    /// build knows none), so without one secured scripts can be planned but not
    /// executed. `plan` sends nothing; `run` needs `--ack`.
    Service {
        /// Service-function script (`ServiceScript`) JSON file.
        #[arg(long)]
        script: PathBuf,
        /// Driver id (see `drivers`); must expose a diagnostic transport.
        #[arg(long, default_value = "mock")]
        driver: String,
        /// For the `mock` driver: JSON UDS store backing the offline transport.
        #[arg(long)]
        store: Option<PathBuf>,
        /// Per-request UDS timeout, milliseconds.
        #[arg(long, default_value_t = 1000)]
        timeout_ms: u64,
        /// Seed/key handler id for the script's SecurityAccess steps. Resolved
        /// by the embedding build; the public OpenTune ships none.
        #[arg(long, value_name = "ID")]
        seedkey: Option<String>,
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Poll live data defined by a RomRaider-style `logger.xml` and write CSV.
    ///
    /// Data-defined by `--def`. UDS `0x22` (DID), OBD-II mode `0x01` (PID) and
    /// UDS `0x23` (raw-memory) parameters are driven; parameters with unknown
    /// addressing are reported as undetermined rather than guessed. With the
    /// default `mock` driver it runs offline.
    Log {
        /// RomRaider-style `logger.xml` definition file.
        #[arg(long)]
        def: PathBuf,
        /// Driver id (see `drivers`); must expose a diagnostic transport.
        #[arg(long, default_value = "mock")]
        driver: String,
        /// For the `mock` driver: JSON UDS store backing the offline transport.
        #[arg(long)]
        store: Option<PathBuf>,
        /// Per-request UDS timeout, milliseconds.
        #[arg(long, default_value_t = 1000)]
        timeout_ms: u64,
        /// Number of polling rounds.
        #[arg(long, default_value_t = 1)]
        rounds: usize,
        /// Sleep between rounds, milliseconds (0 = as fast as possible).
        #[arg(long, default_value_t = 0)]
        interval_ms: u64,
        /// Write the CSV here (otherwise to stdout).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Fast logging: pack UDS DID and memory channels into dynamically
        /// defined DIDs (UDS 0x2C) and read each record with one request.
        /// Falls back to per-channel polling (and says why) if the ECU refuses.
        #[arg(long)]
        fast: bool,
        /// First dynamic DID used by --fast (0xF200..=0xF3FF); further records
        /// use the following DIDs.
        #[arg(long, value_name = "DID", default_value = "0xF200")]
        dynamic_did: String,
        /// Largest record per dynamic DID in bytes (--fast); channels beyond it
        /// split across consecutive dynamic DIDs.
        #[arg(long, default_value_t = 32)]
        max_record: usize,
        /// Diagnostic session to enter (and re-enter after a drop) before
        /// defining, e.g. 0x03 (--fast). Default: leave the session alone.
        #[arg(long, value_name = "SESSION")]
        fast_session: Option<String>,
        /// Only poll these parameter ids (default: all enabled).
        params: Vec<String>,
    },
    /// Generic OBD-II mode-01 live data: channel table, supported PIDs, reads
    /// (multi-PID batched); inspection data: readiness, freeze frame, vehicle
    /// info (CALID/CVN), DTCs. `--driver obdsim` runs against a simulated ECU.
    Obd {
        #[command(subcommand)]
        action: obd_cmd::ObdAction,
    },
    /// Capture every frame on a CAN bus to a log file (candump / CSV / ASC),
    /// listen-only where the adapter allows. Runs until `--duration` or Ctrl-C.
    /// With `--driver mock` it records synthetic traffic offline.
    Candump(candump::CandumpArgs),
    /// TunerStudio-compatible aftermarket ECUs (Speeduino, rusEFI,
    /// MegaSquirt): inspect INI definitions and .msq tunes, read / write
    /// (verified, burn on --confirm) / log over serial. `--port sim` uses a
    /// simulated ECU.
    Ts {
        #[command(subcommand)]
        action: ts_cmd::TsAction,
    },
    /// Run the in-memory UDS conformance server offline and exchange raw
    /// requests against it — no hardware, for testing and demos.
    Simulate {
        /// Seed a data identifier, e.g. `--did F190=0102` (repeatable).
        #[arg(long = "did", value_name = "ID=HEX")]
        dids: Vec<String>,
        /// Require a SecurityAccess unlock before `0x2E` writes are accepted.
        #[arg(long)]
        require_security_write: bool,
        /// One or more UDS requests as hex (e.g. `22F190`), handled in order.
        #[arg(required = true, value_name = "HEX")]
        requests: Vec<String>,
    },
    /// BorgWarner Haldex AWD: decode Haldex CAN frames in a log, read a Gen4
    /// controller (read-only), OpenHaldex status and mode.
    Haldex {
        #[command(subcommand)]
        action: haldex_cmd::HaldexAction,
    },
}

#[derive(Subcommand)]
pub enum AdaptAction {
    /// List the channels declared in the definition (no I/O).
    List,
    /// Read a value channel's current reading (0x22).
    Read {
        /// Channel name (as in the definition).
        channel: String,
    },
}

#[derive(Subcommand)]
pub enum ServiceAction {
    /// Validate the script and print its plan without sending anything.
    Plan,
    /// Execute the script. Requires `--ack`; refuses secured scripts unless a
    /// key provider is given with `--seedkey`.
    Run {
        /// Acknowledge that this sends real requests to the module.
        #[arg(long)]
        ack: bool,
    },
}

#[derive(Subcommand)]
pub enum CodingAction {
    /// Read every coding block and print its raw bytes (DID + hex).
    Read {
        #[command(flatten)]
        conn: CodingConn,
    },
    /// Read and decode every parameter, printing value + label.
    Decode {
        #[command(flatten)]
        conn: CodingConn,
    },
    /// Preview applying `NAME = VALUE`: print the byte/parameter changes a write
    /// would make, without writing anything (a dry run).
    Set {
        /// Parameter name (as in the definition).
        name: String,
        /// New value: a number (decimal or `0x` hex) or an enum option label.
        value: String,
        #[command(flatten)]
        conn: CodingConn,
    },
    /// Apply `NAME = VALUE` to the module: re-reads fresh, merges the single
    /// changed field onto the live bytes, writes it and reads back to verify.
    Write {
        /// Parameter name (as in the definition).
        name: String,
        /// New value: a number (decimal or `0x` hex) or an enum option label.
        value: String,
        /// Produce the plan (backup + diff) without writing anything.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        conn: CodingConn,
    },
}

#[derive(clap::Args)]
pub struct CodingConn {
    /// Coding definition (`CodingDef`) JSON file.
    #[arg(long)]
    pub def: PathBuf,
    /// Driver id (see `drivers`); must expose a diagnostic transport.
    #[arg(long, default_value = "mock")]
    pub driver: String,
    /// For the `mock` driver: JSON coding store (`{ "<did>": "<hex>" }`) that
    /// backs the offline UDS transport. Writes persist back to this file.
    #[arg(long)]
    pub store: Option<PathBuf>,
    /// Per-request UDS timeout, milliseconds.
    #[arg(long, default_value_t = 1000)]
    pub timeout_ms: u64,
}

#[derive(Subcommand)]
pub enum ContainerAction {
    /// Decode a container, auto-detecting FRF / ZIP / ODX / ODX-F / SGO.
    ///
    /// All key material is CALLER-SUPPLIED via the flags below; this tool ships
    /// no keys. A bare, already-plaintext ODX/ZIP/SGO needs no keys at all.
    Unpack {
        /// Input container file (format is auto-detected from its contents).
        #[arg(long = "in")]
        input: PathBuf,
        /// Write one file per decoded segment into this directory.
        #[arg(long)]
        outdir: Option<PathBuf>,
        /// FRF rolling-XOR key material (`frf.key`); required only for an FRF.
        #[arg(long)]
        frf_key: Option<PathBuf>,
        /// AES-128-CBC key for ODX/ODX-F flash blocks, 16 bytes as 32 hex chars.
        #[arg(long)]
        aes_key: Option<String>,
        /// AES-128-CBC IV, 16 bytes as 32 hex chars (requires --aes-key;
        /// defaults to an all-zero IV when omitted).
        #[arg(long)]
        aes_iv: Option<String>,
        /// Repeating-key XOR string (ZF-8HP method 0x22). Mutually exclusive
        /// with --aes-key.
        #[arg(long, conflicts_with = "aes_key")]
        xor_key: Option<String>,
        /// 256-byte SGO format-1 family cipher table (binary file, exactly 256 bytes).
        #[arg(long)]
        sgo_table: Option<PathBuf>,
        /// Which SGO format-1 cipher convention to use.
        #[arg(long, value_enum, default_value_t = SgoCipherArg::Progressive)]
        sgo_cipher: SgoCipherArg,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum SgoCipherArg {
    /// Later PQ-family "progressive" cipher (rolling counter + feedback).
    Progressive,
    /// Early 02EA "feedback" cipher (previous-stored-byte feedback).
    Feedback,
}

#[derive(Subcommand)]
pub enum DbAction {
    /// List units, optionally filtered by kind (ecu/tcu/awd) and a search query.
    List {
        #[arg(long)]
        kind: Option<String>,
        /// Free-text query (name, family, maker, part number, vehicle).
        q: Option<String>,
        /// Database directory.
        #[arg(long, default_value = "database")]
        dir: PathBuf,
    },
    /// Print one unit as stored (JSON).
    Show {
        id: String,
        #[arg(long, default_value = "database")]
        dir: PathBuf,
    },
    /// Validate every unit (schema, unique ids, checksum/ecudef refs, pinout
    /// honesty rules). Exits non-zero on any issue.
    Validate {
        #[arg(long, default_value = "database")]
        dir: PathBuf,
        #[arg(long, default_value = "ecudefs")]
        ecudefs: PathBuf,
    },
}

#[derive(Subcommand)]
pub enum EcudefsAction {
    /// List the ECU definitions in a directory.
    List {
        /// Directory of *.json ECU definitions.
        #[arg(default_value = "ecudefs")]
        dir: PathBuf,
    },
    /// Validate one definition file or every *.json in a directory against the
    /// format, honesty and scope rules (see docs/CONTRIBUTING-ECUDEFS.md).
    /// Exits non-zero if any file has a validation error.
    Validate {
        /// A single ECU-definition *.json file, or a directory of them.
        #[arg(default_value = "ecudefs")]
        path: PathBuf,
        /// Optionally decode each definition's maps against this real dump and
        /// check that addresses/axes land inside the image.
        #[arg(long)]
        image: Option<PathBuf>,
        /// Also print warnings (off by default; only errors show otherwise).
        #[arg(long)]
        warnings: bool,
    },
}

#[derive(Subcommand)]
pub enum ChecksumAction {
    /// Report each protected block and whether it matches.
    Verify {
        /// Algorithm id (e.g. crc32-tail, edc16).
        #[arg(long)]
        algo: String,
        file: PathBuf,
    },
    /// Correct checksums in place (writes the file).
    Fix {
        #[arg(long)]
        algo: String,
        file: PathBuf,
        /// Known-good reference image (your own untouched stock dump) used to
        /// recover per-block constants the library doesn't ship — needed for
        /// EDC17 (VAG constants are firmware-specific); ignored by self-contained
        /// algorithms (edc16/pcr21/edc15c).
        #[arg(long)]
        reference: Option<PathBuf>,
        /// Fix only the blocks whose checksum is currently BAD, leaving valid
        /// blocks byte-for-byte untouched. (Ignored alongside --reference.)
        #[arg(long)]
        only_bad: bool,
    },
    /// List the available checksum algorithms.
    Algos,
    /// Verify an image and emit a flash **checksum attestation** bound to its
    /// bytes. `satisfied` reflects the real per-block result, so the flash
    /// policy refuses an internally inconsistent image. Does not modify the file.
    Attest {
        /// Algorithm id (e.g. edc16, pcr21).
        #[arg(long)]
        algo: String,
        /// Full ECU image to verify and attest.
        file: PathBuf,
        /// Write the attestation JSON here (otherwise printed to stdout).
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

/// The driver options shared by `info`, `read`, `flash`, `dump` and
/// `identify`. An extra command can flatten it into its own arguments and open
/// the driver with [`CliContext::open_connect`](crate::CliContext::open_connect);
/// [`ConnectArgs::config`] is the `ConnectConfig` it opens with.
#[derive(clap::Args, Debug, Clone)]
pub struct ConnectArgs {
    /// Driver id (see `drivers`).
    #[arg(long, default_value = "mock")]
    pub driver: String,
    /// For the mock driver: the image file to use as the "ECU".
    #[arg(long)]
    pub file: Option<PathBuf>,
    /// Base address mapped to file offset 0 (hex).
    #[arg(long, default_value = "0x0")]
    pub base: String,
    /// Seed/key handler id for the driver's security-access unlock. Resolved
    /// by the embedding build; the public OpenTune ships none.
    #[arg(long, value_name = "ID")]
    pub seedkey: Option<String>,
    /// Adapter port: serial device (e.g. /dev/ttyUSB0), CAN interface or
    /// host:port, as the driver expects (driver option `port`).
    #[arg(long, value_name = "PORT")]
    pub port: Option<String>,
    /// Serial baud of the adapter link (driver option `baud`).
    #[arg(long, value_name = "BAUD")]
    pub baud: Option<String>,
    /// Vehicle bus: `hs` or `ms` for the ELM327 drivers, or a driver-specific
    /// value (driver option `bus`).
    #[arg(long, value_name = "BUS")]
    pub bus: Option<String>,
    /// Diagnostic request CAN id, hex (default 7E0). Give --rx-id too.
    #[arg(long, value_name = "HEX")]
    pub tx_id: Option<String>,
    /// Diagnostic response CAN id, hex (default 7E8). Give --tx-id too.
    #[arg(long, value_name = "HEX")]
    pub rx_id: Option<String>,
    /// Extra driver option, e.g. `--opt read_mode=upload --opt timeout_ms=5000`
    /// (repeatable; see docs/DRIVERS.md for each driver's options).
    #[arg(long = "opt", value_name = "KEY=VALUE")]
    pub opts: Vec<String>,
}

/// Drivers that program only 11-bit CAN headers: a 29-bit id would be
/// truncated to a different module, so it is refused instead.
const ELEVEN_BIT_ONLY: &[&str] = &["elm327obd"];

/// Option keys a dedicated flag owns; `--opt` refuses them.
const FLAG_KEYS: &[(&str, &str)] = &[
    ("file", "--file"),
    ("base", "--base"),
    ("port", "--port"),
    ("baud", "--baud"),
    ("bus", "--bus"),
    ("tx_id", "--tx-id"),
    ("rx_id", "--rx-id"),
];

/// A non-blank flag value, trimmed (a blank value counts as not given).
fn given(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// A CAN id: hex with or without `0x` (`7E0`, `0x7E0`), at most 29 bits.
fn parse_can_id(flag: &str, s: &str) -> anyhow::Result<u32> {
    let h = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    let v = u32::from_str_radix(h, 16)
        .map_err(|_| anyhow::anyhow!("{flag} '{s}': not a hex CAN id (e.g. 7E0)"))?;
    if v > 0x1FFF_FFFF {
        anyhow::bail!("{flag} '{s}': larger than a 29-bit CAN id");
    }
    Ok(v)
}

impl ConnectArgs {
    /// The `ConnectConfig` these flags describe, mapped like the desktop app's
    /// adapter settings: `--tx-id`/`--rx-id` (both or neither; default
    /// 7E0/7E8) set the request/response ids, `--port`/`--baud`/`--bus` and
    /// every `--opt KEY=VALUE` go into `options`, and `--file`/`--base` keep
    /// feeding the mock driver. Bad values are refused before any driver is
    /// made: a malformed hex id or `--opt`, a non-numeric baud, a 29-bit id on
    /// an 11-bit-only driver, or an option given twice.
    pub fn config(&self) -> anyhow::Result<opentune_core::ConnectConfig> {
        let mut cfg = opentune_core::ConnectConfig::default();
        match (given(&self.tx_id), given(&self.rx_id)) {
            (Some(t), Some(r)) => {
                cfg.tx_id = parse_can_id("--tx-id", t)?;
                cfg.rx_id = parse_can_id("--rx-id", r)?;
            }
            (None, None) => {}
            _ => anyhow::bail!(
                "set both --tx-id and --rx-id (or neither for the engine ECU, 7E0/7E8)"
            ),
        }
        if ELEVEN_BIT_ONLY.contains(&self.driver.as_str())
            && (cfg.tx_id > 0x7FF || cfg.rx_id > 0x7FF)
        {
            anyhow::bail!(
                "the {} driver supports 11-bit CAN ids only (got {:X}/{:X})",
                self.driver,
                cfg.tx_id,
                cfg.rx_id
            );
        }
        for raw in &self.opts {
            let (k, v) = raw
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("--opt '{raw}': expected KEY=VALUE"))?;
            let k = k.trim();
            if k.is_empty() {
                anyhow::bail!("--opt '{raw}': empty option name");
            }
            if let Some((_, flag)) = FLAG_KEYS.iter().find(|(key, _)| *key == k) {
                anyhow::bail!("--opt '{raw}': use {flag} for '{k}'");
            }
            if cfg
                .options
                .insert(k.to_owned(), v.trim().to_owned())
                .is_some()
            {
                anyhow::bail!("--opt: option '{k}' given more than once");
            }
        }
        if let Some(f) = &self.file {
            cfg.options
                .insert("file".into(), f.to_string_lossy().into_owned());
        }
        cfg.options.insert("base".into(), self.base.clone());
        if let Some(b) = given(&self.baud) {
            if !b.parse::<u32>().is_ok_and(|n| n > 0) {
                anyhow::bail!("--baud '{b}': expected a positive whole number (e.g. 38400)");
            }
        }
        for (key, v) in [
            ("port", &self.port),
            ("baud", &self.baud),
            ("bus", &self.bus),
        ] {
            if let Some(v) = given(v) {
                cfg.options.insert(key.into(), v.to_owned());
            }
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod connect_tests {
    use super::ConnectArgs;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[command(flatten)]
        conn: ConnectArgs,
    }

    fn parse(args: &[&str]) -> ConnectArgs {
        T::try_parse_from(std::iter::once("t").chain(args.iter().copied()))
            .expect("parse")
            .conn
    }

    fn err(args: &[&str]) -> String {
        format!("{:#}", parse(args).config().expect_err("expected an error"))
    }

    #[test]
    fn defaults_are_unchanged() {
        let cfg = parse(&[]).config().unwrap();
        let def = opentune_core::ConnectConfig::default();
        assert_eq!((cfg.tx_id, cfg.rx_id), (def.tx_id, def.rx_id));
        assert_eq!(cfg.bitrate, def.bitrate);
        let keys: Vec<&str> = cfg.options.keys().map(String::as_str).collect();
        assert_eq!(keys, ["base"], "only the mock's base by default");
        assert_eq!(cfg.options["base"], "0x0");

        let cfg = parse(&["--file", "img.bin", "--base", "0x8000"])
            .config()
            .unwrap();
        assert_eq!(cfg.options["file"], "img.bin");
        assert_eq!(cfg.options["base"], "0x8000");
    }

    #[test]
    fn adapter_flags_map_into_the_config() {
        let cfg = parse(&[
            "--driver",
            "elm327obd",
            "--port",
            "/dev/ttyUSB1",
            "--baud",
            "115200",
            "--bus",
            "ms",
            "--tx-id",
            "726",
            "--rx-id",
            "0x72E",
            "--opt",
            "read_mode=upload",
            "--opt",
            "timeout_ms = 5000",
            "--opt",
            "alfid=",
        ])
        .config()
        .unwrap();
        assert_eq!((cfg.tx_id, cfg.rx_id), (0x726, 0x72E));
        let o = &cfg.options;
        assert_eq!(o["port"], "/dev/ttyUSB1");
        assert_eq!(o["baud"], "115200");
        assert_eq!(o["bus"], "ms");
        assert_eq!(o["read_mode"], "upload");
        assert_eq!(o["timeout_ms"], "5000");
        assert_eq!(o["alfid"], "", "an empty value is passed through");
        // A value may itself contain '='.
        let cfg = parse(&["--opt", "x=a=b"]).config().unwrap();
        assert_eq!(cfg.options["x"], "a=b");
    }

    #[test]
    fn twenty_nine_bit_ids_only_where_the_driver_supports_them() {
        let ext = ["--tx-id", "18DA10F1", "--rx-id", "18DAF110"];
        let mut args = vec!["--driver", "elm327"];
        args.extend(ext);
        let cfg = parse(&args).config().unwrap();
        assert_eq!((cfg.tx_id, cfg.rx_id), (0x18DA_10F1, 0x18DA_F110));
        let mut args = vec!["--driver", "elm327obd"];
        args.extend(ext);
        let e = err(&args);
        assert!(e.contains("11-bit CAN ids only"), "{e}");
    }

    #[test]
    fn bad_values_are_refused_clearly() {
        for (args, want) in [
            (
                &["--tx-id", "7G0", "--rx-id", "7E8"][..],
                "--tx-id '7G0': not a hex CAN id",
            ),
            (
                &["--tx-id", "7E0", "--rx-id", "zz"][..],
                "--rx-id 'zz': not a hex CAN id",
            ),
            (
                &["--tx-id", "20000000", "--rx-id", "7E8"][..],
                "larger than a 29-bit",
            ),
            (&["--tx-id", "7E0"][..], "set both --tx-id and --rx-id"),
            (&["--rx-id", "7E8"][..], "set both --tx-id and --rx-id"),
            (&["--opt", "read_mode"][..], "expected KEY=VALUE"),
            (&["--opt", "=1"][..], "empty option name"),
            (&["--opt", "port=/dev/x"][..], "use --port"),
            (&["--opt", "file=x"][..], "use --file"),
            (
                &["--opt", "a=1", "--opt", "a=2"][..],
                "given more than once",
            ),
            (&["--baud", "fast"][..], "--baud 'fast'"),
            (&["--baud", "0"][..], "--baud '0'"),
        ] {
            let e = err(args);
            assert!(e.contains(want), "{args:?}: {e}");
        }
    }
}
