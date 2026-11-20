//! Offline commands over local image files: diff, map finding, compare.
use crate::mapfind_cmd;
use anyhow::{anyhow, Context, Result};
use opentune_mapfind::diff::{diff_regions, u16_shape, MapDelta, U16Shape};
use std::path::PathBuf;

/// `diff`
pub fn diff_files(a: PathBuf, b: PathBuf) -> Result<()> {
    let da = std::fs::read(&a).with_context(|| format!("read {}", a.display()))?;
    let db = std::fs::read(&b).with_context(|| format!("read {}", b.display()))?;
    diff(&da, &db);
    Ok(())
}

/// `findmaps`: one image (heuristic scan) or a stock/tuned pair (diff clustering).
pub fn findmaps(
    stock: PathBuf,
    tuned: Option<PathBuf>,
    single: mapfind_cmd::SingleArgs,
    gap: usize,
    min_len: usize,
    emit: Option<PathBuf>,
) -> Result<()> {
    let Some(tuned) = tuned else {
        return mapfind_cmd::run_single(&stock, &single, emit.as_deref());
    };
    let a = std::fs::read(&stock).with_context(|| format!("read {}", stock.display()))?;
    let b = std::fs::read(&tuned).with_context(|| format!("read {}", tuned.display()))?;
    let regions = diff_regions(&a, &b, gap, min_len);
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
    Ok(())
}

/// `compare`
pub fn compare(
    ecudefs: PathBuf,
    def: String,
    stock: PathBuf,
    tuned: PathBuf,
    map: Option<String>,
) -> Result<()> {
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
    Ok(())
}

/// Hint whether a region's size looks like a table of 16-bit cells.
fn guess_stride(len: usize) -> String {
    match u16_shape(len) {
        Some(U16Shape::Grid { rows, cols }) => format!("  (~{rows}x{cols} u16?)"),
        Some(U16Shape::Cells(cells)) => format!("  (~{cells} u16 cells?)"),
        None => String::new(),
    }
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
        let delta = MapDelta::from_pairs(pa.iter().copied().zip(pb.iter().copied()));
        let changed = delta.changed;
        let max_abs = delta.max_abs;
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
        let avg = delta.mean();
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

fn human_len(n: usize) -> String {
    if n >= 1024 * 1024 {
        format!("{} MiB", n / (1024 * 1024))
    } else {
        format!("{} KiB", n / 1024)
    }
}
