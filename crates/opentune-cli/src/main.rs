//! OpenTune command-line interface.
//!
//! Wires the available interface drivers into a registry and exposes the
//! offline-capable operations (identify, read, diff, checksum) plus a
//! safety-gated flash. With the `mock` driver everything works on local files.
use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use opentune_container::{BlockCipher, Keys, SgoCipher};
use opentune_core::{ConnectConfig, Interface, MemoryRegion, Progress, Registry};
use std::path::PathBuf;

mod candump;
mod obd_cmd;

#[derive(Parser)]
#[command(
    name = "opentune",
    version,
    about = "Open ECU reading/flashing/calibration"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
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
        /// Output file.
        #[arg(long)]
        out: PathBuf,
    },
    /// Byte-diff two images and summarise changed regions.
    Diff { a: PathBuf, b: PathBuf },
    /// Verify or correct checksums in an image.
    Checksum {
        #[command(subcommand)]
        action: ChecksumAction,
    },
    /// Write a region through the safety orchestrator (backup → write → verify).
    Flash {
        #[command(flatten)]
        conn: ConnectArgs,
        /// Address to write at (hex).
        #[arg(long)]
        addr: String,
        /// File of the exact bytes to write at `addr`.
        #[arg(long)]
        data: PathBuf,
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
    },
    /// Find candidate calibration maps by clustering the diff of two images.
    Findmaps {
        /// Stock image.
        stock: PathBuf,
        /// Modified image.
        tuned: PathBuf,
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
    /// key provider, which the CLI does not yet pass — so secured scripts can be
    /// planned but not executed here. `plan` sends nothing; `run` needs `--ack`.
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
}

#[derive(Subcommand)]
enum AdaptAction {
    /// List the channels declared in the definition (no I/O).
    List,
    /// Read a value channel's current reading (0x22).
    Read {
        /// Channel name (as in the definition).
        channel: String,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Validate the script and print its plan without sending anything.
    Plan,
    /// Execute the script. Requires `--ack`; refuses secured scripts (no key
    /// provider is wired in the CLI yet).
    Run {
        /// Acknowledge that this sends real requests to the module.
        #[arg(long)]
        ack: bool,
    },
}

#[derive(Subcommand)]
enum CodingAction {
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
struct CodingConn {
    /// Coding definition (`CodingDef`) JSON file.
    #[arg(long)]
    def: PathBuf,
    /// Driver id (see `drivers`); must expose a diagnostic transport.
    #[arg(long, default_value = "mock")]
    driver: String,
    /// For the `mock` driver: JSON coding store (`{ "<did>": "<hex>" }`) that
    /// backs the offline UDS transport. Writes persist back to this file.
    #[arg(long)]
    store: Option<PathBuf>,
    /// Per-request UDS timeout, milliseconds.
    #[arg(long, default_value_t = 1000)]
    timeout_ms: u64,
}

#[derive(Subcommand)]
enum ContainerAction {
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
enum SgoCipherArg {
    /// Later PQ-family "progressive" cipher (rolling counter + feedback).
    Progressive,
    /// Early 02EA "feedback" cipher (previous-stored-byte feedback).
    Feedback,
}

#[derive(Subcommand)]
enum DbAction {
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
enum EcudefsAction {
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
enum ChecksumAction {
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

#[derive(clap::Args)]
struct ConnectArgs {
    /// Driver id (see `drivers`).
    #[arg(long, default_value = "mock")]
    driver: String,
    /// For the mock driver: the image file to use as the "ECU".
    #[arg(long)]
    file: Option<PathBuf>,
    /// Base address mapped to file offset 0 (hex).
    #[arg(long, default_value = "0x0")]
    base: String,
}

fn registry() -> Registry {
    let mut r = Registry::new();
    r.register("mock", "Mock / file driver", || {
        Box::new(opentune_driver_mock::MockDriver::new())
    });
    r.register("openport2", "OpenPort 2.0 (J2534/CDC-ACM)", || {
        Box::new(opentune_driver_openport2::OpenPort2Driver::new())
    });
    r.register("socketcan", "SocketCAN", || {
        Box::new(opentune_driver_socketcan::SocketCanDriver::new())
    });
    r.register("elm327", "K-line (ELM327)", || {
        Box::new(opentune_driver_elm327::Elm327Driver::new())
    });
    r.register("kkl", "K-line (KKL 409.1 raw, FTDI/CH340)", || {
        Box::new(opentune_driver_kkl::KklDriver::new())
    });
    r.register("kline-bridge", "Arduino K-line bridge", || {
        Box::new(opentune_driver_kline_bridge::BridgeDriver::new())
    });
    r.register("slcan", "slcan USB-CAN (CANable/CANtact)", || {
        Box::new(opentune_driver_slcan::SlcanDriver::new())
    });
    r.register("j2534", "J2534 PassThru (generic, Windows)", || {
        Box::new(opentune_driver_j2534::J2534Driver::new())
    });
    r.register("gsusb", "gs_usb / candleLight USB-CAN", || {
        Box::new(opentune_driver_gsusb::GsUsbDriver::new())
    });
    r.register("elm327net", "ELM327 over WiFi / Bluetooth", || {
        Box::new(opentune_driver_elm327net::Elm327NetDriver::new())
    });
    r.register("elm327obd", "ELM327 serial (OBD read diagnostics)", || {
        Box::new(opentune_driver_elm327_obd::Elm327Obd::new())
    });
    r.register(
        "vcar",
        "Virtual car (offline multi-module simulator)",
        || Box::new(opentune_driver_vcar::VcarDriver::new()),
    );
    r
}

fn parse_num(s: &str) -> Result<u32> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x") {
        u32::from_str_radix(hex, 16).with_context(|| format!("bad hex: {s}"))
    } else {
        s.parse::<u32>().with_context(|| format!("bad number: {s}"))
    }
}

/// Outcome of rebasing an A2L import's CPU/logical addresses to flash-file
/// offsets. Both lists are empty when every address sat at or above the base.
#[derive(Debug, Default)]
struct RebaseReport {
    /// Maps whose own data address was below `base` and so could not be located
    /// in the image; each was dropped from the map list. Formatted for display.
    dropped: Vec<String>,
    /// Stored axis addresses that were below `base`. The map was kept but the
    /// offending axis address was cleared (so no wrong byte is ever read).
    axis_failures: Vec<String>,
}

/// Rebase every map (and its stored axes) in place from A2L CPU/logical
/// addresses down to flash-file offsets by subtracting `base`, using **checked**
/// subtraction so an address below `base` is reported rather than silently
/// wrapped. A map whose own data address underflows is removed from `maps`; an
/// axis address that underflows is cleared and the map kept. This is the single
/// rebasing path shared by `import-a2l` and `a2l2xdf` so both behave identically.
fn rebase_maps_checked(maps: &mut Vec<opentune_core::ecudef::MapDef>, base: u32) -> RebaseReport {
    fn rebase_axis(
        ax: Option<&mut opentune_core::ecudef::AxisDef>,
        base: u32,
        map_name: &str,
        which: &str,
        failures: &mut Vec<String>,
    ) {
        if let Some(ax) = ax {
            if let Some(a) = ax.address {
                match a.checked_sub(base) {
                    Some(v) => ax.address = Some(v),
                    None => {
                        ax.address = None;
                        failures.push(format!(
                            "{map_name} {which}-axis @0x{a:X} < base 0x{base:X}"
                        ));
                    }
                }
            }
        }
    }

    let mut report = RebaseReport::default();
    let mut kept = Vec::with_capacity(maps.len());
    for mut m in std::mem::take(maps) {
        match m.address.checked_sub(base) {
            Some(a) => m.address = a,
            None => {
                report
                    .dropped
                    .push(format!("{} @0x{:X} < base 0x{:X}", m.name, m.address, base));
                continue;
            }
        }
        rebase_axis(
            m.x_axis.as_mut(),
            base,
            &m.name,
            "x",
            &mut report.axis_failures,
        );
        rebase_axis(
            m.y_axis.as_mut(),
            base,
            &m.name,
            "y",
            &mut report.axis_failures,
        );
        kept.push(m);
    }
    *maps = kept;
    report
}

fn open_driver(reg: &Registry, conn: &ConnectArgs) -> Result<Box<dyn Interface>> {
    let mut drv = reg
        .make(&conn.driver)
        .ok_or_else(|| anyhow!("unknown driver '{}'", conn.driver))?;
    let mut cfg = ConnectConfig::default();
    if let Some(f) = &conn.file {
        cfg.options
            .insert("file".into(), f.to_string_lossy().into_owned());
    }
    cfg.options.insert("base".into(), conn.base.clone());
    drv.open(&cfg)
        .map_err(|e| anyhow!("open {}: {e}", conn.driver))?;
    Ok(drv)
}

fn noprogress(_: Progress) {}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let reg = registry();

