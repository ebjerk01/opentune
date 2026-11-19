//! Standalone offline checksum experiment. See README.md for range conventions.
use std::{env, fs, ops::Range, process::ExitCode, thread};

#[derive(Clone, Copy, Debug)]
enum Order {
    Little,
    Big,
}
use Order::*;

fn number(bytes: &[u8], order: Order) -> u32 {
    match order {
        Little => bytes.iter().rev().fold(0, |n, &b| n << 8 | u32::from(b)),
        Big => bytes.iter().fold(0, |n, &b| n << 8 | u32::from(b)),
    }
}

#[derive(Clone, Copy, Debug)]
enum Reduction {
    Sum,
    OnesComplement,
    TwosComplement,
    Xor,
}

#[derive(Clone, Copy, Debug)]
enum Algorithm {
    Words {
        bytes: usize,
        order: Order,
        bits: u32,
        op: Reduction,
    },
    Fletcher16,
    Adler32,
    Crc {
        bits: u32,
        poly: u32,
        init: u32,
        reflected: bool,
        xorout: u32,
    },
    Vag {
        seed_a: u16,
        seed_b: u16,
    },
}

fn mask(bits: u32) -> u32 {
    u32::MAX >> (32 - bits)
}

impl Algorithm {
    fn width(self) -> usize {
        match self {
            Self::Words { bits, .. } | Self::Crc { bits, .. } => bits as usize / 8,
            Self::Fletcher16 => 2,
            Self::Adler32 | Self::Vag { .. } => 4,
        }
    }

    fn evaluate(self, data: &[u8]) -> Option<u32> {
        match self {
            Self::Words {
                bytes,
                order,
                bits,
                op,
            } => {
                if !data.len().is_multiple_of(bytes) {
                    return None;
                }
                let words = data.chunks_exact(bytes).map(|w| number(w, order));
                let value = match op {
                    Reduction::Sum => words.fold(0u32, u32::wrapping_add),
                    Reduction::OnesComplement => !words.fold(0u32, u32::wrapping_add),
                    Reduction::TwosComplement => words.fold(0u32, u32::wrapping_add).wrapping_neg(),
                    Reduction::Xor => words.fold(0u32, |a, b| a ^ b),
                };
                Some(value & mask(bits))
            }
            Self::Fletcher16 | Self::Adler32 => {
                let (modulus, shift, mut a) = match self {
                    Self::Fletcher16 => (255, 8, 0u32),
                    _ => (65521, 16, 1),
                };
                let mut b = 0;
                for &byte in data {
                    a = (a + u32::from(byte)) % modulus;
                    b = (b + a) % modulus;
                }
                Some(b << shift | a)
            }
            Self::Crc {
                bits,
                poly,
                init,
                reflected,
                xorout,
            } => {
                let mut table = [0; 256];
                for (i, entry) in table.iter_mut().enumerate() {
                    let mut n = (i as u32) << (bits - 8);
                    for _ in 0..8 {
                        n = if n & (1 << (bits - 1)) != 0 {
                            (n << 1) ^ poly
                        } else {
                            n << 1
                        };
                    }
                    *entry = n & mask(bits);
                }
                let mut n = init;
                for &byte in data {
                    let byte = if reflected { byte.reverse_bits() } else { byte };
                    let index = ((n >> (bits - 8)) as u8 ^ byte) as usize;
                    n = (n << 8) ^ table[index];
                }
                if reflected {
                    n = n.reverse_bits() >> (32 - bits);
                }
                Some((n ^ xorout) & mask(bits))
            }
            Self::Vag { seed_a, seed_b } => vag(data, seed_a, seed_b),
        }
    }
}

fn vag(data: &[u8], mut a: u16, mut b: u16) -> Option<u32> {
    if data.is_empty() || !data.len().is_multiple_of(4) {
        return None;
    }
    for (i, chunk) in data.chunks_exact(4).enumerate() {
        let x = u16::from_le_bytes([chunk[0], chunk[1]]);
        let y = u16::from_le_bytes([chunk[2], chunk[3]]);
        if i % 2 == 0 {
            let n = u32::from(b & 15);
            a = (a ^ x).rotate_left(n);
            let carry = if n == 0 { 0 } else { a & 1 };
            b = b.wrapping_sub(y).wrapping_sub(carry) ^ a;
        } else {
            a = a.wrapping_sub(x).wrapping_add(0xdaac);
            b = (b ^ y).rotate_right(u32::from(a & 15));
        }
    }
    Some(u32::from(b ^ 0xdf9b) << 16 | u32::from(a.wrapping_add(0xdaac - 0x8631)))
}

