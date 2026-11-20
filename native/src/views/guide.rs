//! Guide: the bundled user guide (Markdown pages in the `guide` data folder,
//! [`DataRoot::Guide`]) rendered with `egui_commonmark`, plus the pages'
//! ```` ```pinout <unit-id>``` ```` embeds drawn as wiring diagrams from the
//! bundled ECU database ([`pinout`]). Port of the webview app's Guide panel
//! (`guide.rs`, `app-tools.js`, `util.js` `renderMarkdown`). Offline.
//!
//! Links: `#/guide/<id>` switches page; `http(s)` opens outside; any other
//! link target is shown as plain text (as the webview app does). The guide
//! has no images, so no image loader is needed.

mod pinout;

use crate::host::{DataRoot, Host, NativeView, Tier, ViewGroup};
use crate::icons;
use crate::theme::{self, font, Tokens, UI_MEDIUM, UI_STRONG};
use eframe::egui::{self, RichText, Sense, Ui, UiBuilder, Vec2};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use std::path::{Path, PathBuf};

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Public;

pub fn create() -> Box<dyn NativeView> {
    Box::new(GuideView::default())
}

// ---------------------------------------------------------------------------
// Pages (no UI)
// ---------------------------------------------------------------------------

/// One guide page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Section {
    /// The file stem (`NN-slug`), also the `#/guide/<id>` link target.
    pub id: String,
    pub title: String,
}

/// Pretty title from a `NN-slug` stem: `01-hardware` -> "Hardware".
pub(crate) fn guide_title(stem: &str) -> String {
    let slug = stem.split_once('-').map(|x| x.1).unwrap_or(stem);
    let mut out = String::new();
    for (i, word) in slug.split(['-', '_']).enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let mut cs = word.chars();
        if let Some(f) = cs.next() {
            out.extend(f.to_uppercase());
            out.push_str(cs.as_str());
        }
    }
    out
}

/// Every `*.md` page in `dir`, sorted by file name.
pub(crate) fn list(dir: &Path) -> Result<Vec<Section>, String> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("md"))
        .collect();
    files.sort();
    Ok(files
        .iter()
        .filter_map(|p| {
            let stem = p.file_stem()?.to_str()?;
            Some(Section {
                id: stem.to_string(),
                title: guide_title(stem),
            })
        })
        .collect())
}

/// Only a bare `NN-slug` stem is a page id (no path traversal).
pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub(crate) fn read(dir: &Path, id: &str) -> Result<String, String> {
    if !valid_id(id) {
        return Err("invalid guide id".into());
    }
    std::fs::read_to_string(dir.join(format!("{id}.md"))).map_err(|e| format!("{id}: {e}"))
}

/// A piece of a page: Markdown, or a pinout embed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Segment {
    Markdown(String),
    Pinout(String),
}

/// The unit id of a ```` ```pinout <unit-id> ```` fence line.
fn pinout_fence(line: &str) -> Option<&str> {
    let id = line.trim_end().strip_prefix("```pinout")?;
    if !id.starts_with(char::is_whitespace) {
        return None;
    }
    let id = id.trim();
    let ok = id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    ok.then_some(id)
}

/// Split a page into Markdown and pinout embeds (a pinout fence and its
/// body, up to the closing fence, become one embed).
pub(crate) fn segments(md: &str) -> Vec<Segment> {
    let md = md.replace("\r\n", "\n");
    let mut out = Vec::new();
    let mut text = String::new();
    let mut lines = md.lines();
    let mut in_code = false;
    while let Some(line) = lines.next() {
        if !in_code {
            if let Some(id) = pinout_fence(line) {
                for l in lines.by_ref() {
                    if l.starts_with("```") {
                        break;
                    }
                }
                if !text.trim().is_empty() {
                    out.push(Segment::Markdown(sanitize_links(&text)));
                }
                text.clear();
                out.push(Segment::Pinout(id.to_string()));
                continue;
            }
        }
        if line.starts_with("```") {
            in_code = !in_code;
        }
        text.push_str(line);
        text.push('\n');
    }
    if !text.trim().is_empty() {
        out.push(Segment::Markdown(sanitize_links(&text)));
    }
    out
}