    match cli.cmd {
        Command::Drivers => {
            println!("Available drivers:");
            for e in reg.list() {
                println!("  {:<12} {}", e.id, e.display_name);
            }
        }
        Command::Info(conn) => {
            let mut drv = open_driver(&reg, &conn)?;
            let info = drv.info();
            println!("Interface : {} ({})", info.display_name, info.id);
            if let Some(mv) = drv.battery_millivolts()? {
                println!("Battery   : {:.2} V", mv as f64 / 1000.0);
            }
            let id = drv.read_ecu_id()?;
            println!("ECU       : {}", id.ecu_name.unwrap_or_else(|| "?".into()));
            if let Some(sw) = id.sw_number {
                println!("SW        : {sw}");
            }
            if let Some(hw) = id.hw_number {
                println!("HW        : {hw}");
            }
            if let Some(vin) = id.vin {
                println!("VIN       : {vin}");
            }
            drv.close()?;
        }
        Command::Read {
            conn,
            addr,
            len,
            out,
        } => {
            let mut drv = open_driver(&reg, &conn)?;
            let region = MemoryRegion::new(parse_num(&addr)?, parse_num(&len)?);
            let data = drv.read_memory(region, &mut noprogress)?;
            std::fs::write(&out, &data)?;
            println!(
                "Read {} bytes from 0x{:08X} -> {}",
                data.len(),
                region.address,
                out.display()
            );
            drv.close()?;
        }
        Command::Diff { a, b } => {
            let da = std::fs::read(&a).with_context(|| format!("read {}", a.display()))?;
            let db = std::fs::read(&b).with_context(|| format!("read {}", b.display()))?;
            diff(&da, &db);
        }
        Command::Checksum { action } => match action {
            ChecksumAction::Verify { algo, file } => {
                use opentune_checksum::{ChecksumKind, Recognition, Representation};
                let algo = opentune_checksum::get(&algo)
                    .ok_or_else(|| anyhow!("unknown checksum algo '{algo}'"))?;
                let img = std::fs::read(&file)?;
                // A file handed to `checksum verify` is a full ECU image; declare
                // it so, and let the algorithm narrow it (or confirm recognition).
                let reports =
                    opentune_checksum::report_detailed(&*algo, &img, Representation::RawFullImage)?;
                for r in &reports {
                    let rep = match r.representation {
                        Representation::RawFullImage => "full image",
                        Representation::ProgrammingBlock => "extracted block",
                        Representation::ContainerSegment => "container segment",
                        Representation::Unknown => "unknown representation",
                    };
                    let kind = match r.checksum_kind {
                        ChecksumKind::Embedded => "embedded",
                        ChecksumKind::Transfer => "transfer",
                    };
                    // A merely-consistent match must never read as a recognised
                    // ECU family: only a Recognised block is "supported".
                    let recog = match r.recognition {
                        Recognition::Recognised => "recognised",
                        Recognition::Consistent => "consistent-only",
                    };
                    println!(
                        "{:<16} {} stored=0x{:08X} computed=0x{:08X}  [{} | {} | {}]",
                        r.status.name,
                        if r.status.ok { "OK  " } else { "BAD " },
                        r.status.stored,
                        r.status.computed,
                        rep,
                        kind,
                        recog,
                    );
                    if r.status.ok && !r.is_supported() {
                        println!(
                            "                 note: mathematical match only — NOT a recognised {} family (do not treat as supported)",
                            algo.id()
                        );
                    }
                }
            }
            ChecksumAction::Fix {
                algo,
                file,
                reference,
                only_bad,
            } => {
                let algo = opentune_checksum::get(&algo)
                    .ok_or_else(|| anyhow!("unknown checksum algo '{algo}'"))?;
                let mut img = std::fs::read(&file)?;
                let fixes = match reference {
                    Some(r) => {
                        let refimg = std::fs::read(&r)
                            .with_context(|| format!("read reference {}", r.display()))?;
                        algo.correct_with_reference(&mut img, &refimg)?
                    }
                    None if only_bad => opentune_checksum::correct_bad_only(&*algo, &mut img)?,
                    None => algo.correct(&mut img)?,
                };
                if fixes.is_empty() {
                    println!("No corrections needed.");
                } else {
                    std::fs::write(&file, &img)?;
                    for f in fixes {
                        println!(
                            "fixed {:<16} @0x{:08X} 0x{:08X} -> 0x{:08X}",
                            f.name, f.offset, f.old, f.new
                        );
                    }
                }
            }
            ChecksumAction::Algos => {
                for a in opentune_checksum::algorithms() {
                    println!(
                        "{:<12} {:<12} {:<4} {}",
                        a.id,
                        a.family,
                        if a.needs_reference { "ref" } else { "-" },
                        a.name
                    );
                    if !a.notes.is_empty() {
                        println!("               {}", a.notes);
                    }
                }
            }
            ChecksumAction::Attest { algo, file, out } => {
                let img =
                    std::fs::read(&file).map_err(|e| anyhow!("read {}: {e}", file.display()))?;
                let att = opentune_attest::attest_image(&algo, &img).map_err(|e| anyhow!("{e}"))?;
                let json = serde_json::to_string_pretty(&att)?;
                if let Some(out) = out {
                    std::fs::write(&out, &json)
                        .map_err(|e| anyhow!("write {}: {e}", out.display()))?;
                    println!(
                        "Attestation ({}) written to {} — satisfied={}",
                        att.algorithm,
                        out.display(),
                        att.satisfied
                    );
                } else {
                    println!("{json}");
                }
                if !att.satisfied {
                    println!("! {}", att.note);
                }
            }
        },
        Command::Flash {
            conn,
            addr,
            data,
            min_mv,
            vault,
            resume,
            transcript,
            chunk_size,
        } => {
            let mut drv = open_driver(&reg, &conn)?;
            let bytes = std::fs::read(&data).with_context(|| format!("read {}", data.display()))?;
            let region = MemoryRegion::new(parse_num(&addr)?, bytes.len() as u32);
            // Capture ECU identity up front so the archived backup is labelled.
            let id = drv.read_ecu_id().unwrap_or_default();
            let safety = opentune_flash::SafetyConfig {
                min_millivolts: min_mv,
                chunk_size,
                ..Default::default()
            };
            let report = if let Some(state) = resume.as_ref() {
                // Resumable, chunked, checkpointed flash with a session transcript.
                let mut tr = opentune_flash::Transcript::new();
                let rep = opentune_flash::flash_calibration_resumable_logged(
                    drv.as_mut(),
                    region,
                    &bytes,
                    &safety,
                    state,
                    &mut tr,
                    &mut noprogress,
                )
                .map_err(|e| anyhow!("flash: {e}"))?;
                let backup_path = if !rep.backup.is_empty() {
                    opentune_flash::vault::save_backup(&vault, &id, region, &rep.backup).ok()
                } else {
                    None
                };
                let written = match (&transcript, &backup_path) {
                    (Some(dir), _) => tr.write(dir, "flash").ok(),
                    (None, Some(bp)) => tr.write_next_to(bp).ok(),
                    (None, None) => tr.write(&vault, "flash").ok(),
                };
                if let Some(bp) = &backup_path {
                    println!("Pre-flash backup archived: {}", bp.display());
                }
                if let Some((log, jsonl)) = written {
                    println!(
                        "Session transcript: {} / {}",
                        log.display(),
                        jsonl.display()
                    );
                }
                rep
            } else {
                let rep = opentune_flash::flash_calibration(
                    drv.as_mut(),
                    region,
                    &bytes,
                    &safety,
                    &mut noprogress,
                )
                .map_err(|e| anyhow!("flash: {e}"))?;
                // Persist the pre-flash backup — the safety net if the write is bad.
                if !rep.backup.is_empty() {
                    match opentune_flash::vault::save_backup(&vault, &id, region, &rep.backup) {
                        Ok(p) => println!("Pre-flash backup archived: {}", p.display()),
                        Err(e) => eprintln!(
                            "WARNING: could not archive backup to {}: {e}",
                            vault.display()
                        ),
                    }
                }
                rep
            };
            println!(
                "Flashed {} bytes @0x{:08X} — backup {} bytes, verified={}",
                report.bytes_written,
                region.address,
                report.backup.len(),
                report.verified
            );
            drv.close()?;
        }
        Command::Dump {
            conn,
            addr,
            len,
            vault,
        } => {
            let mut drv = open_driver(&reg, &conn)?;
            let region = MemoryRegion::new(parse_num(&addr)?, parse_num(&len)?);
            let id = drv.read_ecu_id().unwrap_or_default();
            let data = drv.read_memory(region, &mut noprogress)?;
            let path = opentune_flash::vault::save_backup(&vault, &id, region, &data)
                .map_err(|e| anyhow!("vault: {e}"))?;
            println!(
                "Dumped {} bytes @0x{:08X} -> {}",
                data.len(),
                region.address,
                path.display()
            );
            drv.close()?;
        }
        Command::Findmaps {
            stock,
            tuned,
            gap,
            min_len,
            emit,
        } => {
            let a = std::fs::read(&stock).with_context(|| format!("read {}", stock.display()))?;
            let b = std::fs::read(&tuned).with_context(|| format!("read {}", tuned.display()))?;
            let regions = find_maps(&a, &b, gap, min_len);
            println!(
                "{} candidate map region(s) (gap={gap}, min_len={min_len}):",
                regions.len()
            );
            for (i, (s, e)) in regions.iter().enumerate() {
                println!(
                    "  map{:<3} 0x{:08X}..0x{:08X}  {:>6} bytes{}",
                    i,
                    s,
                    e,
                    e - s,
                    guess_stride(e - s)
                );
            }
            if let Some(path) = emit {
                let arr: Vec<_> = regions
                    .iter()
                    .enumerate()
                    .map(|(i, (s, e))| {
                        serde_json::json!({
                            "name": format!("map{i}"),
                            "address": s,
                            "length": e - s,
                            "notes": "auto-detected from stock/tuned diff; confirm type/axes",
                        })
                    })
                    .collect();
                std::fs::write(&path, serde_json::to_string_pretty(&arr)?)?;
                println!("wrote {} candidate(s) -> {}", regions.len(), path.display());
            }
        }
        Command::Ecudefs { action } => match action {
            EcudefsAction::List { dir } => {
                let defs = opentune_core::ecudef::load_dir(&dir).map_err(|e| anyhow!("{e}"))?;
                println!("{} ECU definition(s):", defs.len());
                for d in &defs {
                    println!(
                        "  {:<22} {:<16} {:>5} KiB  {:?}",
                        d.id,
                        d.name,
                        d.flash_size / 1024,
                        d.protocol
                    );
                }
            }
            EcudefsAction::Validate {
                path,
                image,
                warnings,
            } => {
                let img = match &image {
                    Some(p) => {
                        Some(std::fs::read(p).with_context(|| format!("read {}", p.display()))?)
                    }
                    None => None,
                };
                let files = collect_ecudef_files(&path)?;
                if files.is_empty() {
                    return Err(anyhow!(
                        "no *.json ECU definitions found at {}",
                        path.display()
                    ));
                }
                let mut total_errors = 0usize;
                let mut total_warns = 0usize;
                let mut bad_files = 0usize;
                for file in &files {
                    let name = file
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("?")
                        .to_string();
                    let text = std::fs::read_to_string(file)
                        .with_context(|| format!("read {}", file.display()))?;
                    let def = match opentune_core::ecudef::EcuDef::from_json(&text) {
                        Ok(d) => d,
                        Err(e) => {
                            total_errors += 1;
                            bad_files += 1;
                            println!("{name}: 1 error, 0 warnings");
                            println!("  ERROR  parse                    {e}");
                            continue;
                        }
                    };
                    let mut issues = def.validate();
                    issues.extend(def.validate_filename(&name));
                    if let Some(img) = &img {
                        issues.extend(def.validate_against_image(img));
                    }
                    let errs = opentune_core::validate::error_count(&issues);
                    let warns = opentune_core::validate::warn_count(&issues);
                    total_errors += errs;
                    total_warns += warns;
                    if errs > 0 {
                        bad_files += 1;
                    }
                    let shown: Vec<_> = issues
                        .iter()
                        .filter(|i| warnings || i.severity == opentune_core::Severity::Error)
                        .collect();
                    if errs == 0 && (warns == 0 || !warnings) {
                        println!("{name}: OK ({warns} warning(s))");
                    } else {
                        println!("{name}: {errs} error(s), {warns} warning(s)");
                    }
                    for i in shown {
                        let loc = if i.location.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", i.location)
                        };
                        println!(
                            "  {:<6} {:<28}{} {}",
                            i.severity.tag(),
                            i.code,
                            loc,
                            i.message
                        );
                    }
                }
                println!(
                    "\n{} file(s): {} error(s), {} warning(s); {} file(s) failed.",
                    files.len(),
                    total_errors,
                    total_warns,
                    bad_files
                );
                if total_errors > 0 {
                    std::process::exit(2);
                }
            }
        },
        Command::Identify {
            conn,
            ecudefs,
            catalog,
        } => {
            let mut drv = open_driver(&reg, &conn)?;
            let id = drv.read_ecu_id()?;
            println!("Reported identity:");
            if let Some(v) = &id.ecu_name {
                println!("  ECU name : {v}");
            }
            if let Some(v) = &id.hw_number {
                println!("  HW/part  : {v}");
            }
            if let Some(v) = &id.sw_number {
                println!("  SW number: {v}");
            }
            if let Some(v) = &id.supplier {
                println!("  Supplier : {v}");
            }
            if let Some(v) = &id.vin {
                println!("  VIN      : {v}");
            }
            let defs = opentune_core::ecudef::load_dir(&ecudefs).unwrap_or_default();
            match opentune_core::ecudef::match_ecu(&defs, &id) {
                Some(d) => println!(
                    "\nMatched definition: {} ({}) — protocol {:?}, checksum {:?}, seedkey {:?}",
                    d.id, d.name, d.protocol, d.checksum, d.seedkey
                ),
                None => println!("\nNo exact definition match in {}", ecudefs.display()),
            }
            if let Some(cat) = catalog {
                let entries = opentune_core::catalog::load_catalog(&cat).unwrap_or_default();
                let hits = opentune_core::catalog::match_catalog(&entries, &id);
                if hits.is_empty() {
                    println!("No catalog candidates.");
                } else {
                    println!("Catalog candidates:");
                    for h in hits.iter().take(10) {
                        println!(
                            "  {} {} [{}] {}",
                            h.make,
                            h.models.join("/"),
                            h.family.clone().unwrap_or_default(),
                            h.source.clone().unwrap_or_default()
                        );
                    }
                }
            }
            // The mock driver serves a file: recognise the image itself too.
            if let (true, Some(file)) = (conn.driver == "mock", &conn.file) {
                if let Ok(bytes) = std::fs::read(file) {
                    let ctx = opentune_scan::ScanContext {
                        catalog: Vec::new(),
                        ecudefs: defs.clone(),
                    };
                    let root = file.parent().unwrap_or_else(|| std::path::Path::new("."));
                    let rec = opentune_scan::scan_file(&ctx, root, file, &bytes);
                    println!("\nOffline image recognition:");
                    for l in opentune_scan::describe(&rec) {
                        println!("  {l}");
                    }
                }
            }
            drv.close()?;
        }
        Command::Db { action } => match action {
            DbAction::List { kind, q, dir } => {
                let db = opentune_db::Database::load_dir(&dir).map_err(|e| anyhow!("{e}"))?;
                let rows = db.list(kind.as_deref(), q.as_deref());
                println!("{} unit(s):", rows.len());
                for r in rows {
                    println!(
                        "  {:<24} {:<4} {:<44} {:<10} pinout: {}",
                        r.id,
                        r.kind.as_str(),
                        r.name,
                        r.checksum.as_deref().unwrap_or("-"),
                        r.pinout_confidence.map_or("none", |c| c.as_str())
                    );
                }
            }
            DbAction::Show { id, dir } => {
                let db = opentune_db::Database::load_dir(&dir).map_err(|e| anyhow!("{e}"))?;
                let v = db.get(&id).ok_or_else(|| anyhow!("unknown unit '{id}'"))?;
                println!("{}", serde_json::to_string_pretty(v)?);
            }
            DbAction::Validate { dir, ecudefs } => {
                let db = opentune_db::Database::load_dir(&dir).map_err(|e| anyhow!("{e}"))?;
                let ids = opentune_db::ecudef_ids(&ecudefs).map_err(|e| anyhow!("{e}"))?;
                let issues = db.validate(Some(&ids));
                for i in &issues {
                    println!("  {i}");
                }
                if !issues.is_empty() {
                    return Err(anyhow!("{} issue(s)", issues.len()));
                }
                println!("{} unit(s) OK", db.entries().len());
            }
        },
        Command::Dtc { code } => match opentune_dtc::lookup(&code) {
            Some(d) => println!("{}  [{:?}]  {}", d.code, d.system, d.description),
            None => println!("{code}: malformed code"),
        },
        Command::Vin { vin } => match opentune_vin::decode_vin(&vin) {
            Ok(v) => {
                println!("VIN {}", v.vin);
                println!(
                    "  WMI        : {} ({}, {})",
                    v.wmi,
                    v.wmi_info.manufacturer.unwrap_or("unknown make"),
                    v.wmi_info.country.unwrap_or("unknown country")
                );
                match v.model_year.resolved {
                    Some(y) => println!("  Model year : {} -> {}", v.model_year.code, y),
                    None => println!(
                        "  Model year : {} -> {:?} (ambiguous)",
                        v.model_year.code, v.model_year.candidates
                    ),
                }
                println!(
                    "  Check digit: {} ({})",
                    v.check_digit,
                    match v.check_digit_valid {
                        Some(true) => "valid",
                        Some(false) => "INVALID",
                        None => "not verifiable (EU filler)",
                    }
                );
                println!("  Plant      : {}   Serial: {}", v.plant_code, v.serial);
                if !v.flags.is_empty() {
                    println!("  Notes      : {}", v.flags.join("; "));
                }
            }
            Err(e) => println!("invalid VIN: {e:?}"),
        },
        Command::DecodePart { part } => {
            let p = opentune_vin::parse_part_number(&part);
            println!("Part {}  [{:?}]", p.normalized, p.kind);
            if let Some(s) = p.supplier {
                println!("  Supplier: {s}");
            }
            if let Some(g) = &p.vag_type_group {
                println!(
                    "  VAG: type={} main={} sub={} ver={}",
                    g,
                    p.vag_main_group.as_deref().unwrap_or("-"),
                    p.vag_subgroup.as_deref().unwrap_or("-"),
                    p.vag_version.as_deref().unwrap_or("-")
                );
            }
            if let Some(f) = &p.bosch_family {
                println!(
                    "  Bosch: family={} consecutive={}",
                    f,
                    p.bosch_consecutive.as_deref().unwrap_or("-")
                );
            }
            if !p.flags.is_empty() {
                println!("  Notes: {}", p.flags.join("; "));
            }
        }
        Command::Compare {
            ecudefs,
            def,
            stock,
            tuned,
            map,
        } => {
            let defs = opentune_core::ecudef::load_dir(&ecudefs)
                .map_err(|e| anyhow!("load {}: {e}", ecudefs.display()))?;
            let d = defs
                .iter()
                .find(|d| d.id == def)
                .ok_or_else(|| anyhow!("no definition '{def}' in {}", ecudefs.display()))?;
            if d.maps.is_empty() {
                println!("Definition '{def}' has no maps to compare.");
                return Ok(());
            }
            let a = std::fs::read(&stock).with_context(|| format!("read {}", stock.display()))?;
            let b = std::fs::read(&tuned).with_context(|| format!("read {}", tuned.display()))?;
            compare_maps(d, &a, &b, map.as_deref());
        }
        Command::ImportXdf { file, into } => {
            // XDF payloads in the wild are sometimes Windows-1251/Latin-1, not
            // UTF-8. The structure is ASCII; decode lossily so a non-UTF-8 unit
            // string can't abort the import (affected text is flagged, not lost).
            let raw = std::fs::read(&file).map_err(|e| anyhow!("read {}: {e}", file.display()))?;
            let src = String::from_utf8_lossy(&raw);
            let import = opentune_xdf::parse_xdf(&src).map_err(|e| anyhow!("{e:?}"))?;
            println!(
                "XDF: {}  (base offset 0x{:X}) — {} map(s)",
                import.title.as_deref().unwrap_or("(untitled)"),
                import.base_offset,
                import.maps.len()
            );
            for m in &import.maps {
                let note = if m.notes.is_empty() {
                    String::new()
                } else {
                    format!("  ! {}", m.notes)
                };
                println!(
                    "  {:<28} @0x{:06X}  {}x{}  {:?}/{:?}  x{}{:+}{}",
                    m.name,
                    m.address,
                    m.rows,
                    m.cols,
                    m.data_type,
                    m.endian,
                    m.scaling.factor,
                    m.scaling.offset,
                    note
                );
            }
            // Scalar constants are a distinct object class; surface them too
            // (previously dropped) with name/addr/type/scaling/unit.
            if !import.constants.is_empty() {
                println!("Constants ({}):", import.constants.len());
                for c in &import.constants {
                    let unit = if c.unit.is_empty() {
                        String::new()
                    } else {
                        format!("  [{}]", c.unit)
                    };
                    let note = if c.notes.is_empty() {
                        String::new()
                    } else {
                        format!("  ! {}", c.notes)
                    };
                    println!(
                        "  {:<28} @0x{:06X}  {:?}/{:?}  x{}{:+}{}{}",
                        c.name,
                        c.address,
                        c.data_type,
                        c.endian,
                        c.scaling.factor,
                        c.scaling.offset,
                        unit,
                        note
                    );
                }
            }
            // Import warnings were also dropped before; print them so a lossy /
            // skipped-entry import is visible rather than silent.
            for w in &import.warnings {
                println!("  ! warning: {w}");
            }
            if let Some(into) = into {
                let txt = std::fs::read_to_string(&into)
                    .map_err(|e| anyhow!("read {}: {e}", into.display()))?;
                let mut def: opentune_core::ecudef::EcuDef = serde_json::from_str(&txt)
                    .map_err(|e| anyhow!("parse {}: {e}", into.display()))?;
                let have: std::collections::HashSet<String> =
                    def.maps.iter().map(|m| m.name.clone()).collect();
                let mut added = 0usize;
                for m in import.maps {
                    if have.contains(&m.name) {
                        continue;
                    }
                    def.maps.push(m);
                    added += 1;
                }
                let out = serde_json::to_string_pretty(&def)?;
                std::fs::write(&into, out).map_err(|e| anyhow!("write {}: {e}", into.display()))?;
                println!(
                    "Merged {added} new map(s) into {} ({} total).",
                    into.display(),
                    def.maps.len()
                );
            }
        }
        Command::ImportEcuflash { file, into } => {
            // EcuFlash/RomRaider XML can be Latin-1/Win-1252; decode lossily.
            let raw = std::fs::read(&file).map_err(|e| anyhow!("read {}: {e}", file.display()))?;
            let src = String::from_utf8_lossy(&raw);
            let import = opentune_ecuflash::parse_ecuflash(&src).map_err(|e| anyhow!("{e}"))?;
            let r = &import.rom_id;
            println!(
                "EcuFlash: {} {} {} {} [{}] ({}) — {} map(s)",
                r.make,
                r.model,
                r.submodel,
                r.year,
                r.xmlid,
                r.memmodel,
                import.maps.len()
            );
            for w in &import.warnings {
                println!("  ! warning: {w}");
            }
            for m in &import.maps {
                let note = if m.notes.is_empty() {
                    String::new()
                } else {
                    format!("  ! {}", m.notes)
                };
                println!(
                    "  {:<28} @0x{:06X}  {}x{}  {:?}/{:?}  x{}{:+}{}",
                    m.name,
                    m.address,
                    m.rows,
                    m.cols,
                    m.data_type,
                    m.endian,
                    m.scaling.factor,
                    m.scaling.offset,
                    note
                );
            }
            if let Some(into) = into {
                let txt = std::fs::read_to_string(&into)
                    .map_err(|e| anyhow!("read {}: {e}", into.display()))?;
                let mut def: opentune_core::ecudef::EcuDef = serde_json::from_str(&txt)
                    .map_err(|e| anyhow!("parse {}: {e}", into.display()))?;
                let have: std::collections::HashSet<String> =
                    def.maps.iter().map(|m| m.name.clone()).collect();
                let mut added = 0usize;
                for m in import.maps {
                    if have.contains(&m.name) {
                        continue;
                    }
                    def.maps.push(m);
                    added += 1;
                }
                std::fs::write(&into, serde_json::to_string_pretty(&def)?)
                    .map_err(|e| anyhow!("write {}: {e}", into.display()))?;
                println!(
                    "Merged {added} new map(s) into {} ({} total).",
                    into.display(),
                    def.maps.len()
                );
            }
        }
        Command::ImportOls {
            file,
            version,
            into,
        } => {
            let raw = std::fs::read(&file).map_err(|e| anyhow!("read {}: {e}", file.display()))?;
            let project = opentune_ols::OlsProject::parse_version(&raw, version)
                .map_err(|e| anyhow!("parse {}: {e}", file.display()))?;
            println!(
                "WinOLS: format {} — ROM {} byte(s) (version {}) — {} map(s)",
                project.format_version,
                project.rom.size(),
                project.rom.version_index,
                project.maps.len()
            );
            for w in &project.warnings {
                println!("  ! warning: {w}");
            }
            for m in &project.maps {
                println!(
                    "  {:<28} @0x{:06X}  {}x{}",
                    m.name, m.address, m.rows, m.cols
                );
            }
            if let Some(into) = into {
                let txt = std::fs::read_to_string(&into)
                    .map_err(|e| anyhow!("read {}: {e}", into.display()))?;
                let mut def: opentune_core::ecudef::EcuDef = serde_json::from_str(&txt)
                    .map_err(|e| anyhow!("parse {}: {e}", into.display()))?;
                let have: std::collections::HashSet<String> =
                    def.maps.iter().map(|m| m.name.clone()).collect();
                let mut added = 0usize;
                for m in project.maps {
                    if have.contains(&m.name) {
                        continue;
                    }
                    def.maps.push(m);
                    added += 1;
                }
                let out = serde_json::to_string_pretty(&def)?;
                std::fs::write(&into, out).map_err(|e| anyhow!("write {}: {e}", into.display()))?;
                println!(
                    "Merged {added} new map(s) into {} ({} total).",
                    into.display(),
                    def.maps.len()
                );
            }
        }
        Command::Packs { dir } => {
            let lib = opentune_packs::PackLibrary::load_all(&dir);
            if lib.packs.is_empty() && lib.failed.is_empty() {
                println!(
                    "No packs found under {} (a pack is a directory with pack.json).",
                    dir.display()
                );
            }
            for p in &lib.packs {
                let m = &p.manifest;
                let name = if m.name.is_empty() {
                    "(unnamed)"
                } else {
                    &m.name
                };
                let ver = if m.version.is_empty() {
                    "?"
                } else {
                    &m.version
                };
                println!("pack {} — {name} v{ver}", m.id);
                if !m.maker.is_empty() || !m.platform.is_empty() {
                    println!("  {} {}", m.maker, m.platform);
                }
                if !m.license.is_empty() {
                    println!("  license: {}", m.license);
                }
                println!(
                    "  defs: {} ecu · {} coding · {} adaptation · {} service · {} logger · {} module-list  ({} loaded)",
                    p.ecudefs.len(),
                    p.coding.len(),
                    p.adaptation.len(),
                    p.services.len(),
                    p.loggers.len(),
                    p.modules.len(),
                    p.report.loaded,
                );
                for n in &p.report.notes {
                    println!("  ! {} [{}]: {}", n.file, n.kind.dir(), n.message);
                }
            }
            for (d, e) in &lib.failed {
                println!("! failed to open pack {}: {e}", d.display());
            }
            let c = lib.counts();
            println!(
                "TOTAL across {} pack(s): {} ecu · {} coding · {} adaptation · {} service · {} logger · {} module-list",
                lib.packs.len(),
                c[0].1,
                c[1].1,
                c[2].1,
                c[3].1,
                c[4].1,
                c[5].1,
            );
        }
        Command::ImportA2l { file, into, base } => {
            let base = parse_num(&base)?;
            // A2L text can be Latin-1/Win-1252; decode lossily (ASCII structure).
            let raw = std::fs::read(&file).map_err(|e| anyhow!("read {}: {e}", file.display()))?;
            let src = String::from_utf8_lossy(&raw);
            let import = opentune_a2l::parse_a2l(&src).map_err(|e| anyhow!("{e}"))?;
            println!(
                "A2L: project {:?} module {:?} — {} map(s)  (base 0x{:X})",
                import.project.as_deref().unwrap_or("?"),
                import.module.as_deref().unwrap_or("?"),
                import.maps.len(),
                base
            );
            // Rebase CPU addresses to bin file offsets (checked: never wrap).
            let mut maps = import.maps;
            let rebase = rebase_maps_checked(&mut maps, base);
            for d in &rebase.dropped {
                println!("  DROPPED (address below base): {d}");
            }
            for a in &rebase.axis_failures {
                println!("  WARN axis address below base (cleared): {a}");
            }
            for m in maps.iter().take(60) {
                let note = if m.notes.is_empty() {
                    String::new()
                } else {
                    format!("  ! {}", m.notes)
                };
                println!(
                    "  {:<34} @0x{:06X}  {}x{}  {:?}/{:?}  x{}{:+}{}",
                    m.name,
                    m.address,
                    m.rows,
                    m.cols,
                    m.data_type,
                    m.endian,
                    m.scaling.factor,
                    m.scaling.offset,
                    note
                );
            }
            if maps.len() > 60 {
                println!("  … and {} more", maps.len() - 60);
            }
            if let Some(into) = into {
                let txt = std::fs::read_to_string(&into)
                    .map_err(|e| anyhow!("read {}: {e}", into.display()))?;
                let mut def: opentune_core::ecudef::EcuDef = serde_json::from_str(&txt)
                    .map_err(|e| anyhow!("parse {}: {e}", into.display()))?;
                let have: std::collections::HashSet<String> =
                    def.maps.iter().map(|m| m.name.clone()).collect();
                let mut added = 0usize;
                for m in maps {
                    if have.contains(&m.name) {
                        continue;
                    }
                    def.maps.push(m);
                    added += 1;
                }
                let out = serde_json::to_string_pretty(&def)?;
                std::fs::write(&into, out).map_err(|e| anyhow!("write {}: {e}", into.display()))?;
                println!(
                    "Merged {added} new map(s) into {} ({} total).",
                    into.display(),
                    def.maps.len()
                );
            }
        }
        Command::Vault { dir, verify } => {
            if !dir.is_dir() {
                return Err(anyhow!("vault directory not found: {}", dir.display()));
            }
            let mut metas: Vec<PathBuf> = std::fs::read_dir(&dir)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().map(|x| x == "meta").unwrap_or(false))
                .collect();
            metas.sort();
            if metas.is_empty() {
                println!("No backups in {}", dir.display());
                return Ok(());
            }
            println!("{} backup(s) in {}:", metas.len(), dir.display());
            for meta in &metas {
                let txt = std::fs::read_to_string(meta).unwrap_or_default();
                let field = |k: &str| -> String {
                    txt.lines()
                        .find_map(|l| l.split_once('=').filter(|(key, _)| key.trim() == k))
                        .map(|(_, v)| v.trim().to_string())
                        .unwrap_or_default()
                };
                let bin = meta.with_extension("bin");
                let name = bin
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let sw = field("sw_number");
                let addr = field("address");
                let len = field("length");
                let stored_crc = field("crc32");
                let mut status = String::new();
                if verify {
                    status = match std::fs::read(&bin) {
                        Ok(data) => {
                            let got = format!("0x{:08X}", opentune_checksum::crc32(&data));
                            if got.eq_ignore_ascii_case(&stored_crc) {
                                "  [OK]".into()
                            } else {
                                format!("  [MISMATCH got {got}]")
                            }
                        }
                        Err(_) => "  [.bin missing]".into(),
                    };
                }
                println!(
                    "  {:<40} sw={:<12} @{} {} bytes crc={}{}",
                    name, sw, addr, len, stored_crc, status
                );
            }
        }
        Command::A2l2Xdf {
            file,
            out,
            base,
            only,
        } => {
            let base = parse_num(&base)?;
            // A2L text can be Latin-1/Win-1252; decode lossily (ASCII structure),
            // exactly like `import-a2l`.
            let raw = std::fs::read(&file).map_err(|e| anyhow!("read {}: {e}", file.display()))?;
            let src = String::from_utf8_lossy(&raw);
            let import = opentune_a2l::parse_a2l(&src).map_err(|e| anyhow!("{e}"))?;
            let title = import
                .module
                .clone()
                .or_else(|| import.project.clone())
                .unwrap_or_else(|| "a2l2xdf".to_string());
            let parsed_total = import.maps.len();
            let mut maps = import.maps;

            // Optional name filter (case-insensitive substring), applied before
            // rebasing so dropped-vs-filtered stay distinct in the report.
            let mut filtered_out: Vec<String> = Vec::new();
            if let Some(sub) = &only {
                let needle = sub.to_ascii_lowercase();
                let mut kept = Vec::with_capacity(maps.len());
                for m in std::mem::take(&mut maps) {
                    if m.name.to_ascii_lowercase().contains(&needle) {
                        kept.push(m);
                    } else {
                        filtered_out.push(m.name.clone());
                    }
                }
                maps = kept;
            }

            // Rebase CPU/logical addresses to flash-file offsets (checked).
            let rebase = rebase_maps_checked(&mut maps, base);

            // Flag non-linear conversions: the A2L importer imports any
            // non-affine COMPU_METHOD pre-scaled (factor 1, offset 0) and records
            // it in `notes` — surface that so the XDF is not mistaken for exact.
            let nonlinear: Vec<String> = maps
                .iter()
                .filter(|m| m.notes.contains("pre-scaled"))
                .map(|m| format!("{}: {}", m.name, m.notes))
                .collect();

            if maps.is_empty() {
                return Err(anyhow!(
                    "no maps to export (parsed {parsed_total}, {} filtered out, {} dropped by rebasing)",
                    filtered_out.len(),
                    rebase.dropped.len()
                ));
            }

            let xml = opentune_xdf::export_xdf(Some(&title), &maps);
            match &out {
                Some(p) => {
                    std::fs::write(p, &xml).map_err(|e| anyhow!("write {}: {e}", p.display()))?;
                }
                None => print!("{xml}"),
            }

            // --- Conversion-loss report (always; to stderr so it never mixes
            // into a piped XDF on stdout). Never silently lossy. ---
            eprintln!("=== a2l2xdf conversion-loss report ===");
            eprintln!(
                "A2L: project {:?} module {:?} — parsed {parsed_total} map(s), base 0x{base:X}",
                import.project.as_deref().unwrap_or("?"),
                import.module.as_deref().unwrap_or("?"),
            );
            eprintln!("Exported {} map(s) to XDF.", maps.len());
            if let Some(sub) = &only {
                eprintln!(
                    "Filtered by --only {sub:?}: {} map(s) excluded (not loss).",
                    filtered_out.len()
                );
                for n in &filtered_out {
                    eprintln!("    - {n}");
                }
            }
            if rebase.dropped.is_empty() {
                eprintln!("Maps dropped (address rebasing underflowed base): none.");
            } else {
                eprintln!(
                    "Maps dropped (address rebasing underflowed base): {}",
                    rebase.dropped.len()
                );
                for d in &rebase.dropped {
                    eprintln!("    - {d}");
                }
            }
            if rebase.axis_failures.is_empty() {
                eprintln!("Axis addresses that failed rebasing: none.");
            } else {
                eprintln!(
                    "Axis addresses that failed rebasing (axis cleared, map kept): {}",
                    rebase.axis_failures.len()
                );
                for a in &rebase.axis_failures {
                    eprintln!("    - {a}");
                }
            }
            if nonlinear.is_empty() {
                eprintln!("Non-linear scalings imported pre-scaled: none.");
            } else {
                eprintln!(
                    "Non-linear scalings flagged (imported pre-scaled, factor 1 / offset 0): {}",
                    nonlinear.len()
                );
                for n in &nonlinear {
                    eprintln!("    ! {n}");
                }
            }
            if let Some(p) = &out {
                eprintln!("Wrote {} map(s) to {}", maps.len(), p.display());
            }
        }
        Command::ExportXdf { ecudefs, def, out } => {
            let defs = opentune_core::ecudef::load_dir(&ecudefs)
                .map_err(|e| anyhow!("load {}: {e}", ecudefs.display()))?;
            let d = defs
                .iter()
                .find(|d| d.id == def)
                .ok_or_else(|| anyhow!("no definition '{def}' in {}", ecudefs.display()))?;
            if d.maps.is_empty() {
                return Err(anyhow!("definition '{def}' has no maps to export"));
            }
            let xml = opentune_xdf::export_xdf(Some(&d.name), &d.maps);
            match out {
                Some(p) => {
                    std::fs::write(&p, &xml).map_err(|e| anyhow!("write {}: {e}", p.display()))?;
                    println!("Wrote {} map(s) to {}", d.maps.len(), p.display());
                }
                None => print!("{xml}"),
            }
        }
        Command::ExportA2l { ecudefs, def, out } => {
            let defs = opentune_core::ecudef::load_dir(&ecudefs)
                .map_err(|e| anyhow!("load {}: {e}", ecudefs.display()))?;
            let d = defs
                .iter()
                .find(|d| d.id == def)
                .ok_or_else(|| anyhow!("no definition '{def}' in {}", ecudefs.display()))?;
            if d.maps.is_empty() {
                return Err(anyhow!("definition '{def}' has no maps to export"));
            }
            let a2l = opentune_a2l::export_a2l(&d.maps, Some(&d.id), Some(&d.name));
            match out {
                Some(p) => {
                    std::fs::write(&p, &a2l).map_err(|e| anyhow!("write {}: {e}", p.display()))?;
                    println!("Wrote {} map(s) to {}", d.maps.len(), p.display());
                }
                None => print!("{a2l}"),
            }
        }
        Command::Scan {
            dir,
            human,
            catalog,
            ecudefs,
        } => {
            let ctx = opentune_scan::ScanContext::load(&catalog, &ecudefs)?;
            if dir.is_file() {
                let bytes =
                    std::fs::read(&dir).map_err(|e| anyhow!("read {}: {e}", dir.display()))?;
                let root = dir.parent().unwrap_or_else(|| std::path::Path::new("."));
                let rec = opentune_scan::scan_file(&ctx, root, &dir, &bytes);
                if human {
                    println!("{}", dir.display());
                    for l in opentune_scan::describe(&rec) {
                        println!("{l}");
                    }
                } else {
                    println!("{}", serde_json::to_string_pretty(&rec)?);
                }
            } else {
                let report = opentune_scan::scan_dir(&ctx, &dir)?;
                if human {
                    for rec in &report.records {
                        println!("{}", rec.rel_path);
                        for l in opentune_scan::describe(rec) {
                            println!("  {l}");
                        }
                    }
                    println!("{}", serde_json::to_string_pretty(&report.summary)?);
                } else {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                }
            }
        }
        Command::Container { action } => match action {
            ContainerAction::Unpack {
                input,
                outdir,
                frf_key,
                aes_key,
                aes_iv,
                xor_key,
                sgo_table,
                sgo_cipher,
            } => {
                container_unpack(ContainerUnpackArgs {
                    input,
                    outdir,
                    frf_key,
                    aes_key,
                    aes_iv,
                    xor_key,
                    sgo_table,
                    sgo_cipher,
                })?;
            }
        },
        Command::Cbf { file, json, dtcs } => {
            let cbf = opentune_cbf::parse_file(&file)
                .map_err(|e| anyhow!("parse {}: {e}", file.display()))?;
            if json {
                println!(
                    "{}",
                    opentune_cbf::to_json(&cbf).map_err(|e| anyhow!("{e}"))?
                );
            } else if dtcs {
                for entry in opentune_cbf::dtc_entries(&cbf) {
                    println!("{}\t{}", entry.code, entry.description);
                }
            } else {
                println!("CBF version: {}", cbf.cbf_version);
                if !cbf.source_xml.is_empty() {
                    println!("source: {}", cbf.source_xml);
                }
                for ecu in &cbf.ecus {
                    println!(
                        "ECU {} ({}): {} DTCs, {} services, {} variants",
                        ecu.qualifier,
                        ecu.name.as_deref().unwrap_or("-"),
                        ecu.dtcs.len(),
                        ecu.services.len(),
                        ecu.variants.len(),
                    );
                }
            }
        }
        Command::Coding { action } => run_coding(&reg, action)?,
        Command::Adaptation {
            def,
            driver,
            store,
            timeout_ms,
            action,
        } => run_adaptation(&reg, &def, &driver, store.as_deref(), timeout_ms, action)?,
        Command::Autoscan {
            modules,
            driver,
            store,
            timeout_ms,
        } => run_autoscan(&reg, &modules, &driver, store.as_deref(), timeout_ms)?,
        Command::Service {
            script,
            driver,
            store,
            timeout_ms,
            action,
        } => run_service(&reg, &script, &driver, store.as_deref(), timeout_ms, action)?,
        Command::Simulate {
            dids,
            require_security_write,
            requests,
        } => run_simulate(&dids, require_security_write, &requests)?,
        Command::Obd { action } => obd_cmd::run(&reg, action)?,
        Command::Log {
            def,
            driver,
            store,
            timeout_ms,
            rounds,
            interval_ms,
            out,
            params,
        } => run_log(
            &reg,
            &def,
            &driver,
            store.as_deref(),
            timeout_ms,
            rounds,
            interval_ms,
            out.as_deref(),
            &params,
        )?,
        Command::Candump(args) => candump::run(&reg, args)?,
    }
    Ok(())
}

/// Open the `conn`'s driver and borrow its diagnostic transport, erroring
/// clearly when the driver cannot expose one.
/// Apply serial-port overrides from the environment to a connect config, so a
/// live serial driver (e.g. `elm327obd`) can be pointed at whatever device it
/// enumerates as without a per-command flag:
///   `OPENTUNE_PORT=/dev/ttyUSB1 OPENTUNE_BAUD=115200 opentune autoscan …`
/// Unset variables leave the driver's own defaults (elm327obd: /dev/ttyUSB0
/// @38400) untouched.
fn apply_serial_env(cfg: &mut ConnectConfig) {
    if let Ok(port) = std::env::var("OPENTUNE_PORT") {
        if !port.trim().is_empty() {
            cfg.options.insert("port".into(), port);
        }
    }
    if let Ok(baud) = std::env::var("OPENTUNE_BAUD") {
        if !baud.trim().is_empty() {
            cfg.options.insert("baud".into(), baud);
        }
    }
}

fn open_coding_driver(reg: &Registry, conn: &CodingConn) -> Result<Box<dyn Interface>> {
    let mut drv = reg
        .make(&conn.driver)
        .ok_or_else(|| anyhow!("unknown driver '{}'", conn.driver))?;
    let mut cfg = ConnectConfig::default();
    if let Some(store) = &conn.store {
        cfg.options
            .insert("coding_store".into(), store.to_string_lossy().into_owned());
    }
    apply_serial_env(&mut cfg);
    drv.open(&cfg)
        .map_err(|e| anyhow!("open {}: {e}", conn.driver))?;
    Ok(drv)
}

/// Load and semantically validate a coding definition from `conn.def`.
fn load_coding_def(conn: &CodingConn) -> Result<opentune_coding::CodingDef> {
    let text = std::fs::read_to_string(&conn.def)
        .with_context(|| format!("read coding def {}", conn.def.display()))?;
    let def = opentune_coding::CodingDef::from_json(&text)
        .map_err(|e| anyhow!("parse coding def {}: {e}", conn.def.display()))?;
    def.validate()
        .map_err(|e| anyhow!("invalid coding def {}: {e}", conn.def.display()))?;
    Ok(def)
}

/// Open a driver that exposes a diagnostic transport, optionally backing the
/// `mock` driver's offline UDS with a `--store` JSON. Shared by the adaptation,
/// auto-scan and service-function subcommands.
fn open_diag_driver(
    reg: &Registry,
    driver: &str,
    store: Option<&std::path::Path>,
) -> Result<Box<dyn Interface>> {
    let mut drv = reg
        .make(driver)
        .ok_or_else(|| anyhow!("unknown driver '{driver}'"))?;
    let mut cfg = ConnectConfig::default();
    if let Some(store) = store {
        cfg.options
            .insert("coding_store".into(), store.to_string_lossy().into_owned());
    }
    apply_serial_env(&mut cfg);
    drv.open(&cfg).map_err(|e| anyhow!("open {driver}: {e}"))?;
    Ok(drv)
}

/// Parse a whitespace-tolerant hex-byte string (`"22 F1 90"` or `"22F190"`).
fn parse_hex_vec(s: &str) -> Result<Vec<u8>> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !clean.len().is_multiple_of(2) {
        return Err(anyhow!("hex string '{s}' has an odd number of digits"));
    }
    (0..clean.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&clean[i..i + 2], 16)
                .map_err(|_| anyhow!("invalid hex byte in '{s}'"))
        })
        .collect()
}