fn algorithms() -> Vec<Algorithm> {
    let mut out = vec![Algorithm::Fletcher16, Algorithm::Adler32];
    for bytes in [1, 2, 4] {
        for order in [Little, Big] {
            if bytes == 1 && matches!(order, Big) {
                continue;
            }
            for bits in [8, 16, 32] {
                if bits < bytes as u32 * 8 {
                    continue;
                }
                for op in [
                    Reduction::Sum,
                    Reduction::OnesComplement,
                    Reduction::TwosComplement,
                    Reduction::Xor,
                ] {
                    out.push(Algorithm::Words {
                        bytes,
                        order,
                        bits,
                        op,
                    });
                }
            }
        }
    }
    for (bits, poly) in [
        (16, 0x8005),
        (16, 0x1021),
        (32, 0x04c11db7),
        (32, 0x1edc6f41),
    ] {
        for init in [0, mask(bits)] {
            for xorout in [0, mask(bits)] {
                for reflected in [false, true] {
                    out.push(Algorithm::Crc {
                        bits,
                        poly,
                        init,
                        reflected,
                        xorout,
                    });
                }
            }
        }
    }
    for (seed_a, seed_b) in [(0, 0), (0x8631, 0xefcd)] {
        out.push(Algorithm::Vag { seed_a, seed_b });
    }
    out
}

fn search(
    images: &[Vec<u8>],
    region: Range<usize>,
    slots: Range<usize>,
    jobs: usize,
) -> Vec<String> {
    let configs = algorithms();
    let chunk_size = configs.len().div_ceil(jobs);
    thread::scope(|scope| {
        let mut threads = Vec::new();
        for chunk in configs.chunks(chunk_size) {
            let region = region.clone();
            let slots = slots.clone();
            threads.push(scope.spawn(move || {
                let mut matches = Vec::new();
                for &algo in chunk {
                    let width = algo.width();
                    let Some(first) = algo.evaluate(&images[0][region.clone()]) else { continue };
                    let mut candidates = Vec::new();
                    for slot in slots.clone() {
                        let end = slot.saturating_add(width);
                        if slot < region.end && region.start < end { continue; }
                        let Some(bytes) = images[0].get(slot..end) else { continue };
                        for order in [Little, Big] {
                            if width == 1 && matches!(order, Big) { continue; }
                            if number(bytes, order) == first { candidates.push((slot, order)); }
                        }
                    }
                    for image in &images[1..] {
                        if candidates.is_empty() { break; }
                        // Input validation guarantees equal image lengths.
                        let value = algo.evaluate(&image[region.clone()]).unwrap();
                        candidates.retain(|&(slot, order)| number(&image[slot..slot+width], order) == value);
                    }
                    for (slot, order) in candidates {
                        matches.push(format!("region={:#x}..{:#x} slot={slot:#x} bytes={width} stored={order:?} {algo:x?}", region.start, region.end));
                    }
                }
                matches
            }));
        }
        threads
            .into_iter()
            .flat_map(|h| h.join().expect("search worker failed"))
            .collect()
    })
}

fn changed(a: &[u8], b: &[u8], edited: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for (i, (&left, &right)) in a.iter().zip(b).enumerate() {
        if left == right || edited.iter().any(|r| r.contains(&i)) {
            continue;
        }
        match ranges.last_mut() {
            Some(last) if last.end == i => last.end += 1,
            _ => ranges.push(i..i + 1),
        }
    }
    ranges
}

fn integer(s: &str) -> Result<usize, String> {
    let n = match s.strip_prefix("0x") {
        Some(hex) => usize::from_str_radix(hex, 16),
        None => s.parse(),
    };
    n.map_err(|_| format!("invalid unsigned integer {s:?}"))
}

fn range(s: &str) -> Result<Range<usize>, String> {
    let (a, b) = s.split_once("..").ok_or("expected START..END")?;
    let r = integer(a)?..integer(b)?;
    if r.is_empty() {
        return Err(format!("empty range {s}"));
    }
    Ok(r)
}

