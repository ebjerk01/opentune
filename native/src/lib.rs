//! OpenTune native frontend: egui/eframe on wgpu, calling the Rust core
//! in-process. See README.md.
//!
//! As a library it can be embedded: another binary calls [`run`] with its own
//! [`NativeConfig`] (branding, extra checksum algorithms, extra views, an
//! adapter). [`host`] is the plugin API; [`theme`] and [`icons`] let plugin
//! views look native.

mod adapters;
mod app;
mod bench;
mod hex;
pub mod host;
pub mod icons;
mod image;
mod mapview;
mod paths;
mod screenshot;
mod settings;
mod shell;
pub mod theme;
mod views;

use bench::{BenchCfg, Scene};
pub use host::{Branding, Host, NativeConfig, NativeView};
use std::path::PathBuf;

pub(crate) struct Args {
    pub file: Option<PathBuf>,
    pub defs: Option<PathBuf>,
    pub def: Option<String>,
    pub bench: Option<BenchCfg>,
    pub vsync: bool,
    /// Capture the window to this PNG after a few frames, then exit.
    pub screenshot: Option<PathBuf>,
    /// Force a theme for this run ("light" / "dark"), not saved.
    pub theme: Option<String>,
    /// Open this view first (flash, maps, ecudb, drivers, ...).
    pub view: Option<String>,
}

const USAGE: &str = "usage: opentune-native [FILE] [--defs DIR] [--def ID]
                       [--bench hex|maps [--frames N] [--px N] [--csv FILE]] [--no-vsync]
                       [--screenshot OUT.png] [--theme light|dark] [--view flash|maps|ecudb|drivers]";

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        file: None,
        defs: None,
        def: None,
        bench: None,
        vsync: true,
        screenshot: None,
        theme: None,
        view: None,
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
            "--screenshot" => a.screenshot = Some(PathBuf::from(val("--screenshot")?)),
            "--theme" => a.theme = Some(val("--theme")?),
            "--view" => a.view = Some(val("--view")?),
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

/// The app icon (assets/icon-256.png), decoded for the window.
fn icon() -> Option<eframe::egui::IconData> {
    let dec = png::Decoder::new(std::io::Cursor::new(
        include_bytes!("../assets/icon-256.png").as_slice(),
    ));
    let mut r = dec.read_info().ok()?;
    let mut buf = vec![0; r.output_buffer_size()?];
    let info = r.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    buf.truncate(info.buffer_size());
    Some(eframe::egui::IconData {
        rgba: buf,
        width: info.width,
        height: info.height,
    })
}

/// Run the app (parses the command line).
pub fn run(config: NativeConfig) -> eframe::Result {
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
            .with_title(config.branding.name.clone())
            .with_app_id(config.branding.app_id.clone())
            .with_icon(icon().unwrap_or_default()),
        persist_window: true,
        renderer: eframe::Renderer::Wgpu,
        wgpu_options,
        ..Default::default()
    };
    // The storage (settings, window geometry) is per app id, so an embedding
    // build keeps its own.
    let storage_name = config.branding.app_id.clone();
    eframe::run_native(
        &storage_name,
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, args, config)))),
    )
}
