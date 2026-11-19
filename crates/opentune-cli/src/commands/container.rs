//! `container unpack` / `container identify`: decode VAG / Ford / BMW flash
//! containers (VAG ones with caller-supplied keys).
use crate::cli::SgoCipherArg;
use anyhow::{anyhow, bail, Context, Result};
use opentune_container::bmw::{self, ProcessClass, SweName};
use opentune_container::{BlockCipher, InputFormat, Keys, SgoCipher, Unpacked};
use std::path::{Path, PathBuf};

/// Parsed flags for `container unpack`, grouped to keep the dispatcher tidy.
pub struct ContainerUnpackArgs {
    pub input: PathBuf,
    pub pair: Option<PathBuf>,
    pub flat: Option<PathBuf>,
    pub fill: String,
    pub outdir: Option<PathBuf>,
    pub frf_key: Option<PathBuf>,
    pub aes_key: Option<String>,
    pub aes_iv: Option<String>,
    pub xor_key: Option<String>,
    pub sgo_table: Option<PathBuf>,
    pub sgo_cipher: SgoCipherArg,
}

/// Largest file `container unpack` / `identify` reads into memory.
const MAX_INPUT: u64 = 512 * 1024 * 1024;

fn read_input(path: &Path) -> Result<Vec<u8>> {
    let len = std::fs::metadata(path)
        .with_context(|| format!("read {}", path.display()))?
        .len();
    if len > MAX_INPUT {
        bail!("{} is {len} bytes (limit {MAX_INPUT})", path.display());
    }
    std::fs::read(path).with_context(|| format!("read {}", path.display()))
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A located BMW SWE pair.
struct SwePair {
    name: Option<SweName>,
    xml_path: PathBuf,
    bin_path: Option<PathBuf>,
}

/// Decide whether `input` is (half of) a BMW SWE and find the other half.
/// `Ok(None)` = not a BMW SWE. A CAFD `.caf` file is refused here.
fn locate_swe(input: &Path, bytes: &[u8], pair: Option<&Path>) -> Result<Option<SwePair>> {
    let name = SweName::parse(&file_name(input));
    let is_xml = opentune_container::detect(bytes) == InputFormat::BmwSweXml;
    if let Some(n) = &name {
        if n.class == ProcessClass::Cafd || n.kind == "caf" {
            bail!(
                "{}: BMW CAFD coding data (.caf). Its format is not publicly documented, \
                 so OpenTune does not decode it.",
                input.display()
            );
        }
    }
    let named_half = name
        .as_ref()
        .is_some_and(|n| n.kind == "bin" || n.kind == "xml");
    if !is_xml && !named_half {
        if pair.is_some() {
            bail!("--pair only applies to BMW SWE containers");
        }
        return Ok(None);
    }
    let input_is_xml = is_xml || name.as_ref().is_some_and(|n| n.kind == "xml");
    let other = match pair {
        Some(p) => Some(p.to_path_buf()),
        None => name
            .as_ref()
            .map(|n| input.with_file_name(n.sibling(if input_is_xml { "bin" } else { "xml" }))),
    };
    let other = other.filter(|p| p.is_file());
    Ok(Some(if input_is_xml {
        SwePair {
            name,
            xml_path: input.to_path_buf(),
            bin_path: other,
        }
    } else {
        let xml_path = other.ok_or_else(|| {
            anyhow!(
                "{}: BMW SWE flash data needs its .xml descriptor: put the sibling \
                 `{}` next to it or pass --pair <file.xml>",
                input.display(),
                name.as_ref().map(|n| n.sibling("xml")).unwrap_or_default()
            )
        })?;
        SwePair {
            name,
            xml_path,
            bin_path: Some(input.to_path_buf()),
        }
    }))
}

fn print_swe_header(name: Option<&SweName>, d: &bmw::SweDescriptor) {
    if let Some(n) = name {
        println!(
            "SWE       : {} {} version {} ({})",
            n.class_raw,
            n.id,
            n.version,
            n.class.describe()
        );
    }
    if let Some((class, id, ver)) = d.ident() {
        println!(
            "Ident     : class {} id {} version {}",
            class.unwrap_or("?"),
            id.unwrap_or("?"),
            ver.unwrap_or("?")
        );
        if let (Some(n), Some(id)) = (name, id) {
            if !id.eq_ignore_ascii_case(&n.id) {
                println!(
                    "  ! descriptor identifier {id} differs from the file name ({})",
                    n.id
                );
            }
        }
    }
    for note in d.notes() {
        println!("Note      : {note}");
    }
}

/// Decode a flash container and report (and optionally extract) its segments.
pub fn container_unpack(args: ContainerUnpackArgs) -> Result<()> {
    let bytes = read_input(&args.input)?;
    let fill = u8::from_str_radix(args.fill.trim().trim_start_matches("0x"), 16)
        .with_context(|| format!("--fill '{}' is not one hex byte", args.fill))?;

    let unpacked: Unpacked = match locate_swe(&args.input, &bytes, args.pair.as_deref())? {
        Some(p) => {
            let bin_path = p.bin_path.ok_or_else(|| {
                anyhow!(
                    "{}: BMW SWE descriptor without its .bin: put the sibling next to it \
                     or pass --pair <file.bin> (`container identify` reads a descriptor alone)",
                    p.xml_path.display()
                )
            })?;
            let xml = String::from_utf8(read_input(&p.xml_path)?)
                .with_context(|| format!("{} is not UTF-8 XML", p.xml_path.display()))?;
            let bin = read_input(&bin_path)?;
            let swe = bmw::unpack_pair(&xml, &bin)
                .map_err(|e| anyhow!("decode {}: {e}", p.xml_path.display()))?;
            println!("Descriptor: {}", p.xml_path.display());
            println!("Data      : {}", bin_path.display());
            print_swe_header(p.name.as_ref(), &swe.descriptor);
            swe.into_unpacked()
        }
        None => {
            let keys = build_container_keys(&args)?;
            opentune_container::unpack(&bytes, &keys)
                .map_err(|e| anyhow!("decode {}: {e}", args.input.display()))?
        }
    };

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

    if let Some(path) = &args.flat {
        let img = opentune_container::flatten(&unpacked.segments, fill)
            .map_err(|e| anyhow!("flat image: {e}"))?;
        std::fs::write(path, &img.data).with_context(|| format!("write {}", path.display()))?;
        println!(
            "Flat image: {} bytes at 0x{:08X}..0x{:08X} (gaps 0x{fill:02X}) -> {}",
            img.data.len(),
            img.base,
            u64::from(img.base) + img.data.len() as u64,
            path.display()
        );
    }
    Ok(())
}

/// `container identify`: name the format; for a BMW SWE also summarise the
/// descriptor without decompressing anything.
pub fn container_identify(input: &Path, pair: Option<&Path>) -> Result<()> {
    let bytes = read_input(input)?;
    let swe = match locate_swe(input, &bytes, pair) {
        Ok(s) => s,
        Err(e) => {
            // A CAFD (or a .bin without its descriptor) is still identifiable.
            if let Some(n) = SweName::parse(&file_name(input)) {
                println!("Format    : BMW PSdZData SWE ({})", n.kind);
                println!(
                    "SWE       : {} {} version {} ({})",
                    n.class_raw,
                    n.id,
                    n.version,
                    n.class.describe()
                );
                println!("Size      : {} bytes", bytes.len());
                println!("Note      : {e}");
                return Ok(());
            }
            return Err(e);
        }
    };
    let Some(p) = swe else {
        println!("Format    : {:?}", opentune_container::detect(&bytes));
        println!("Size      : {} bytes", bytes.len());
        println!("(content sniffing; run `container unpack` to decode)");
        return Ok(());
    };
    println!("Format    : BMW PSdZData SWE flash container");
    println!("Descriptor: {}", p.xml_path.display());
    match &p.bin_path {
        Some(b) => println!(
            "Data      : {} ({} bytes)",
            b.display(),
            std::fs::metadata(b).map(|m| m.len()).unwrap_or(0)
        ),
        None => println!("Data      : (no .bin found)"),
    }
    let xml = String::from_utf8(read_input(&p.xml_path)?)
        .with_context(|| format!("{} is not UTF-8 XML", p.xml_path.display()))?;
    let d = bmw::parse_descriptor(&xml).map_err(|e| anyhow!("{}: {e}", p.xml_path.display()))?;
    if let Some(s) = &d.short_name {
        println!("Name      : {s}");
    }
    print_swe_header(p.name.as_ref(), &d);
    println!("Segments  : {}", d.segments.len());
    for s in &d.segments {
        let codec = match s.codec() {
            Ok(c) => format!("{c:?}"),
            Err(why) => format!("REFUSED: {why}"),
        };
        println!(
            "  {:<24} src 0x{:08X}..=0x{:08X}  target 0x{:08X}..=0x{:08X}  {}",
            s.id(),
            s.source_start,
            s.source_end,
            s.target_start,
            s.target_end,
            codec
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
