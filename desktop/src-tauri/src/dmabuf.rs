//! WebKitGTK DMA-BUF renderer policy (Linux).
//!
//! GPU rendering stays on by default: OpenTune sets
//! `WEBKIT_DISABLE_DMABUF_RENDERER=1` only on a setup known to break the
//! DMA-BUF path, and never touches `WEBKIT_DISABLE_COMPOSITING_MODE`.
//!
//! **Overrides** win, in this order: `OPENTUNE_KEEP_WEBKIT_ENV=1` (touch
//! nothing); a `WEBKIT_DISABLE_DMABUF_RENDERER` the user set (`1` or `0`,
//! never changed); `OPENTUNE_DMABUF=on|off|auto` (`on` never sets it, `off`
//! always sets `=1`, `auto` = the rules below, the default).
//!
//! **Rules** (auto), first hit wins and is reported as the reason:
//! 1. no usable render node (no `/dev/dri/renderD*`, or none readable and
//!    writable);
//! 2. NVIDIA proprietary driver (`/proc/driver/nvidia/version`, or a render
//!    node bound to `nvidia`; nouveau is fine);
//! 3. a virtual / emulated GPU (`virtio_gpu`, `vmwgfx`, `qxl`, `vboxvideo`,
//!    `bochs-drm`, `bochs`, `cirrus`, `simpledrm`);
//! 4. software GL forced (`LIBGL_ALWAYS_SOFTWARE`, `GALLIUM_DRIVER=llvmpipe|softpipe`);
//! 5. remote X (`SSH_CONNECTION` set, no `WAYLAND_DISPLAY`, and a `DISPLAY`
//!    that is not a local `:N[.M]`);
//! 6. **self-heal**: a marker file is written at every auto start and removed
//!    when the frontend reports its first painted frame. A marker left over
//!    from a start that had DMA-BUF on means that start never drew: the next
//!    starts use the fallback (`=1`). After 2 clean fallback starts DMA-BUF
//!    is retried once; if that start fails too it stays off for good
//!    (`state: off`) — a broken setup is not retried forever.
//!    `OPENTUNE_DMABUF=on` forces it back; deleting the state file resets it.
//!
//! amdgpu, i915/xe, radeon, nouveau, panfrost, v3d and the like fall through
//! to DMA-BUF on. Everything here is a pure function over a [`Probe`] (the
//! FS/env view) and the persisted [`Persist`] state, so the tests need no real
//! `/dev`, `/proc` or `/sys`; [`probe_system`] and the file helpers are the
//! only I/O.

use serde::Serialize;
use std::path::{Path, PathBuf};

/// `OPENTUNE_DMABUF=auto|on|off`.
pub const KNOB_ENV: &str = "OPENTUNE_DMABUF";
/// The WebKitGTK variable this module may set.
pub const DMABUF_ENV: &str = "WEBKIT_DISABLE_DMABUF_RENDERER";
/// Set next to [`DMABUF_ENV`] when OpenTune set it, so a relaunched child (the
/// updater restarts the app with the parent's environment) re-decides instead
/// of taking the inherited value for the user's.
pub const SET_BY_US_ENV: &str = "OPENTUNE_DMABUF_SET_BY_APP";

/// Render-node drivers of virtual / emulated GPUs (rule 3).
pub const VIRTUAL_DRIVERS: [&str; 8] = [
    "virtio_gpu",
    "vmwgfx",
    "qxl",
    "vboxvideo",
    "bochs-drm",
    "bochs",
    "cirrus",
    "simpledrm",
];

/// Which override or rule made the decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Rule {
    /// Not Linux: WebKitGTK is not in use.
    NotLinux,
    /// `OPENTUNE_KEEP_WEBKIT_ENV=1`.
    KeepEnv,
    /// `WEBKIT_DISABLE_DMABUF_RENDERER` set by the user.
    UserVar,
    /// `OPENTUNE_DMABUF=on`.
    KnobOn,
    /// `OPENTUNE_DMABUF=off`.
    KnobOff,
    /// Rule 1.
    NoRenderNode,
    /// Rule 2.
    Nvidia,
    /// Rule 3.
    VirtualGpu,
    /// Rule 4.
    SoftwareGl,
    /// Rule 5.
    RemoteX,
    /// Rule 6 (fallback or permanently off after a failed retry).
    SelfHeal,
    /// No known-bad case: DMA-BUF on.
    None,
}