/// Is `url` a link the guide follows (`http(s)` or `#/guide/<id>`)?
fn followable(url: &str) -> bool {
    let u = url.trim();
    u.starts_with("https://")
        || u.starts_with("http://")
        || u.strip_prefix("#/guide/").is_some_and(valid_id)
}

/// Turn `[text](target)` into plain `text` unless the target is followable,
/// outside code (fenced blocks and inline code spans).
pub(crate) fn sanitize_links(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut fenced = false;
    for line in md.split_inclusive('\n') {
        if line.starts_with("```") {
            fenced = !fenced;
        }
        if fenced || line.starts_with("```") || !line.contains("](") {
            out.push_str(line);
            continue;
        }
        out.push_str(&sanitize_line(line));
    }
    out
}

fn sanitize_line(line: &str) -> String {
    let b = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let mut code = false;
    while i < b.len() {
        let c = b[i];
        if c == b'`' {
            code = !code;
        }
        if !code && c == b'[' {
            // [text](target) with no nested brackets in text.
            if let Some(close) = line[i + 1..].find(']').map(|x| i + 1 + x) {
                let text = &line[i + 1..close];
                if !text.contains('[') && line[close + 1..].starts_with('(') {
                    if let Some(end) = line[close + 2..].find(')').map(|x| close + 2 + x) {
                        let url = &line[close + 2..end];
                        if !url.contains(char::is_whitespace) && !followable(url) {
                            out.push_str(text);
                            i = end + 1;
                            continue;
                        }
                    }
                }
            }
        }
        let ch = line[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

/// The bundled database, loaded on the first pinout embed.
enum Db {
    Loaded(opentune_db::Database),
    Failed(String),
}

#[derive(Default)]
pub struct GuideView {
    sections: Option<Result<Vec<Section>, String>>,
    cur: Option<String>,
    page: Option<Result<Vec<Segment>, String>>,
    cache: CommonMarkCache,
    db: Option<Db>,
    /// Scroll the page to the top on the next frame.
    to_top: bool,
}

impl GuideView {
    fn open(&mut self, dir: &Path, id: &str) {
        self.cur = Some(id.to_string());
        self.page = Some(read(dir, id).map(|md| segments(&md)));
        self.to_top = true;
    }

    fn ensure_db(&mut self, host: &Host<'_>) {
        if self.db.is_some() {
            return;
        }
        self.db = Some(match host.data_dir(DataRoot::Database) {
            None => Db::Failed("the ECU database folder was not found in this build".into()),
            Some(d) => match opentune_db::Database::load_dir(d) {
                Ok(db) => Db::Loaded(db),
                Err(e) => Db::Failed(format!("the ECU database did not load: {e}")),
            },
        });
    }

    fn nav(&mut self, ui: &mut Ui, t: &Tokens, sections: &[Section]) -> Option<String> {
        let mut pick = None;
        theme::panel(
            ui,
            t,
            "Guide",
            |ui| {
                theme::pill(ui, t, &sections.len().to_string(), theme::Tone::Muted, true);
            },
            |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("guide-nav")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add_space(6.0);
                        for s in sections {
                            let on = self.cur.as_deref() == Some(s.id.as_str());
                            let w = ui.available_width();
                            let (r, resp) =
                                ui.allocate_exact_size(Vec2::new(w, 28.0), Sense::click());
                            let bg = r.shrink2(Vec2::new(6.0, 1.0));
                            if on {
                                ui.painter().rect_filled(
                                    bg,
                                    egui::CornerRadius::same(theme::RADIUS_SM),
                                    t.sel,
                                );
                            } else if resp.hovered() {
                                ui.painter().rect_filled(
                                    bg,
                                    egui::CornerRadius::same(theme::RADIUS_SM),
                                    t.surface2,
                                );
                            }
                            ui.painter().text(
                                egui::pos2(bg.left() + 10.0, bg.center().y),
                                egui::Align2::LEFT_CENTER,
                                &s.title,
                                font(if on { UI_STRONG } else { UI_MEDIUM }, 13.0),
                                if on { t.fg } else { t.fg_dim },
                            );
                            if resp.clicked() {
                                pick = Some(s.id.clone());
                            }
                        }
                    });
            },
        );
        pick
    }

    fn body(
        &mut self,
        ui: &mut Ui,
        host: &Host<'_>,
        t: &Tokens,
        sections: &[Section],
    ) -> Option<String> {
        let title = self
            .cur
            .as_deref()
            .and_then(|c| sections.iter().find(|s| s.id == c))
            .map(|s| s.title.clone())
            .unwrap_or_else(|| "Guide".into());
        let needs_db = matches!(&self.page, Some(Ok(segs)) if segs.iter().any(|s| matches!(s, Segment::Pinout(_))));
        if needs_db {
            self.ensure_db(host);
        }
        // Page links are hooks: `#/guide/<id>` switches page.
        for s in sections {
            let hook = format!("#/guide/{}", s.id);
            if self.cache.get_link_hook(&hook).is_none() {
                self.cache.add_link_hook(hook);
            }
        }
        let mut follow = None;
        let to_top = std::mem::take(&mut self.to_top);
        theme::panel(
            ui,
            t,
            &title,
            |_| {},
            |ui| {
                let mut sa = egui::ScrollArea::vertical()
                    .id_salt("guide-body")
                    .auto_shrink([false, false]);
                if to_top {
                    sa = sa.vertical_scroll_offset(0.0);
                }
                sa.show(ui, |ui| {
                    egui::Frame::new()
                        .inner_margin(egui::Margin::symmetric(20, 14))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            match &self.page {
                                None => {
                                    ui.label(
                                        RichText::new("Pick a page on the left.").color(t.fg_dim),
                                    );
                                }
                                Some(Err(e)) => {
                                    ui.label(RichText::new(e).color(t.crit));
                                }
                                Some(Ok(segs)) => {
                                    for seg in segs {
                                        match seg {
                                            Segment::Markdown(md) => {
                                                CommonMarkViewer::new().show(
                                                    ui,
                                                    &mut self.cache,
                                                    md,
                                                );
                                                for s in sections {
                                                    let hook = format!("#/guide/{}", s.id);
                                                    if self.cache.get_link_hook(&hook) == Some(true)
                                                    {
                                                        follow = Some(s.id.clone());
                                                    }
                                                }
                                            }
                                            Segment::Pinout(id) => {
                                                ui.add_space(6.0);
                                                pinout_embed(ui, t, self.db.as_ref(), id);
                                                ui.add_space(6.0);
                                            }
                                        }
                                    }
                                }
                            }
                        });
                });
            },
        );
        follow
    }
}