/// Dispatch the `simulate` subcommand: run the in-memory UDS server offline.
fn run_simulate(dids: &[String], require_security_write: bool, requests: &[String]) -> Result<()> {
    use opentune_udssim::server::{ServerConfig, UdsServer};

    let config = ServerConfig {
        require_security_for_write: require_security_write,
        ..Default::default()
    };
    let mut server = UdsServer::with_config(config);

    for d in dids {
        let (id, hex) = d
            .split_once('=')
            .ok_or_else(|| anyhow!("--did expects ID=HEX, got '{d}'"))?;
        let did = u16::from_str_radix(id.trim().trim_start_matches("0x"), 16)
            .map_err(|_| anyhow!("invalid DID '{id}' (expected hex, e.g. F190)"))?;
        server.set_did(did, parse_hex_vec(hex)?);
    }

    for r in requests {
        let req = parse_hex_vec(r)?;
        let resp = server.handle(&req);
        println!("-> {:<20} <- {}", hex_bytes(&req), hex_bytes(&resp));
    }
    Ok(())
}

/// Format bytes as uppercase, space-separated hex.
fn hex_bytes(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Dispatch the `log` subcommand: parse a logger.xml and poll live data to CSV.
#[allow(clippy::too_many_arguments)]
fn run_log(
    reg: &Registry,
    def_path: &std::path::Path,
    driver: &str,
    store: Option<&std::path::Path>,
    timeout_ms: u64,
    rounds: usize,
    interval_ms: u64,
    out: Option<&std::path::Path>,
    params: &[String],
) -> Result<()> {
    use opentune_core::CoreDiag;

    let raw = std::fs::read(def_path)
        .with_context(|| format!("read logger def {}", def_path.display()))?;
    let src = String::from_utf8_lossy(&raw);
    let def = opentune_logdef::parse(&src)
        .map_err(|e| anyhow!("parse logger def {}: {e:?}", def_path.display()))?;

    let ids: Vec<&str> = params.iter().map(String::as_str).collect();

    let mut drv = open_diag_driver(reg, driver, store)?;
    let transport = drv
        .diag()
        .ok_or_else(|| anyhow!("driver '{driver}' does not expose a diagnostic transport"))?;
    let mut diag = CoreDiag::new(transport, timeout_ms);

    match out {
        Some(path) => {
            let mut f = std::fs::File::create(path)
                .map_err(|e| anyhow!("create {}: {e}", path.display()))?;
            opentune_logdef::poll_to_csv(&mut diag, &def, &ids, rounds, interval_ms, &mut f)?;
            println!("Logged {rounds} round(s) to {}", path.display());
        }
        None => {
            let mut stdout = std::io::stdout().lock();
            opentune_logdef::poll_to_csv(&mut diag, &def, &ids, rounds, interval_ms, &mut stdout)?;
        }
    }
    Ok(())
}

/// Dispatch the `adaptation` subcommand.
fn run_adaptation(
    reg: &Registry,
    def_path: &std::path::Path,
    driver: &str,
    store: Option<&std::path::Path>,
    timeout_ms: u64,
    action: AdaptAction,
) -> Result<()> {
    use opentune_adaptation::AdaptationDef;
    use opentune_core::CoreDiag;

    let text = std::fs::read_to_string(def_path)
        .with_context(|| format!("read adaptation def {}", def_path.display()))?;
    let def = AdaptationDef::from_json(&text)
        .map_err(|e| anyhow!("parse adaptation def {}: {e}", def_path.display()))?;
    def.validate()
        .map_err(|e| anyhow!("invalid adaptation def {}: {e}", def_path.display()))?;

    match action {
        AdaptAction::List => {
            println!("Adaptation channels in {} ({}):", def.name, def.id);
            for ch in &def.channels {
                println!("  {}", ch.name);
            }
        }
        AdaptAction::Read { channel } => {
            let mut drv = open_diag_driver(reg, driver, store)?;
            let transport = drv.diag().ok_or_else(|| {
                anyhow!("driver '{driver}' does not expose a diagnostic transport")
            })?;
            let mut diag = CoreDiag::new(transport, timeout_ms);
            let r = opentune_adaptation::read_value(&mut diag, &def, &channel)
                .map_err(|e| anyhow!("{e}"))?;
            println!(
                "{}: raw={} engineering={} {}",
                r.channel, r.raw, r.engineering, r.unit
            );
        }
    }
    Ok(())
}

/// Dispatch the `autoscan` subcommand (read-only whole-vehicle scan).
fn run_autoscan(
    reg: &Registry,
    modules_path: &std::path::Path,
    driver: &str,
    store: Option<&std::path::Path>,
    timeout_ms: u64,
) -> Result<()> {
    use opentune_autoscan::VehicleModules;
    use opentune_core::CoreDiag;

    let text = std::fs::read_to_string(modules_path)
        .with_context(|| format!("read module map {}", modules_path.display()))?;
    let modules = VehicleModules::from_json(&text)
        .map_err(|e| anyhow!("parse module map {}: {e}", modules_path.display()))?;

    // Bind the adapter to each module's own request/response ids before
    // probing it (as the desktop does): one binding would send every module's
    // requests to the one ECU it is bound to.
    let mut results = Vec::with_capacity(modules.modules.len());
    for m in &modules.modules {
        let mut drv = reg
            .make(driver)
            .ok_or_else(|| anyhow!("unknown driver '{driver}'"))?;
        let mut cfg = ConnectConfig {
            tx_id: m.req_id,
            rx_id: m.resp_id,
            ..ConnectConfig::default()
        };
        if let Some(store) = store {
            cfg.options
                .insert("coding_store".into(), store.to_string_lossy().into_owned());
        }
        apply_serial_env(&mut cfg);
        drv.open(&cfg).map_err(|e| anyhow!("open {driver}: {e}"))?;
        let transport = drv
            .diag()
            .ok_or_else(|| anyhow!("driver '{driver}' does not expose a diagnostic transport"))?;
        let mut diag = CoreDiag::new(transport, timeout_ms);
        results.push(opentune_autoscan::scan_module(&mut diag, m));
        let _ = drv.close();
    }
    let scan = opentune_autoscan::VehicleScan { modules: results };

    let present = scan.present().count();
    println!(
        "Scanned {} modules: {} present, {} DTC(s) total",
        scan.modules.len(),
        present,
        scan.dtc_count()
    );
    for m in &scan.modules {
        let mark = if m.present { "+" } else { "-" };
        print!("  [{mark}] {:<24}", m.name);
        if let Some(id) = &m.identity {
            if let Some(vin) = &id.vin {
                print!(" VIN={vin}");
            }
        }
        if !m.dtcs.is_empty() {
            print!(" {} DTC(s)", m.dtcs.len());
        }
        if let Some(err) = &m.error {
            print!(" ({err})");
        }
        println!();
    }
    Ok(())
}

/// Dispatch the `service` subcommand.
fn run_service(
    reg: &Registry,
    script_path: &std::path::Path,
    driver: &str,
    store: Option<&std::path::Path>,
    timeout_ms: u64,
    action: ServiceAction,
) -> Result<()> {
    use opentune_core::CoreDiag;
    use opentune_servicefn::{run_script, RunOptions, ServiceScript};

    let text = std::fs::read_to_string(script_path)
        .with_context(|| format!("read service script {}", script_path.display()))?;
    let script = ServiceScript::from_json(&text)
        .map_err(|e| anyhow!("parse service script {}: {e}", script_path.display()))?;

    let dry_run = matches!(action, ServiceAction::Plan);
    if let ServiceAction::Run { ack } = &action {
        if !ack {
            return Err(anyhow!(
                "`service run` sends real requests; re-run with --ack (or use `service plan`)"
            ));
        }
    }

    let mut drv = open_diag_driver(reg, driver, store)?;
    let transport = drv
        .diag()
        .ok_or_else(|| anyhow!("driver '{driver}' does not expose a diagnostic transport"))?;
    let mut diag = CoreDiag::new(transport, timeout_ms);

    let opts = RunOptions {
        dry_run,
        skip_waits: false,
    };
    // The CLI ships no OEM seed/key provider; a secured script errors cleanly.
    let report = run_script(&mut diag, &script, None, &opts).map_err(|e| anyhow!("{e}"))?;

    println!(
        "Service '{}': {:?}{}",
        report.script_id,
        report.outcome,
        if report.security_unlocked {
            " (security unlocked)"
        } else {
            ""
        }
    );
    for s in &report.steps {
        println!(
            "  #{:<2} {:<20} {:?}  {}",
            s.index, s.op, s.status, s.detail
        );
    }
    Ok(())
}

/// Resolve a user-supplied value for `name`: a number (decimal or `0x` hex), or
/// an enum option label declared for that parameter.
fn resolve_coding_value(def: &opentune_coding::CodingDef, name: &str, value: &str) -> Result<u64> {
    let v = value.trim();
    let num = if let Some(hex) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        v.parse::<u64>().ok()
    };
    if let Some(n) = num {
        return Ok(n);
    }
    // Not numeric: try to match an enum option label.
    let param = def
        .param(name)
        .ok_or_else(|| anyhow!("unknown parameter '{name}'"))?;
    param
        .options
        .iter()
        .find(|o| o.label.eq_ignore_ascii_case(v))
        .map(|o| o.value)
        .ok_or_else(|| {
            anyhow!("value '{value}' is neither a number nor a known option of '{name}'")
        })
}