/// Self-heal state, persisted between starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealState {
    /// DMA-BUF on (normal).
    On,
    /// A start with DMA-BUF on did not draw: fallback until 2 clean starts.
    Fallback,
    /// This start retries DMA-BUF once.
    Retry,
    /// The retry failed too: off for good.
    Off,
}

impl HealState {
    fn as_str(self) -> &'static str {
        match self {
            HealState::On => "on",
            HealState::Fallback => "fallback",
            HealState::Retry => "retry",
            HealState::Off => "off",
        }
    }
    fn parse(s: &str) -> Option<HealState> {
        Some(match s.trim() {
            "on" => HealState::On,
            "fallback" => HealState::Fallback,
            "retry" => HealState::Retry,
            "off" => HealState::Off,
            _ => return None,
        })
    }
}

/// The persisted self-heal state (`render-state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Persist {
    pub state: HealState,
    /// Clean (drawn) starts with the fallback since entering it.
    pub clean: u32,
}

impl Default for Persist {
    fn default() -> Self {
        Persist {
            state: HealState::On,
            clean: 0,
        }
    }
}

/// Clean fallback starts before DMA-BUF is retried once.
pub const CLEAN_BEFORE_RETRY: u32 = 2;

/// One `/dev/dri/renderD*` node.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderNode {
    /// `renderD128`.
    pub name: String,
    /// Basename of `/sys/class/drm/<name>/device/driver`.
    pub driver: Option<String>,
    /// Readable and writable by this process.
    pub usable: bool,
}

/// The FS / environment view the rules look at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Probe {
    pub nodes: Vec<RenderNode>,
    /// `/proc/driver/nvidia/version` exists.
    pub nvidia_proc: bool,
    pub libgl_always_software: Option<String>,
    pub gallium_driver: Option<String>,
    pub ssh_connection: bool,
    pub wayland_display: bool,
    pub display: Option<String>,
}

impl Probe {
    /// The render-node driver names, deduplicated, in node order.
    pub fn drivers(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for d in self.nodes.iter().filter_map(|n| n.driver.clone()) {
            if !out.contains(&d) {
                out.push(d);
            }
        }
        out
    }
}

/// `OPENTUNE_DMABUF`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Knob {
    Auto,
    On,
    Off,
}

impl Knob {
    fn as_str(self) -> &'static str {
        match self {
            Knob::Auto => "auto",
            Knob::On => "on",
            Knob::Off => "off",
        }
    }
}

/// The user's overrides, read from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overrides {
    pub keep_env: bool,
    /// `WEBKIT_DISABLE_DMABUF_RENDERER` as the user set it.
    pub user_var: Option<String>,
    pub knob: Knob,
    /// `OPENTUNE_DMABUF` had a value this module does not know (treated as auto).
    pub knob_unknown: Option<String>,
}

impl Overrides {
    pub fn from_env(get: &dyn Fn(&str) -> Option<String>) -> Overrides {
        let inherited_from_us = get(SET_BY_US_ENV).is_some_and(|v| v == "1");
        let (knob, knob_unknown) = match get(KNOB_ENV).map(|v| v.trim().to_ascii_lowercase()) {
            None => (Knob::Auto, None),
            Some(v) => match v.as_str() {
                "" | "auto" => (Knob::Auto, None),
                "on" | "1" | "true" | "yes" => (Knob::On, None),
                "off" | "0" | "false" | "no" => (Knob::Off, None),
                _ => (Knob::Auto, Some(v)),
            },
        };
        Overrides {
            keep_env: get(crate::startup::KEEP_ENV).is_some_and(|v| truthy(&v)),
            user_var: if inherited_from_us {
                None
            } else {
                get(DMABUF_ENV)
            },
            knob,
            knob_unknown,
        }
    }
}

fn truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// `:0`, `:1.0` — a local X display.
fn local_display(d: &str) -> bool {
    let Some(rest) = d.strip_prefix(':') else {
        return false;
    };
    let mut parts = rest.splitn(2, '.');
    let num =
        |s: Option<&str>| s.is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
    let first = parts.next();
    match parts.next() {
        None => num(first),
        Some(screen) => num(first) && num(Some(screen)),
    }
}

