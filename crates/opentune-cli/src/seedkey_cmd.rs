//! `opentune seedkey`: run a user-supplied seed/key script offline.
//!
//! OpenTune ships no seed/key algorithm or script. This command only runs
//! what the user gives it — typed in, from a file, or from the user's own
//! flash container.
use anyhow::{anyhow, bail, Context, Result};
use opentune_sa2::vm::{self, Step};
use opentune_sa2::{assemble_text, parse_hex, Program};
use std::path::PathBuf;

#[derive(clap::Subcommand)]
pub enum SeedkeyAction {
    /// Run a VAG SA2 seed/key script over a 4-byte seed, or disassemble it.
    ///
    /// The script comes from `--script` (hex), `--script-file` (raw bytes, hex
    /// text, or SA2 assembler source) or `--container` (your own FRF / ZIP /
    /// ODX / SGO: the ODX `SECURITY` block with method `SA2`, or the SGO SA2
    /// field). Offline only; nothing is sent to a vehicle.
    Sa2(Sa2Args),
}

#[derive(clap::Args)]
#[command(group(
    clap::ArgGroup::new("source")
        .required(true)
        .args(["script", "script_file", "container"])
))]
pub struct Sa2Args {
    /// The script as hex (spaces, commas and `0x` prefixes are ignored).
    #[arg(long, value_name = "HEX")]
    pub script: Option<String>,
    /// A file holding the script: raw bytes, hex text, or assembler source
    /// (`loop 3`, `rsl`, `bcc label`, `eor 0x...`, `finish`, ...).
    #[arg(long, value_name = "FILE")]
    pub script_file: Option<PathBuf>,
    /// Take the script from a flash container (FRF / ZIP / ODX / ODX-F / SGO).
    #[arg(long, value_name = "FILE")]
    pub container: Option<PathBuf>,
    /// FRF rolling-XOR key material (`frf.key`), needed only to open an FRF.
    #[arg(long, value_name = "FILE", requires = "container")]
    pub frf_key: Option<PathBuf>,
    /// Which of the container's SA2 scripts to use (see `container unpack`).
    #[arg(long, default_value_t = 0)]
    pub index: usize,
    /// The 4-byte seed from the ECU (`67 11 <seed>`), as hex.
    #[arg(long, value_name = "HEX")]
    pub seed: Option<String>,
    /// Print a disassembly listing (offsets, bytes, loop nesting, branch
    /// targets, lints).
    #[arg(long)]
    pub disasm: bool,
    /// Print every executed instruction with the register and carry after it.
    #[arg(long, requires = "seed")]
    pub trace: bool,
    /// Step budget: abort a script that runs longer than this.
    #[arg(long, default_value_t = vm::MAX_STEPS)]
    pub max_steps: u64,
}

pub fn run(action: SeedkeyAction) -> Result<()> {
    match action {
        SeedkeyAction::Sa2(a) => sa2(a),
    }
}

fn sa2(a: Sa2Args) -> Result<()> {
    if a.seed.is_none() && !a.disasm {
        bail!("nothing to do: give --seed to compute a key, and/or --disasm to list the script");
    }
    let (script, origin) = load_script(&a)?;
    println!("SA2 script : {} bytes ({origin})", script.len());

    if a.disasm {
        match Program::parse(&script) {
            Ok(p) => print!("{}", p.listing()),
            Err(e) => println!("; linear disassembly stopped: {e}"),
        }
    }

    let Some(seed_hex) = &a.seed else {
        return Ok(());
    };
    let seed = parse_hex(seed_hex).map_err(|e| anyhow!("--seed: {e}"))?;
    let seed: [u8; 4] = seed
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("--seed must be exactly 4 bytes (got {})", seed.len()))?;
    let seed = u32::from_be_bytes(seed);

    let mut print_step = |s: &Step| {
        println!(
            "  {:04X}  {:<18} reg={:08X} carry={} loops={}",
            s.offset,
            s.op.to_string(),
            s.register,
            u8::from(s.carry),
            s.loop_depth
        );
    };
    let trace: Option<&mut dyn FnMut(&Step)> = if a.trace { Some(&mut print_step) } else { None };
    let out = vm::run_with(&script, seed, a.max_steps, trace).map_err(|e| anyhow!("SA2: {e}"))?;
    println!("Seed       : {seed:08X}");
    println!("Key        : {:08X}", out.key);
    println!("Steps      : {}", out.steps);
    Ok(())
}