/// Print one coding's blocks as `DID  hex`.
fn print_coding_blocks(coding: &opentune_coding::Coding) {
    for b in &coding.blocks {
        let hex: String = b.bytes.iter().map(|x| format!("{x:02X}")).collect();
        println!("  0x{:04X}  {}", b.did, hex);
    }
}

/// Print a write report's plan (byte + parameter changes) and its outcome.
fn print_coding_report(rep: &opentune_coding::WriteReport) {
    if rep.security_required {
        println!("note: this module requires SecurityAccess (0x27) to be unlocked first");
    }
    if rep.param_changes.is_empty() {
        println!("No parameter changes.");
    } else {
        println!("Parameter changes:");
        for pc in &rep.param_changes {
            println!(
                "  {:<20} {} ({}) -> {} ({})",
                pc.name, pc.old.0, pc.old.1, pc.new.0, pc.new.1
            );
        }
    }
    if !rep.byte_changes.is_empty() {
        println!("Byte changes:");
        for bc in &rep.byte_changes {
            println!(
                "  0x{:04X}[{}] 0x{:02X} -> 0x{:02X}",
                bc.did, bc.byte, bc.old, bc.new
            );
        }
    }
    match &rep.outcome {
        opentune_coding::WriteOutcome::Planned => {
            println!("Outcome: planned (dry run, nothing written)")
        }
        opentune_coding::WriteOutcome::NoChange => println!("Outcome: no change (nothing written)"),
        opentune_coding::WriteOutcome::Verified => println!("Outcome: written and verified"),
        opentune_coding::WriteOutcome::Unverified => println!("Outcome: written (verify skipped)"),
        opentune_coding::WriteOutcome::VerifyFailed { did } => {
            println!("Outcome: VERIFY FAILED for block 0x{did:04X}")
        }
        opentune_coding::WriteOutcome::WriteError {
            uncertain_did,
            message,
        } => println!(
            "Outcome: WRITE ERROR at block 0x{uncertain_did:04X} ({message}); backup retained"
        ),
    }
}