/// Rules 1–5: the first known-bad case, with its reason.
pub fn known_bad(p: &Probe) -> Option<(Rule, String)> {
    if !p.nodes.iter().any(|n| n.usable) {
        let why = if p.nodes.is_empty() {
            "no /dev/dri/renderD* render node".to_string()
        } else {
            "no render node is readable and writable".to_string()
        };
        return Some((Rule::NoRenderNode, why));
    }
    if p.nvidia_proc
        || p.nodes
            .iter()
            .any(|n| n.driver.as_deref() == Some("nvidia"))
    {
        return Some((Rule::Nvidia, "NVIDIA proprietary driver".to_string()));
    }
    if let Some(d) = p
        .nodes
        .iter()
        .filter_map(|n| n.driver.as_deref())
        .find(|d| VIRTUAL_DRIVERS.contains(d))
    {
        return Some((Rule::VirtualGpu, format!("virtual / emulated GPU ({d})")));
    }
    let gallium = p
        .gallium_driver
        .as_deref()
        .map(|g| g.trim().to_ascii_lowercase());
    if p.libgl_always_software.as_deref().is_some_and(truthy)
        || matches!(gallium.as_deref(), Some("llvmpipe") | Some("softpipe"))
    {
        return Some((Rule::SoftwareGl, "software GL forced".to_string()));
    }
    if p.ssh_connection && !p.wayland_display && !p.display.as_deref().is_some_and(local_display) {
        return Some((Rule::RemoteX, "remote X display over SSH".to_string()));
    }
    None
}

/// The decision, as the trace, `render_env` and the Render check show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Decision {
    /// The DMA-BUF renderer is on for this process.
    pub enabled: bool,
    pub rule: Rule,
    /// Human-readable reason ("none, DMA-BUF on" when nothing fired).
    pub reason: String,
    /// OpenTune set `WEBKIT_DISABLE_DMABUF_RENDERER=1`.
    pub set_env: bool,
    /// Render-node driver name(s).
    pub drivers: Vec<String>,
    /// `OPENTUNE_DMABUF` as understood (`auto` / `on` / `off`).
    pub knob: &'static str,
    /// Self-heal state after this start (auto mode only).
    pub heal: Option<HealState>,
    /// One-shot notice for this start ("the last start did not draw ...").
    pub notice: Option<String>,
}

impl Decision {
    pub fn not_linux() -> Decision {
        Decision {
            enabled: false,
            rule: Rule::NotLinux,
            reason: "not Linux (WebKitGTK not in use)".into(),
            set_env: false,
            drivers: Vec::new(),
            knob: "auto",
            heal: None,
            notice: None,
        }
    }
}

/// What a start does: the decision, and (auto mode) the state and marker to
/// write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub decision: Decision,
    /// The state to persist (auto mode only).
    pub persist: Option<Persist>,
    /// The marker to write for this start: `Some(dmabuf_on)` (auto mode only).
    pub marker: Option<bool>,
}

/// WebKitGTK's reading of the variable: any value but `0` disables DMA-BUF.
fn var_disables(v: &str) -> bool {
    v != "0"
}