/// Read an SA2 script file: raw bytes, hex text, or assembler source.
/// Returns the bytes and a short description of where they came from.
pub(crate) fn script_from_file(path: &std::path::Path) -> Result<(Vec<u8>, String)> {
    let raw = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let origin = format!("from {}", path.display());
    let text = std::str::from_utf8(&raw)
        .ok()
        .filter(|t| t.chars().all(|c| !c.is_control() || c.is_whitespace()));
    match text {
        None => Ok((raw, format!("{origin}, raw bytes"))),
        Some(text) => match parse_hex(text) {
            Ok(bytes) => Ok((bytes, format!("{origin}, hex"))),
            Err(_) => assemble_text(text)
                .map(|b| (b, format!("{origin}, assembled")))
                .map_err(|e| anyhow!("{}: neither hex nor SA2 assembler: {e}", path.display())),
        },
    }
}

/// Build the handler for a `--seedkey sa2:<hex>` / `sa2:@<file>` id, or
/// `None` when `id` is not of that form.
pub(crate) fn sa2_from_id(id: &str) -> Option<Result<opentune_sa2::Sa2SeedKey>> {
    let spec = id.strip_prefix("sa2:")?;
    let bytes = match spec.strip_prefix('@') {
        Some(path) => script_from_file(std::path::Path::new(path)).map(|(b, _)| b),
        None => parse_hex(spec).map_err(|e| anyhow!("seed-key id '{id}': {e}")),
    };
    Some(bytes.map(opentune_sa2::Sa2SeedKey::new))
}

/// The script bytes plus a short description of where they came from.
fn load_script(a: &Sa2Args) -> Result<(Vec<u8>, String)> {
    if let Some(hex) = &a.script {
        let bytes = parse_hex(hex).map_err(|e| anyhow!("--script: {e}"))?;
        return Ok((bytes, "from --script".into()));
    }
    if let Some(path) = &a.script_file {
        return script_from_file(path);
    }
    let Some(path) = &a.container else {
        bail!("give --script, --script-file or --container");
    };
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut keys = opentune_container::Keys::default();
    if let Some(k) = &a.frf_key {
        keys.frf_key =
            Some(std::fs::read(k).with_context(|| format!("read frf key {}", k.display()))?);
    }
    let unpacked = opentune_container::unpack(&bytes, &keys)
        .map_err(|e| anyhow!("decode {}: {e}", path.display()))?;
    let scripts = unpacked.sa2_scripts;
    if scripts.is_empty() {
        bail!(
            "{}: no SA2 script found (looked for an ODX SECURITY block with SECURITY-METHOD \
             'SA2', and the SGO header's SA2 field)",
            path.display()
        );
    }
    let s = scripts.get(a.index).ok_or_else(|| {
        anyhow!(
            "--index {} out of range: {} has {} SA2 script(s)",
            a.index,
            path.display(),
            scripts.len()
        )
    })?;
    let session = s
        .session
        .as_deref()
        .map(|n| format!(", session {n}"))
        .unwrap_or_default();
    let origin = format!(
        "from {} #{} of {}: {}{session}",
        path.display(),
        a.index,
        scripts.len(),
        s.field
    );
    Ok((s.bytes.clone(), origin))
}

#[cfg(test)]
mod tests {
    use super::sa2_from_id;
    use opentune_core::protocol::seedkey::SeedKey;

    #[test]
    fn sa2_seedkey_ids() {
        assert!(sa2_from_id("udssim-test").is_none());
        let sk = sa2_from_id("sa2:87FFFFFFFF4C").unwrap().unwrap();
        assert_eq!(
            sk.compute(&[0, 0, 0, 1], 0x11).unwrap(),
            vec![0xFF, 0xFF, 0xFF, 0xFE]
        );
        assert!(sa2_from_id("sa2:8").unwrap().is_err());
        assert!(sa2_from_id("sa2:@/nonexistent/sa2-script")
            .unwrap()
            .is_err());
    }
}
