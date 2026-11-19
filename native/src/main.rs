//! OpenTune native frontend (prototype): egui/eframe on wgpu, calling the Rust
//! core in-process. See README.md.

mod adapters;
mod app;
mod bench;
mod hex;
mod icons;
mod image;
mod mapview;
mod settings;
mod shell;
mod theme;
mod views;

use bench::{BenchCfg, Scene};
use std::path::PathBuf;

pub struct Args {
    pub file: Option<PathBuf>,
    pub defs: Option<PathBuf>,
    pub def: Option<String>,
    pub bench: Option<BenchCfg>,
    pub vsync: bool,
}

const USAGE: &str = "usage: opentune-native [FILE] [--defs DIR] [--def ID]
                       [--bench hex|maps [--frames N] [--px N] [--csv FILE]] [--no-vsync]";

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        file: None,
        defs: None,
        def: None,
        bench: None,
        vsync: true,
    };
    let (mut scene, mut frames, mut px, mut csv) = (None, 600usize, 24.0f32, None);
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        let mut val = |name: &str| {
            it.next()
                .ok_or_else(|| format!("{name} needs a value\n{USAGE}"))
        };
        match x.as_str() {
            "--defs" => a.defs = Some(PathBuf::from(val("--defs")?)),
            "--def" => a.def = Some(val("--def")?),
            "--bench" => {
                let s = val("--bench")?;
                scene = Some(Scene::parse(&s).ok_or(format!("unknown scene '{s}'\n{USAGE}"))?);
            }
            "--frames" => {
                frames = val("--frames")?
                    .parse()
                    .map_err(|e| format!("--frames: {e}"))?
            }
            "--px" => px = val("--px")?.parse().map_err(|e| format!("--px: {e}"))?,
            "--csv" => csv = Some(PathBuf::from(val("--csv")?)),
            "--no-vsync" => a.vsync = false,
            "-h" | "--help" => return Err(USAGE.into()),
            f if !f.starts_with("--") && a.file.is_none() => a.file = Some(PathBuf::from(f)),
            other => return Err(format!("unknown argument '{other}'\n{USAGE}")),
        }
    }
    if let Some(scene) = scene {
        a.bench = Some(BenchCfg {
            scene,
            frames,
            px_per_frame: px,
            csv,
        });
    }
    Ok(a)
}

fn main() -> eframe::Result {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let mut wgpu_options = eframe::WgpuConfiguration::default();
    wgpu_options.surface.present_mode = if args.vsync {
        eframe::wgpu::PresentMode::AutoVsync
    } else {
        eframe::wgpu::PresentMode::AutoNoVsync
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([960.0, 600.0])
            .with_title("OpenTune"),
        persist_window: true,
        renderer: eframe::Renderer::Wgpu,
        wgpu_options,
        ..Default::default()
    };
    eframe::run_native(
        "OpenTune native",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, args)))),
    )
}