/// Dispatch the `coding` subcommand: load the def, obtain a transport from the
/// selected driver, wrap it in [`opentune_coding::CoreDiag`] and run the op.
fn run_coding(reg: &Registry, action: CodingAction) -> Result<()> {
    use opentune_coding::{read_coding, set, write_coding, CoreDiag, WriteOptions};

    let conn = match &action {
        CodingAction::Read { conn }
        | CodingAction::Decode { conn }
        | CodingAction::Set { conn, .. }
        | CodingAction::Write { conn, .. } => conn,
    };
    let def = load_coding_def(conn)?;
    let timeout = conn.timeout_ms;
    let mut drv = open_coding_driver(reg, conn)?;
    let transport = drv.diag().ok_or_else(|| {
        anyhow!(
            "driver '{}' does not expose a diagnostic transport for coding (try --driver mock --store <file>)",
            conn.driver
        )
    })?;
    let mut diag = CoreDiag::new(transport, timeout);

    match action {
        CodingAction::Read { .. } => {
            let coding = read_coding(&mut diag, &def).map_err(|e| anyhow!("{e}"))?;
            println!("Coding for {} ({}):", def.name, def.id);
            print_coding_blocks(&coding);
        }
        CodingAction::Decode { .. } => {
            let coding = read_coding(&mut diag, &def).map_err(|e| anyhow!("{e}"))?;
            let params = opentune_coding::decode(&def, &coding).map_err(|e| anyhow!("{e}"))?;
            println!("Decoded coding for {} ({}):", def.name, def.id);
            for p in params {
                println!("  {:<20} {:>6}  {}", p.name, p.value, p.label);
            }
        }
        CodingAction::Set { name, value, .. } => {
            let val = resolve_coding_value(&def, &name, &value)?;
            // Preview only: read fresh as the baseline, apply the edit, dry-run.
            let baseline = read_coding(&mut diag, &def).map_err(|e| anyhow!("{e}"))?;
            let desired = set(&def, &baseline, &name, val).map_err(|e| anyhow!("{e}"))?;
            let opts = WriteOptions {
                dry_run: true,
                ..Default::default()
            };
            let rep = write_coding(&mut diag, &def, &baseline, &desired, &opts)
                .map_err(|e| anyhow!("{e}"))?;
            println!("Planned change: {name} = {value}");
            print_coding_report(&rep);
        }
        CodingAction::Write {
            name,
            value,
            dry_run,
            ..
        } => {
            let val = resolve_coding_value(&def, &name, &value)?;
            let baseline = read_coding(&mut diag, &def).map_err(|e| anyhow!("{e}"))?;
            let desired = set(&def, &baseline, &name, val).map_err(|e| anyhow!("{e}"))?;
            let opts = WriteOptions {
                dry_run,
                ..Default::default()
            };
            let rep = write_coding(&mut diag, &def, &baseline, &desired, &opts)
                .map_err(|e| anyhow!("{e}"))?;
            print_coding_report(&rep);
        }
    }
    Ok(())
}