fn pinout_embed(ui: &mut Ui, t: &Tokens, db: Option<&Db>, id: &str) {
    theme::panel_frame(t)
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            match db {
                Some(Db::Loaded(db)) => match db.unit(id) {
                    Some(u) => {
                        ui.label(
                            RichText::new(format!("Bench wiring for {}", u.name))
                                .font(font(UI_STRONG, 12.5))
                                .color(t.fg),
                        );
                        pinout::show(ui, t, u);
                    }
                    None => {
                        ui.label(
                            RichText::new(format!(
                                "Wiring diagram: unit '{id}' is not in the bundled ECU database."
                            ))
                            .color(t.warn),
                        );
                    }
                },
                Some(Db::Failed(e)) => {
                    ui.label(
                        RichText::new(format!("Wiring diagram for '{id}': {e}")).color(t.warn),
                    );
                }
                None => {
                    ui.label(RichText::new("Loading the wiring diagram…").color(t.fg_dim));
                }
            }
        });
}

impl NativeView for GuideView {
    fn id(&self) -> &str {
        "guide"
    }
    fn label(&self) -> &str {
        "Guide"
    }
    fn icon(&self) -> &str {
        icons::BOOK_OPEN
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Help
    }
    fn tier(&self) -> Tier {
        TIER
    }
    fn ui(&mut self, ui: &mut Ui, host: &mut Host<'_>) {
        let t = *host.tokens();
        let Some(dir) = host.data_dir(DataRoot::Guide).map(Path::to_path_buf) else {
            theme::empty_state(
                ui,
                &t,
                icons::BOOK_OPEN,
                "Guide resources not found",
                "This build has no guide folder. The same Markdown pages are in the project \
                 source under desktop/src-tauri/guide/.",
            );
            return;
        };
        if self.sections.is_none() {
            self.sections = Some(list(&dir));
        }
        let sections = match self.sections.clone() {
            Some(Ok(s)) if !s.is_empty() => s,
            Some(Err(e)) => {
                theme::empty_state(ui, &t, icons::WARNING, "The guide did not load", &e);
                return;
            }
            _ => {
                theme::empty_state(
                    ui,
                    &t,
                    icons::BOOK_OPEN,
                    "The guide is empty",
                    &format!("No Markdown pages in {}.", dir.display()),
                );
                return;
            }
        };
        if self.cur.is_none() {
            let first = sections[0].id.clone();
            self.open(&dir, &first);
        }
        let (left, right) = crate::views::maps::two_pane(ui);
        let mut pick = None;
        ui.scope_builder(UiBuilder::new().max_rect(left), |ui| {
            pick = self.nav(ui, &t, &sections);
        });
        ui.scope_builder(UiBuilder::new().max_rect(right), |ui| {
            if let Some(f) = self.body(ui, host, &t, &sections) {
                pick = Some(f);
            }
        });
        if let Some(id) = pick {
            self.open(&dir, &id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_ids_and_listing() {
        assert_eq!(guide_title("01-hardware"), "Hardware");
        assert_eq!(guide_title("21-ECU-database"), "ECU Database");
        assert_eq!(guide_title("44-edc15c6-bench"), "Edc15c6 Bench");
        assert_eq!(guide_title("plain"), "Plain");
        assert!(valid_id("38-SA2-script"));
        for bad in ["", "../x", "a/b", "a.md", "a b"] {
            assert!(!valid_id(bad), "{bad}");
        }
        let dir = std::env::temp_dir().join(format!("ot-n3-guide-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("10-second.md"), "# Two\n").unwrap();
        std::fs::write(dir.join("00-first-page.md"), "# One\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        let s = list(&dir).unwrap();
        let ids: Vec<&str> = s.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, ["00-first-page", "10-second"]);
        assert_eq!(s[0].title, "First Page");
        assert_eq!(read(&dir, "10-second").unwrap(), "# Two\n");
        assert!(read(&dir, "../10-second").is_err());
        assert!(read(&dir, "99-missing").is_err());
        assert!(list(&dir.join("none")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pinout_fences_become_embeds() {
        let md =
            "# Bench\n\nIntro.\n\n```pinout synthetic-unit\n```\n\n## After\n\n```\ncode\n```\n";
        let s = segments(md);
        assert_eq!(s.len(), 3, "{s:?}");
        assert!(matches!(&s[0], Segment::Markdown(m) if m.contains("Intro.")));
        assert_eq!(s[1], Segment::Pinout("synthetic-unit".into()));
        assert!(
            matches!(&s[2], Segment::Markdown(m) if m.contains("## After") && m.contains("code"))
        );
        // Not an embed: bad id, no space, or inside a code block.
        assert!(pinout_fence("```pinout Bad_Id").is_none());
        assert!(pinout_fence("```pinoutx").is_none());
        let inside = "```\n```pinout x\n```\n";
        assert!(segments(inside)
            .iter()
            .all(|s| matches!(s, Segment::Markdown(_))));
    }

    #[test]
    fn only_followable_links_stay_links() {
        let md = "See [the bench page](#/guide/44-edc15c6-bench), [docs](https://example.org/x) \
                  and [a file](../docs/X.md). `[code](y.md)` stays.\n";
        let out = sanitize_links(md);
        assert!(out.contains("[the bench page](#/guide/44-edc15c6-bench)"));
        assert!(out.contains("[docs](https://example.org/x)"));
        assert!(out.contains(" and a file."), "{out}");
        assert!(out.contains("`[code](y.md)`"));
        let fenced = "```\n[x](y.md)\n```\n";
        assert_eq!(sanitize_links(fenced), fenced);
        assert!(!followable("#/guide/../x"));
        assert!(followable("#/guide/00-overview"));
    }

    /// The repository's guide pages all list, read and split.
    #[test]
    fn repository_guide_loads() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../desktop/src-tauri/guide");
        if !dir.is_dir() {
            return;
        }
        let s = list(&dir).unwrap();
        assert!(!s.is_empty());
        let mut embeds = 0;
        for sec in &s {
            let md = read(&dir, &sec.id).unwrap();
            embeds += segments(&md)
                .iter()
                .filter(|x| matches!(x, Segment::Pinout(_)))
                .count();
        }
        assert!(embeds >= 1, "a pinout embed is in the guide");
    }
}