const USAGE: &str = "search [--slot OFFSET] [--region START..END] [--jobs N] IMAGE IMAGE ...\ndiff --edited START..END STOCK MODIFIED";

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let mode = args.next().ok_or(USAGE)?;
    if mode == "--help" {
        println!("{USAGE}");
        return Ok(());
    }
    if mode != "search" && mode != "diff" {
        return Err(USAGE.into());
    }
    let mut slots = Vec::new();
    let mut regions = Vec::new();
    let mut paths = Vec::new();
    let mut jobs = thread::available_parallelism().map_or(1, usize::from);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} requires a value"));
        match arg.as_str() {
            "--slot" if mode == "search" => slots.push(integer(&value()?)?),
            "--region" if mode == "search" => regions.push(range(&value()?)?),
            "--edited" if mode == "diff" => regions.push(range(&value()?)?),
            "--jobs" if mode == "search" => jobs = integer(&value()?)?,
            "--gpu" => return Err("TODO: GPU search is unimplemented".into()),
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            _ => paths.push(arg),
        }
    }
    if paths.len() < 2 || jobs == 0 || (mode == "diff" && (paths.len() != 2 || regions.is_empty()))
    {
        return Err(USAGE.into());
    }
    let images: Vec<_> = paths
        .iter()
        .map(|p| fs::read(p).map_err(|e| format!("{p}: {e}")))
        .collect::<Result<_, _>>()?;
    let size = images[0].len();
    if images.iter().any(|v| v.len() != size) {
        return Err("image layouts must have equal lengths".into());
    }
    if regions.iter().any(|r| r.end > size) || slots.iter().any(|&s| s >= size) {
        return Err("range or slot outside image".into());
    }
    if mode == "diff" {
        for r in changed(&images[0], &images[1], &regions) {
            println!("{:#x}..{:#x}", r.start, r.end);
        }
    } else {
        if slots.is_empty() {
            if regions.is_empty() {
                // Common trailer layout: try each stored width at end of file.
                slots.extend(
                    [1, 2, 4]
                        .into_iter()
                        .filter_map(|w| size.checked_sub(w))
                        .filter(|&s| s > 0),
                );
                if slots.is_empty() {
                    return Err("image too short for payload and checksum".into());
                }
            } else {
                for r in regions {
                    for hit in search(&images, r, 0..size, jobs) {
                        println!("{hit}");
                    }
                }
                return Ok(());
            }
        }
        for slot in slots {
            let default = [0..slot];
            let ranges = if regions.is_empty() {
                &default[..]
            } else {
                &regions
            };
            for r in ranges {
                if r.is_empty() {
                    return Err("slot at 0 requires an explicit --region".into());
                }
                for hit in search(&images, r.clone(), slot..slot + 1, jobs) {
                    println!("{hit}");
                }
            }
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_checks() {
        let input = b"123456789";
        for (bits, poly, init, reflected, xorout, expected) in [
            (16, 0x8005, 0, true, 0, 0xbb3d),
            (16, 0x1021, 0xffff, false, 0, 0x29b1),
            (32, 0x04c11db7, u32::MAX, true, u32::MAX, 0xcbf43926),
            (32, 0x1edc6f41, u32::MAX, true, u32::MAX, 0xe3069283),
        ] {
            assert_eq!(
                Algorithm::Crc {
                    bits,
                    poly,
                    init,
                    reflected,
                    xorout
                }
                .evaluate(input),
                Some(expected)
            );
        }
        assert_eq!(Algorithm::Fletcher16.evaluate(b"abcde"), Some(0xc8f0));
        assert_eq!(Algorithm::Adler32.evaluate(b"Wikipedia"), Some(0x11e60398));
        assert_eq!(vag(&[1, 0, 0, 0], 0, 0), Some(0xdf9a547c));
        assert_eq!(vag(&[1, 2, 3], 0, 0), None);
    }

    #[test]
    fn every_image_and_storage_order_must_match() {
        let mut images: Vec<_> = [1u16, 256, 0xff12]
            .into_iter()
            .map(|n| {
                let mut image = vec![0xe7; 2];
                image.extend(n.to_be_bytes());
                image.extend([0, 3]);
                image.extend((!n.wrapping_add(3)).to_le_bytes());
                image
            })
            .collect();
        let find = |images: &[Vec<u8>], jobs| {
            search(images, 2..6, 0..8, jobs)
                .into_iter()
                .filter(|s| {
                    s.contains("slot=0x6")
                        && s.contains("stored=Little")
                        && s.contains("order: Big")
                        && s.contains("OnesComplement")
                })
                .collect::<Vec<_>>()
        };
        let matches = find(&images, 3);
        assert!(!matches.is_empty());
        assert_eq!(matches, find(&images, 1));
        images[2][7] ^= 1;
        assert!(find(&images, 3).is_empty());
    }

    #[test]
    fn diff_splits_at_edits_and_unchanged_bytes() {
        assert_eq!(
            changed(&[0; 8], &[1, 1, 1, 0, 1, 0, 1, 1], &[1..2, 4..5]),
            vec![0..1, 2..3, 6..8]
        );
    }
}