/// Decide this start. `marker` is the marker a previous start left behind
/// (`Some(dmabuf_on_then)` = that start never reported a painted frame).
pub fn decide(o: &Overrides, p: &Probe, persisted: Persist, marker: Option<bool>) -> Plan {
    let drivers = p.drivers();
    let mk = |enabled: bool, rule: Rule, reason: String, set_env: bool| Decision {
        enabled,
        rule,
        reason,
        set_env,
        drivers: drivers.clone(),
        knob: o.knob.as_str(),
        heal: None,
        notice: None,
    };
    let fixed = |decision: Decision| Plan {
        decision,
        persist: None,
        marker: None,
    };
    if o.keep_env {
        let on = !o.user_var.as_deref().is_some_and(var_disables);
        return fixed(mk(
            on,
            Rule::KeepEnv,
            "OPENTUNE_KEEP_WEBKIT_ENV=1: environment left as inherited".into(),
            false,
        ));
    }
    if let Some(v) = o.user_var.as_deref() {
        return fixed(mk(
            !var_disables(v),
            Rule::UserVar,
            format!("{DMABUF_ENV}={v} set by the user"),
            false,
        ));
    }
    match o.knob {
        Knob::On => {
            return fixed(mk(true, Rule::KnobOn, format!("{KNOB_ENV}=on"), false));
        }
        Knob::Off => {
            return fixed(mk(false, Rule::KnobOff, format!("{KNOB_ENV}=off"), true));
        }
        Knob::Auto => {}
    }

    // Account for a previous start that never drew with DMA-BUF on.
    let mut st = persisted;
    let mut notice = None;
    if marker == Some(true) {
        match st.state {
            HealState::Retry => {
                st = Persist {
                    state: HealState::Off,
                    clean: 0,
                };
                notice = Some(
                    "The last start did not draw again after retrying the DMA-BUF renderer: \
                     it stays off from now on (OPENTUNE_DMABUF=on forces it)."
                        .to_string(),
                );
            }
            HealState::Off => {}
            HealState::On | HealState::Fallback => {
                st = Persist {
                    state: HealState::Fallback,
                    clean: 0,
                };
                notice = Some(
                    "The last start did not draw a frame with the DMA-BUF renderer on: \
                     this start uses the fallback (DMA-BUF off)."
                        .to_string(),
                );
            }
        }
    }

    let (enabled, rule, reason) = if let Some((rule, why)) = known_bad(p) {
        (false, rule, why)
    } else {
        match st.state {
            HealState::On | HealState::Retry => {
                st.state = HealState::On;
                (true, Rule::None, "none, DMA-BUF on".to_string())
            }
            HealState::Fallback if st.clean >= CLEAN_BEFORE_RETRY => {
                st.state = HealState::Retry;
                (
                    true,
                    Rule::None,
                    format!(
                        "self-heal: retrying DMA-BUF once after {CLEAN_BEFORE_RETRY} clean starts with the fallback"
                    ),
                )
            }
            HealState::Fallback => (
                false,
                Rule::SelfHeal,
                format!(
                    "self-heal: last start did not draw ({}/{CLEAN_BEFORE_RETRY} clean starts before one retry)",
                    st.clean
                ),
            ),
            HealState::Off => (
                false,
                Rule::SelfHeal,
                "self-heal: DMA-BUF failed again after a retry; kept off".to_string(),
            ),
        }
    };
    let mut decision = mk(enabled, rule, reason, !enabled);
    decision.heal = Some(st.state);
    decision.notice = notice;
    Plan {
        decision,
        persist: Some(st),
        marker: Some(enabled),
    }
}

/// The frontend painted its first frame: the start in `marker` was clean.
/// A fallback start counts towards the retry; a successful retry goes back to
/// normal.
pub fn on_painted(st: Persist, marker: Option<bool>) -> Persist {
    match (marker, st.state) {
        (Some(false), HealState::Fallback) => Persist {
            state: HealState::Fallback,
            clean: st.clean.saturating_add(1),
        },
        (Some(true), HealState::Retry) => Persist::default(),
        _ => st,
    }
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

/// Where the marker and state live: `$XDG_CACHE_HOME/opentune` or
/// `~/.cache/opentune`.
pub fn state_dir(get: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let base = get("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| get("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("opentune"))
}

pub fn marker_path(dir: &Path) -> PathBuf {
    dir.join("render-pending")
}

pub fn state_path(dir: &Path) -> PathBuf {
    dir.join("render-state")
}

/// `state: fallback` / `clean: 1` lines; anything unreadable = the default.
pub fn parse_state(text: &str) -> Persist {
    let mut p = Persist::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        match k.trim() {
            "state" => {
                if let Some(s) = HealState::parse(v) {
                    p.state = s;
                }
            }
            "clean" => p.clean = v.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    p
}

pub fn format_state(p: Persist) -> String {
    format!(
        "# OpenTune WebKit DMA-BUF self-heal state (docs/TROUBLESHOOTING-STARTUP.md).\n\
         # Delete this file to reset it.\nstate: {}\nclean: {}\n",
        p.state.as_str(),
        p.clean
    )
}

/// `dmabuf: on|off`.
pub fn parse_marker(text: &str) -> Option<bool> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == "dmabuf").then(|| v.trim() == "on")
    })
}

pub fn read_state(dir: &Path) -> Persist {
    std::fs::read_to_string(state_path(dir))
        .map(|t| parse_state(&t))
        .unwrap_or_default()
}

