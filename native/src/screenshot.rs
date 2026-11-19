//! `--screenshot OUT.png`: capture the window after it has settled, write a
//! PNG and close. Used to review the UI against the design target.

use eframe::egui::{self, ColorImage};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Frames to wait before capturing (fonts, layout and scroll areas settle).
pub const SETTLE_FRAMES: u32 = 40;

pub struct Shot {
    pub path: PathBuf,
    frame: u32,
    requested: bool,
    done: Arc<AtomicBool>,
}

impl Shot {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            frame: 0,
            requested: false,
            done: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Call once per frame.
    pub fn step(&mut self, ctx: &egui::Context) {
        self.frame += 1;
        if self.done.load(Ordering::SeqCst) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if self.frame >= SETTLE_FRAMES && !self.requested {
            self.requested = true;
            let path = self.path.clone();
            let done = self.done.clone();
            let repaint = ctx.clone();
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                egui::ScreenshotCallback::new(move |img| {
                    match write_png(&path, &img) {
                        Ok(()) => println!(
                            "screenshot: {} ({}x{})",
                            path.display(),
                            img.size[0],
                            img.size[1]
                        ),
                        Err(e) => eprintln!("screenshot: {e}"),
                    }
                    done.store(true, Ordering::SeqCst);
                    repaint.request_repaint();
                }),
            ));
        }
        ctx.request_repaint();
    }
}

pub fn write_png(path: &PathBuf, img: &ColorImage) -> std::io::Result<()> {
    let f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(f, img.size[0] as u32, img.size[1] as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(std::io::Error::other)?;
    let data: Vec<u8> = img
        .pixels
        .iter()
        .flat_map(|c| c.to_srgba_unmultiplied())
        .collect();
    w.write_image_data(&data).map_err(std::io::Error::other)
}
