//! `container unpack`: decode VAG flash containers with caller-supplied keys.
use crate::cli::SgoCipherArg;
use anyhow::{anyhow, Context, Result};
use opentune_container::{BlockCipher, Keys, SgoCipher};
use std::path::PathBuf;

/// Parsed flags for `container unpack`, grouped to keep the dispatcher tidy.
pub struct ContainerUnpackArgs {
    pub input: PathBuf,
    pub outdir: Option<PathBuf>,
    pub frf_key: Option<PathBuf>,
    pub aes_key: Option<String>,
    pub aes_iv: Option<String>,
    pub xor_key: Option<String>,
    pub sgo_table: Option<PathBuf>,
    pub sgo_cipher: SgoCipherArg,
}

/// Decode a VAG flash container and report (and optionally extract) its segments.
pub fn container_unpack(args: ContainerUnpackArgs) -> Result<()> {
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
    for (i, s) in unpacked.sa2_scripts.iter().enumerate() {
        println!(
            "SA2 script: #{i} {} bytes from {}{} (run it: `opentune seedkey sa2 --container {} --index {i} --seed ...`)",
            s.bytes.len(),
            s.field,
            s.session
                .as_deref()
                .map(|n| format!(" in session {n}"))
                .unwrap_or_default(),
            args.input.display(),
        );
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
