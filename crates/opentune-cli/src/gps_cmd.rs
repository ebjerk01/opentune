//! `opentune gps`: read a GPS/GNSS receiver (NMEA 0183 over a serial or USB
//! CDC-ACM port) or replay a recorded NMEA file, printing one CSV row per fix
//! and the NMEA line counters. The same source plugs into `opentune log
//! --gps`. See docs/LIVE-LOGGING.md (GPS channels).
use anyhow::{anyhow, bail, Result};
use opentune_gps::{GpsFeed, GpsReader, GpsSnapshot, ReplayFeed};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(clap::Args)]
pub struct GpsArgs {
    /// Serial port of the receiver (`/dev/ttyACM0`, `COM5`) or a recorded
    /// NMEA file (replayed one epoch per row).
    #[arg(value_name = "PORT|FILE", required_unless_present = "list_ports")]
    source: Option<String>,
    /// List serial ports (likely GNSS receivers first) and exit.
    #[arg(long)]
    list_ports: bool,
    /// Baud rate for a UART receiver (USB CDC-ACM ignores it).
    #[arg(long, default_value_t = opentune_gps::DEFAULT_BAUD)]
    baud: u32,
    /// Rows to print (default: every epoch of a file, 10 from a port).
    #[arg(long)]
    count: Option<usize>,
    /// Milliseconds between rows from a port.
    #[arg(long, default_value_t = 1000)]
    interval_ms: u64,
    /// Write the CSV here (otherwise to stdout).
    #[arg(long)]
    out: Option<PathBuf>,
}

/// CSV header of `opentune gps`.
pub const GPS_CSV_HEADER: &str =
    "row,utc_date,utc_time,lat,lon,alt_m,speed_kmh,course_deg,sats,fix,hdop,good,note";

fn opt<T: ToString>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_default()
}

fn row(out: &mut dyn Write, i: usize, s: &GpsSnapshot) -> std::io::Result<()> {
    let f = &s.fix;
    let pos = |v: Option<f64>| opt(v.filter(|_| s.good));
    writeln!(
        out,
        "{i},{},{},{},{},{},{},{},{},{},{},{},{}",
        opt(f.date),
        opt(f.time),
        pos(f.lat),
        pos(f.lon),
        pos(f.alt_m),
        pos(f.speed_kmh),
        pos(f.course_deg),
        opt(f.sats.filter(|_| !s.stale)),
        if s.stale {
            String::new()
        } else {
            f.fix_code().to_string()
        },
        opt(f.hdop.filter(|_| !s.stale)),
        u8::from(s.good),
        s.note.replace([',', '\n'], " ")
    )
}

pub fn run(a: GpsArgs) -> Result<()> {
    if a.list_ports {
        let ports = opentune_gps::list_ports();
        if ports.is_empty() {
            println!("no serial ports found");
        }
        for p in ports {
            let usb = match (p.vid, p.pid) {
                (Some(v), Some(d)) => format!(" USB {v:04x}:{d:04x}"),
                _ => String::new(),
            };
            let product = p.product.map(|s| format!(" {s}")).unwrap_or_default();
            let hint = if p.likely_gnss { "  (likely GNSS)" } else { "" };
            println!("{}{usb}{product}{hint}", p.name);
        }
        return Ok(());
    }
    let Some(spec) = a.source.as_deref() else {
        bail!("give a serial port or an NMEA file (or --list-ports)");
    };
    let mut sink: Box<dyn Write> = match &a.out {
        Some(p) => {
            Box::new(std::fs::File::create(p).map_err(|e| anyhow!("create {}: {e}", p.display()))?)
        }
        None => Box::new(std::io::stdout().lock()),
    };
    writeln!(sink, "{GPS_CSV_HEADER}")?;

    let path = Path::new(spec);
    let (rows, good, last) = if path.is_file() {
        let mut feed = ReplayFeed::open(path).map_err(|e| anyhow!("read {spec}: {e}"))?;
        let limit = a.count.unwrap_or(usize::MAX);
        let (mut n, mut good, mut last) = (0usize, 0usize, None);
        while n < limit {
            let Some(s) = feed.next_epoch() else { break };
            row(&mut *sink, n, &s)?;
            good += usize::from(s.good);
            n += 1;
            last = Some(s);
        }
        (n, good, last)
    } else {
        let mut r = GpsReader::open_serial(spec, a.baud).map_err(|e| anyhow!("{e}"))?;
        // Give the receiver a moment to send its first sentences.
        r.wait_ready(Duration::from_millis(2500));
        let count = a.count.unwrap_or(10);
        let (mut n, mut good, mut last) = (0usize, 0usize, None);
        while n < count {
            if n > 0 {
                std::thread::sleep(Duration::from_millis(a.interval_ms));
            }
            let s = r.snapshot();
            row(&mut *sink, n, &s)?;
            sink.flush()?;
            good += usize::from(s.good);
            n += 1;
            let ended = s.ended.clone();
            last = Some(s);
            if let Some(e) = ended {
                eprintln!("gps: source ended: {e}");
                break;
            }
        }
        (n, good, last)
    };
    sink.flush()?;
    let stats = last.map(|s| s.stats).unwrap_or_default();
    eprintln!("gps: {rows} row(s), {good} with a good fix; {stats}");
    Ok(())
}