/// Parsed flags for `container unpack`, grouped to keep the dispatcher tidy.
struct ContainerUnpackArgs {
    input: PathBuf,
    outdir: Option<PathBuf>,
    frf_key: Option<PathBuf>,
    aes_key: Option<String>,
    aes_iv: Option<String>,
    xor_key: Option<String>,
    sgo_table: Option<PathBuf>,
    sgo_cipher: SgoCipherArg,
}

/// Decode a VAG flash container and report (and optionally extract) its segments.
fn container_unpack(args: ContainerUnpackArgs) -> Result<()> {
    let bytes =
        std::fs::read(&args.input).with_context(|| format!("read {}", args.input.display()))?;
    let keys = build_container_keys(&args)?;

    let unpacked = opentune_container::unpack(&bytes, &keys)
        .map_err(|e| anyhow!("decode {}: {e}", args.input.display()))?;

    println!("Container : {:?}", unpacked.kind);
    if !unpacked.members.is_empty() {
        println!("Members   : {}", unpacked.members.join(", "));
    }
    if unpacked.box_codes.is_empty() {
        println!("Box-codes : (none declared)");
    } else {
        println!("Box-codes : {}", unpacked.box_codes.join(", "));
    }
    println!("Segments  : {}", unpacked.segments.len());

    if let Some(dir) = &args.outdir {
        std::fs::create_dir_all(dir).with_context(|| format!("create outdir {}", dir.display()))?;
    }

    let mut flash_ready = 0usize;
    let mut not_ready = 0usize;
    for seg in &unpacked.segments {
        // A segment is only a usable flash image when it decoded fully AND the
        // container told us where it belongs. Anything else is ambiguous.
        let ready = seg.decoded && seg.address.is_some();
        if ready {
            flash_ready += 1;
        } else {
            not_ready += 1;
        }
        let addr = match seg.address {
            Some(a) => format!("0x{a:08X}"),
            None => "(no address)".to_string(),
        };
        let status = if ready {
            "flash-ready".to_string()
        } else {
            let mut why = Vec::new();
            if !seg.decoded {
                why.push("not fully decoded");
            }
            if seg.address.is_none() {
                why.push("no confirmed address");
            }
            format!("NOT FLASH-READY ({})", why.join(", "))
        };
        println!(
            "  {:<24} {:>12}  {:>9} bytes  {:?}/{}  [{:?}]  {}",
            seg.id,
            addr,
            seg.data.len(),
            seg.compression,
            seg.method,
            seg.source,
            status,
        );
        if let Some(note) = &seg.note {
            println!("      note: {note}");
        }

        if let Some(dir) = &args.outdir {
            // Partial/ambiguous segments get a distinct extension so a decoded
            // flash image can never be mistaken for an incomplete one.
            let ext = if ready { "bin" } else { "partial.bin" };
            let fname = format!("{}.{ext}", sanitize_segment_id(&seg.id));
            let path = dir.join(&fname);
            std::fs::write(&path, &seg.data)
                .with_context(|| format!("write {}", path.display()))?;
            println!("      wrote {}", path.display());
        }
    }

    println!("\n{flash_ready} flash-ready, {not_ready} not flash-ready.");
    if not_ready > 0 {
        println!(
            "Note: 'not flash-ready' segments are best-effort partial output \
             (missing key, unconfirmed address, or incomplete decode) — do NOT flash them."
        );
    }
    Ok(())
}