pub fn read_marker(dir: &Path) -> Option<bool> {
    std::fs::read_to_string(marker_path(dir))
        .ok()
        .and_then(|t| parse_marker(&t))
}

/// Write the state and this start's marker (best effort: a read-only home
/// only loses the self-heal).
pub fn write_start(dir: &Path, st: Persist, marker: bool) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(state_path(dir), format_state(st))?;
    std::fs::write(
        marker_path(dir),
        format!("dmabuf: {}\n", if marker { "on" } else { "off" }),
    )
}

/// First frame painted: clear the marker and record the clean start.
pub fn painted(dir: &Path) -> std::io::Result<()> {
    let marker = read_marker(dir);
    if marker.is_none() {
        return Ok(());
    }
    let next = on_painted(read_state(dir), marker);
    std::fs::write(state_path(dir), format_state(next))?;
    std::fs::remove_file(marker_path(dir))
}

// ---------------------------------------------------------------------------
// System probe
// ---------------------------------------------------------------------------

/// Read the real `/dev/dri`, `/sys/class/drm` and `/proc` view. Render nodes
/// are checked with `access(R_OK|W_OK)` (honours ACLs, does not open the
/// device, so a sleeping discrete GPU is not woken).
#[cfg(target_os = "linux")]
pub fn probe_system(get: &dyn Fn(&str) -> Option<String>) -> Probe {
    use std::os::unix::ffi::OsStrExt;
    let mut nodes: Vec<RenderNode> = std::fs::read_dir("/dev/dri")
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if !name.starts_with("renderD") {
                        return None;
                    }
                    let driver = std::fs::read_link(format!("/sys/class/drm/{name}/device/driver"))
                        .ok()
                        .and_then(|l| l.file_name().map(|f| f.to_string_lossy().into_owned()));
                    let usable = std::ffi::CString::new(e.path().as_os_str().as_bytes())
                        .map(|c| {
                            // SAFETY: `c` is a valid NUL-terminated path that
                            // outlives the call; access() only reads it.
                            unsafe { libc::access(c.as_ptr(), libc::R_OK | libc::W_OK) == 0 }
                        })
                        .unwrap_or(false);
                    Some(RenderNode {
                        name,
                        driver,
                        usable,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    Probe {
        nodes,
        nvidia_proc: Path::new("/proc/driver/nvidia/version").exists(),
        libgl_always_software: get("LIBGL_ALWAYS_SOFTWARE"),
        gallium_driver: get("GALLIUM_DRIVER"),
        ssh_connection: get("SSH_CONNECTION").is_some(),
        wayland_display: get("WAYLAND_DISPLAY").is_some(),
        display: get("DISPLAY"),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn probe_system(_get: &dyn Fn(&str) -> Option<String>) -> Probe {
    Probe::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, driver: &str, usable: bool) -> RenderNode {
        RenderNode {
            name: name.into(),
            driver: Some(driver.into()),
            usable,
        }
    }
    fn gpu(driver: &str) -> Probe {
        Probe {
            nodes: vec![node("renderD128", driver, true)],
            wayland_display: true,
            ..Probe::default()
        }
    }
    fn auto() -> Overrides {
        Overrides {
            keep_env: false,
            user_var: None,
            knob: Knob::Auto,
            knob_unknown: None,
        }
    }
    fn run(p: &Probe) -> Decision {
        decide(&auto(), p, Persist::default(), None).decision
    }

    #[test]
    fn good_drivers_keep_dmabuf_on() {
        for d in [
            "amdgpu", "i915", "xe", "radeon", "nouveau", "panfrost", "v3d",
        ] {
            let dec = run(&gpu(d));
            assert!(dec.enabled, "{d}");
            assert_eq!(dec.rule, Rule::None);
            assert_eq!(dec.reason, "none, DMA-BUF on");
            assert!(!dec.set_env);
            assert_eq!(dec.drivers, vec![d.to_string()]);
        }
    }

    #[test]
    fn rule1_no_usable_render_node() {
        let none = run(&Probe::default());
        assert_eq!(
            (none.enabled, none.rule, none.set_env),
            (false, Rule::NoRenderNode, true)
        );
        assert!(none.reason.contains("no /dev/dri/renderD*"));
        let locked = run(&Probe {
            nodes: vec![node("renderD128", "amdgpu", false)],
            ..Probe::default()
        });
        assert_eq!(locked.rule, Rule::NoRenderNode);
        assert!(locked.reason.contains("readable and writable"));
        // One usable node is enough.
        let mixed = run(&Probe {
            nodes: vec![
                node("renderD128", "amdgpu", false),
                node("renderD129", "i915", true),
            ],
            ..Probe::default()
        });
        assert!(mixed.enabled);
        assert_eq!(mixed.drivers, vec!["amdgpu", "i915"]);
    }

    #[test]
    fn rule2_nvidia_proprietary_not_nouveau() {
        assert_eq!(run(&gpu("nvidia")).rule, Rule::Nvidia);
        let proc_only = Probe {
            nvidia_proc: true,
            ..gpu("i915")
        };
        assert_eq!(
            run(&proc_only).rule,
            Rule::Nvidia,
            "hybrid laptop with nvidia loaded"
        );
        assert!(run(&gpu("nouveau")).enabled);
        // Rule 1 comes first.
        let p = Probe {
            nvidia_proc: true,
            ..Probe::default()
        };
        assert_eq!(run(&p).rule, Rule::NoRenderNode);
    }

    #[test]
    fn rule3_virtual_gpus() {
        for d in VIRTUAL_DRIVERS {
            let dec = run(&gpu(d));
            assert_eq!(dec.rule, Rule::VirtualGpu, "{d}");
            assert!(dec.reason.contains(d));
            assert!(!dec.enabled && dec.set_env);
        }
        // NVIDIA comes first.
        let p = Probe {
            nvidia_proc: true,
            ..gpu("virtio_gpu")
        };
        assert_eq!(run(&p).rule, Rule::Nvidia);
    }

    #[test]
    fn rule4_software_gl() {
        for (sw, gal) in [
            (Some("1"), None),
            (Some("true"), None),
            (None, Some("llvmpipe")),
            (None, Some("softpipe")),
        ] {
            let p = Probe {
                libgl_always_software: sw.map(String::from),
                gallium_driver: gal.map(String::from),
                ..gpu("amdgpu")
            };
            assert_eq!(run(&p).rule, Rule::SoftwareGl, "{sw:?} {gal:?}");
        }
        for (sw, gal) in [
            (Some("0"), None),
            (None, Some("radeonsi")),
            (None, Some("zink")),
        ] {
            let p = Probe {
                libgl_always_software: sw.map(String::from),
                gallium_driver: gal.map(String::from),
                ..gpu("amdgpu")
            };
            assert!(run(&p).enabled, "{sw:?} {gal:?}");
        }
    }

    #[test]
    fn rule5_remote_x() {
        let remote = |display: Option<&str>, wayland: bool| Probe {
            ssh_connection: true,
            wayland_display: wayland,
            display: display.map(String::from),
            ..gpu("amdgpu")
        };
        for d in [
            Some("localhost:10.0"),
            Some("10.0.0.2:0"),
            Some(":"),
            Some(":a"),
            None,
        ] {
            assert_eq!(run(&remote(d, false)).rule, Rule::RemoteX, "{d:?}");
        }
        for d in [Some(":0"), Some(":1.0"), Some(":12")] {
            assert!(run(&remote(d, false)).enabled, "local display {d:?}");
        }
        assert!(
            run(&remote(Some("localhost:10.0"), true)).enabled,
            "Wayland session"
        );
        let no_ssh = Probe {
            ssh_connection: false,
            ..remote(Some("localhost:10.0"), false)
        };
        assert!(run(&no_ssh).enabled);
    }

    #[test]
    fn overrides_win_in_order() {
        let bad = Probe::default(); // rule 1 would fire
        let keep = Overrides {
            keep_env: true,
            user_var: Some("0".into()),
            knob: Knob::Off,
            knob_unknown: None,
        };
        let plan = decide(&keep, &bad, Persist::default(), Some(true));
        assert_eq!(plan.decision.rule, Rule::KeepEnv);
        assert!(plan.decision.enabled && !plan.decision.set_env);
        assert_eq!(
            (plan.persist, plan.marker),
            (None, None),
            "touches no files either"
        );

        let user = |v: &str| Overrides {
            keep_env: false,
            user_var: Some(v.into()),
            knob: Knob::Off,
            knob_unknown: None,
        };
        let on = decide(&user("0"), &bad, Persist::default(), None).decision;
        assert_eq!(
            (on.rule, on.enabled, on.set_env),
            (Rule::UserVar, true, false)
        );
        let off = decide(&user("1"), &gpu("amdgpu"), Persist::default(), None).decision;
        assert_eq!(
            (off.rule, off.enabled, off.set_env),
            (Rule::UserVar, false, false)
        );

        let knob = |k: Knob| Overrides {
            keep_env: false,
            user_var: None,
            knob: k,
            knob_unknown: None,
        };
        let forced_on = decide(
            &knob(Knob::On),
            &bad,
            Persist {
                state: HealState::Off,
                clean: 0,
            },
            Some(true),
        );
        assert_eq!(
            (forced_on.decision.rule, forced_on.decision.enabled),
            (Rule::KnobOn, true)
        );
        assert_eq!(forced_on.persist, None);
        let forced_off =
            decide(&knob(Knob::Off), &gpu("amdgpu"), Persist::default(), None).decision;
        assert_eq!(
            (forced_off.rule, forced_off.enabled, forced_off.set_env),
            (Rule::KnobOff, false, true)
        );
        assert_eq!(forced_off.knob, "off");
    }

    #[test]
    fn overrides_from_env() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let o = Overrides::from_env(&env(&[]));
        assert_eq!(
            (o.keep_env, o.user_var.clone(), o.knob),
            (false, None, Knob::Auto)
        );
        let o = Overrides::from_env(&env(&[
            ("OPENTUNE_KEEP_WEBKIT_ENV", "1"),
            ("OPENTUNE_DMABUF", "OFF"),
        ]));
        assert!(o.keep_env);
        assert_eq!(o.knob, Knob::Off);
        let o = Overrides::from_env(&env(&[("OPENTUNE_DMABUF", "sometimes")]));
        assert_eq!(
            (o.knob, o.knob_unknown.as_deref()),
            (Knob::Auto, Some("sometimes"))
        );
        let o = Overrides::from_env(&env(&[(DMABUF_ENV, "1")]));
        assert_eq!(o.user_var.as_deref(), Some("1"));
        // Inherited from an OpenTune parent (updater relaunch): not the user's.
        let o = Overrides::from_env(&env(&[(DMABUF_ENV, "1"), (SET_BY_US_ENV, "1")]));
        assert_eq!(o.user_var, None);
    }

    #[test]
    fn self_heal_state_machine() {
        let p = gpu("amdgpu");
        let o = auto();
        // 1. Normal start: on, marker on.
        let s1 = decide(&o, &p, Persist::default(), None);
        assert!(s1.decision.enabled);
        assert_eq!(s1.marker, Some(true));
        assert_eq!(s1.decision.heal, Some(HealState::On));
        assert_eq!(s1.decision.notice, None);
        // ... it never drew: the marker is still there at the next start.
        let s2 = decide(&o, &p, s1.persist.unwrap(), s1.marker);
        assert!(!s2.decision.enabled && s2.decision.set_env);
        assert_eq!(s2.decision.rule, Rule::SelfHeal);
        assert!(s2.decision.reason.contains("last start did not draw"));
        assert!(s2
            .decision
            .notice
            .as_deref()
            .unwrap()
            .contains("did not draw"));
        let st = s2.persist.unwrap();
        assert_eq!(
            st,
            Persist {
                state: HealState::Fallback,
                clean: 0
            }
        );
        // Clean fallback start #1.
        let st = on_painted(st, s2.marker);
        assert_eq!(st.clean, 1);
        let s3 = decide(&o, &p, st, None);
        assert!(!s3.decision.enabled);
        assert_eq!(s3.decision.notice, None, "the notice is shown once");
        let st = on_painted(s3.persist.unwrap(), s3.marker);
        assert_eq!(st.clean, 2);
        // After 2 clean starts: one retry with DMA-BUF on.
        let s4 = decide(&o, &p, st, None);
        assert!(s4.decision.enabled && !s4.decision.set_env);
        assert!(s4.decision.reason.contains("retrying"));
        assert_eq!(s4.persist.unwrap().state, HealState::Retry);
        // The retry works: back to normal.
        assert_eq!(
            on_painted(s4.persist.unwrap(), s4.marker),
            Persist::default()
        );
        // The retry fails: off for good, never retried again.
        let s5 = decide(&o, &p, s4.persist.unwrap(), s4.marker);
        assert!(!s5.decision.enabled);
        assert_eq!(s5.persist.unwrap().state, HealState::Off);
        assert!(s5.decision.notice.as_deref().unwrap().contains("stays off"));
        let mut st = s5.persist.unwrap();
        let mut marker = s5.marker;
        for _ in 0..10 {
            st = on_painted(st, marker);
            let next = decide(&o, &p, st, None);
            assert!(!next.decision.enabled, "a broken setup is not retried");
            assert_eq!(next.decision.rule, Rule::SelfHeal);
            st = next.persist.unwrap();
            marker = next.marker;
        }
        assert_eq!(st.state, HealState::Off);
        // OPENTUNE_DMABUF=on still forces it.
        let forced = Overrides {
            knob: Knob::On,
            ..auto()
        };
        assert!(decide(&forced, &p, st, None).decision.enabled);
    }

    #[test]
    fn self_heal_edges() {
        let o = auto();
        // A fallback start that did not draw either is not DMA-BUF's fault.
        let fb = Persist {
            state: HealState::Fallback,
            clean: 1,
        };
        let plan = decide(&o, &gpu("amdgpu"), fb, Some(false));
        assert_eq!(plan.persist, Some(fb));
        assert_eq!(on_painted(fb, None), fb, "nothing pending, nothing counted");
        // A known-bad rule still reports the self-heal state and wins over it.
        let plan = decide(&o, &gpu("vmwgfx"), Persist::default(), Some(true));
        assert_eq!(plan.decision.rule, Rule::VirtualGpu);
        assert_eq!(plan.decision.heal, Some(HealState::Fallback));
        assert_eq!(plan.marker, Some(false));
        // A retry state without a marker (the paint report was lost) is normal.
        let plan = decide(
            &o,
            &gpu("amdgpu"),
            Persist {
                state: HealState::Retry,
                clean: 0,
            },
            None,
        );
        assert!(plan.decision.enabled);
        assert_eq!(plan.persist.unwrap().state, HealState::On);
    }

    #[test]
    fn state_and_marker_files_round_trip() {
        for st in [
            HealState::On,
            HealState::Fallback,
            HealState::Retry,
            HealState::Off,
        ] {
            let p = Persist {
                state: st,
                clean: 3,
            };
            assert_eq!(parse_state(&format_state(p)), p);
        }
        assert_eq!(parse_state("garbage\nstate: bogus\n"), Persist::default());
        assert_eq!(parse_marker("dmabuf: on\n"), Some(true));
        assert_eq!(parse_marker("dmabuf: off\n"), Some(false));
        assert_eq!(parse_marker(""), None);

        let dir = std::env::temp_dir().join(format!("ot-dmabuf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_start(
            &dir,
            Persist {
                state: HealState::Fallback,
                clean: 1,
            },
            false,
        )
        .unwrap();
        assert_eq!(read_marker(&dir), Some(false));
        painted(&dir).unwrap();
        assert_eq!(
            read_marker(&dir),
            None,
            "marker cleared on the first painted frame"
        );
        assert_eq!(
            read_state(&dir),
            Persist {
                state: HealState::Fallback,
                clean: 2
            }
        );
        painted(&dir).unwrap();
        assert_eq!(read_state(&dir).clean, 2, "a second report counts nothing");
        let _ = std::fs::remove_dir_all(&dir);

        let get = |k: &str| match k {
            "XDG_CACHE_HOME" => Some("relative/ignored".to_string()),
            "HOME" => Some("/home/u".to_string()),
            _ => None,
        };
        assert_eq!(
            state_dir(&get),
            Some(PathBuf::from("/home/u/.cache/opentune"))
        );
        let xdg = |k: &str| (k == "XDG_CACHE_HOME").then(|| "/c".to_string());
        assert_eq!(state_dir(&xdg), Some(PathBuf::from("/c/opentune")));
        assert_eq!(state_dir(&|_| None), None);
    }

    #[test]
    fn local_display_forms() {
        for d in [":0", ":1", ":10.0", ":0.1"] {
            assert!(local_display(d), "{d}");
        }
        for d in [
            "",
            ":",
            ":a",
            ":0.",
            "host:0",
            "localhost:10.0",
            ":0.0.0",
            "unix:0",
        ] {
            assert!(!local_display(d), "{d}");
        }
    }
}