/// Assemble caller-supplied [`Keys`] from the `container unpack` flags. No key
/// material is embedded; every value here comes from a flag the user passed.
fn build_container_keys(args: &ContainerUnpackArgs) -> Result<Keys> {
    let mut keys = Keys::default();

    if let Some(p) = &args.frf_key {
        keys.frf_key =
            Some(std::fs::read(p).with_context(|| format!("read frf key {}", p.display()))?);
    }

    keys.block_cipher = if let Some(hex) = &args.aes_key {
        let key = parse_hex_fixed::<16>(hex).context("--aes-key")?;
        let iv = match &args.aes_iv {
            Some(h) => parse_hex_fixed::<16>(h).context("--aes-iv")?,
            None => [0u8; 16],
        };
        BlockCipher::Aes128Cbc { key, iv }
    } else {
        if args.aes_iv.is_some() {
            return Err(anyhow!("--aes-iv requires --aes-key"));
        }
        match &args.xor_key {
            Some(s) => BlockCipher::RepeatingXor(s.clone().into_bytes()),
            None => BlockCipher::None,
        }
    };

    if let Some(p) = &args.sgo_table {
        let table = std::fs::read(p).with_context(|| format!("read sgo table {}", p.display()))?;
        let arr: [u8; 256] = table.as_slice().try_into().map_err(|_| {
            anyhow!(
                "--sgo-table must be exactly 256 bytes ({} had {})",
                p.display(),
                table.len()
            )
        })?;
        keys.sgo_table = Some(arr);
    }

    keys.sgo_cipher = match args.sgo_cipher {
        SgoCipherArg::Progressive => SgoCipher::Progressive,
        SgoCipherArg::Feedback => SgoCipher::Feedback,
    };

    Ok(keys)
}

/// Parse exactly `N` bytes from a hex string (no `0x` prefix, no separators).
fn parse_hex_fixed<const N: usize>(s: &str) -> Result<[u8; N]> {
    let s = s.strip_prefix("0x").unwrap_or(s).trim();
    if s.len() != N * 2 {
        return Err(anyhow!(
            "expected {N} bytes ({} hex chars), got {}",
            N * 2,
            s.len()
        ));
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .with_context(|| format!("bad hex byte at position {i}"))?;
    }
    Ok(out)
}

/// Make a container-local segment id safe to use as a file name.
fn sanitize_segment_id(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "segment".to_string()
    } else {
        cleaned
    }
}

/// Cluster the byte differences between two images into candidate map regions,
/// merging changes separated by fewer than `gap` unchanged bytes.
fn find_maps(a: &[u8], b: &[u8], gap: usize, min_len: usize) -> Vec<(usize, usize)> {
    let n = a.len().min(b.len());
    let mut regions = Vec::new();
    let mut i = 0;
    while i < n {
        if a[i] != b[i] {
            let start = i;
            let mut end = i + 1;
            let mut since_change = 0usize;
            let mut j = i + 1;
            while j < n && since_change <= gap {
                if a[j] != b[j] {
                    end = j + 1;
                    since_change = 0;
                } else {
                    since_change += 1;
                }
                j += 1;
            }
            regions.push((start, end));
            i = end;
        } else {
            i += 1;
        }
    }
    regions
        .into_iter()
        .filter(|(s, e)| e - s >= min_len)
        .collect()
}

/// Hint whether a region's size looks like a table of 16-bit cells.
fn guess_stride(len: usize) -> String {
    if len >= 4 && len.is_multiple_of(2) {
        let cells = len / 2;
        for &w in &[16usize, 12, 10, 8] {
            if cells.is_multiple_of(w) {
                return format!("  (~{}x{} u16?)", cells / w, w);
            }
        }
        return format!("  (~{cells} u16 cells?)");
    }
    String::new()
}

/// Summarise the byte differences between two images as contiguous ranges.
fn diff(a: &[u8], b: &[u8]) {
    if a.len() != b.len() {
        println!("size differs: {} vs {} bytes", a.len(), b.len());
    }
    let n = a.len().min(b.len());
    let mut changed = 0usize;
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n {
        if a[i] != b[i] {
            let start = i;
            while i < n && a[i] != b[i] {
                changed += 1;
                i += 1;
            }
            ranges.push((start, i));
        } else {
            i += 1;
        }
    }
    println!(
        "{} differing bytes in {} region(s) over {} compared bytes",
        changed,
        ranges.len(),
        n
    );
    for (s, e) in ranges.iter().take(40) {
        println!("  0x{:08X}..0x{:08X}  ({} bytes)", s, e, e - s);
    }
    if ranges.len() > 40 {
        println!("  … and {} more regions", ranges.len() - 40);
    }
}

/// Read one map cell's RAW value at a byte offset, honoring type + byte order.
fn read_cell(
    img: &[u8],
    off: usize,
    dt: opentune_core::ecudef::DataType,
    endian: opentune_core::ecudef::Endian,
) -> Option<f64> {
    use opentune_core::ecudef::{DataType as T, Endian as E};
    let w = dt.byte_len() as usize;
    let b = img.get(off..off + w)?;
    let le = matches!(endian, E::Little);
    let v = match dt {
        T::U8 => b[0] as f64,
        T::S8 => (b[0] as i8) as f64,
        T::U16 => {
            let a = [b[0], b[1]];
            (if le {
                u16::from_le_bytes(a)
            } else {
                u16::from_be_bytes(a)
            }) as f64
        }
        T::S16 => {
            let a = [b[0], b[1]];
            (if le {
                i16::from_le_bytes(a)
            } else {
                i16::from_be_bytes(a)
            }) as f64
        }
        T::U32 => {
            let a = [b[0], b[1], b[2], b[3]];
            (if le {
                u32::from_le_bytes(a)
            } else {
                u32::from_be_bytes(a)
            }) as f64
        }
        T::S32 => {
            let a = [b[0], b[1], b[2], b[3]];
            (if le {
                i32::from_le_bytes(a)
            } else {
                i32::from_be_bytes(a)
            }) as f64
        }
        T::F32 => {
            let a = [b[0], b[1], b[2], b[3]];
            (if le {
                f32::from_le_bytes(a)
            } else {
                f32::from_be_bytes(a)
            }) as f64
        }
    };
    Some(v)
}

/// Decode every cell of a map (physical values) from an image, or `None` if the
/// map runs off the end of the image.
fn decode_map_phys(m: &opentune_core::ecudef::MapDef, img: &[u8]) -> Option<Vec<f64>> {
    let n = (m.rows as usize) * (m.cols as usize);
    let w = m.data_type.byte_len() as usize;
    let base = m.address as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let raw = read_cell(img, base + i * w, m.data_type, m.endian)?;
        out.push(m.scaling.to_physical(raw));
    }
    Some(out)
}

/// Compare two images map-by-map through a definition's MapDefs.
fn compare_maps(d: &opentune_core::ecudef::EcuDef, a: &[u8], b: &[u8], only: Option<&str>) {
    println!(
        "Comparing {} map(s) of '{}' ({} → {})",
        d.maps.len(),
        d.id,
        human_len(a.len()),
        human_len(b.len())
    );
    let mut any_changed = false;
    for m in &d.maps {
        if let Some(sub) = only {
            if !m.name.to_lowercase().contains(&sub.to_lowercase()) {
                continue;
            }
        }
        let (Some(pa), Some(pb)) = (decode_map_phys(m, a), decode_map_phys(m, b)) else {
            println!("  {:<32} [out of range — skipped]", m.name);
            continue;
        };
        let mut changed = 0usize;
        let mut max_abs = 0f64;
        let mut sum_delta = 0f64;
        for (x, y) in pa.iter().zip(pb.iter()) {
            let dlt = y - x;
            if dlt.abs() > 1e-9 {
                changed += 1;
                sum_delta += dlt;
                if dlt.abs() > max_abs {
                    max_abs = dlt.abs();
                }
            }
        }
        let unit = if m.unit.is_empty() {
            String::new()
        } else {
            format!(" {}", m.unit)
        };
        if changed == 0 {
            if only.is_some() {
                println!("  {:<32} unchanged", m.name);
            }
            continue;
        }
        any_changed = true;
        let avg = sum_delta / changed as f64;
        println!(
            "  {:<32} {}x{}  {} cell(s) changed  Δmax {:+.3}{u}  Δavg {:+.3}{u}",
            m.name,
            m.rows,
            m.cols,
            changed,
            max_abs,
            avg,
            u = unit
        );
        // Full grid for a single explicitly-requested map.
        if only.is_some() {
            print_grid(m, &pa, &pb);
        }
    }
    if !any_changed && only.is_none() {
        println!("  No mapped cells differ.");
    }
}

/// Print a before→after grid for one map.
fn print_grid(m: &opentune_core::ecudef::MapDef, pa: &[f64], pb: &[f64]) {
    let cols = m.cols as usize;
    for r in 0..m.rows as usize {
        let mut line = String::from("      ");
        for c in 0..cols {
            let i = r * cols + c;
            let (x, y) = (pa[i], pb[i]);
            if (y - x).abs() > 1e-9 {
                line.push_str(&format!("{:>7.1}>{:<7.1}", x, y));
            } else {
                line.push_str(&format!("{:>7.1}{:<8}", x, ""));
            }
        }
        println!("{}", line.trim_end());
    }
}

/// Resolve an ecudef path to the list of *.json files to validate: a single
/// file is returned as-is; a directory yields every *.json inside it (sorted).
fn collect_ecudef_files(path: &std::path::Path) -> Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    if path.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(path)
            .with_context(|| format!("read dir {}", path.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .filter(|p| {
                let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                !opentune_core::ecudef::is_scaffolding_file(name)
            })
            .collect();
        files.sort();
        return Ok(files);
    }
    Err(anyhow!(
        "{} is neither a file nor a directory",
        path.display()
    ))
}

fn human_len(n: usize) -> String {
    if n >= 1024 * 1024 {
        format!("{} MiB", n / (1024 * 1024))
    } else {
        format!("{} KiB", n / 1024)
    }
}
