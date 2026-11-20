//! Capability gating: a panel names what its action needs from the adapter
//! ([`Need`]) and [`gate`] says whether the connected adapter (its probe in
//! the top bar's link) can do it, drawing why not. As the webview app's
//! `ot-caps.js`: an ELM327 cannot flash, a K-line cable has no CAN.

use crate::host::{Capabilities, Host};
use crate::icons;
use crate::theme::{font, UI_MEDIUM};
use eframe::egui::{RichText, Ui};

/// Something an action needs from the adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    /// Request/response diagnostics (CAN, or DoIP over Ethernet).
    Diag,
    Can,
    CanFd,
    KLine,
    ReadMemory,
    WriteMemory,
    BenchBoot,
    BatteryVoltage,
    ProgrammingVoltage,
    /// A VW TP2.0 channel (`transport=tp20`).
    Tp20,
    Doip,
}

impl Need {
    pub fn label(self) -> &'static str {
        match self {
            Need::Diag => "a diagnostic link (CAN or DoIP)",
            Need::Can => "CAN bus",
            Need::CanFd => "CAN-FD",
            Need::KLine => "K-line",
            Need::ReadMemory => "memory read",
            Need::WriteMemory => "memory write / flashing",
            Need::BenchBoot => "bench / boot access",
            Need::BatteryVoltage => "battery voltage",
            Need::ProgrammingVoltage => "programming voltage",
            Need::Tp20 => "VW TP2.0",
            Need::Doip => "DoIP (diagnostics over Ethernet)",
        }
    }

    pub fn met(self, c: &Capabilities) -> bool {
        match self {
            Need::Diag => c.can || c.doip,
            Need::Can => c.can,
            Need::CanFd => c.can_fd,
            Need::KLine => c.kline,
            Need::ReadMemory => c.read_memory,
            Need::WriteMemory => c.write_memory,
            Need::BenchBoot => c.bench_boot,
            Need::BatteryVoltage => c.battery_voltage,
            Need::ProgrammingVoltage => c.programming_voltage,
            Need::Tp20 => c.vwtp20,
            Need::Doip => c.doip,
        }
    }

    const ALL: [Need; 11] = [
        Need::Can,
        Need::CanFd,
        Need::KLine,
        Need::Doip,
        Need::Tp20,
        Need::ReadMemory,
        Need::WriteMemory,
        Need::BenchBoot,
        Need::BatteryVoltage,
        Need::ProgrammingVoltage,
        Need::Diag,
    ];
}

/// `Ok` when `caps` covers every need; else why not (no probe yet, or the
/// missing capabilities).
pub fn check(caps: Option<&Capabilities>, needs: &[Need]) -> Result<(), String> {
    let Some(c) = caps else {
        return Err("connect an adapter first (its capabilities are not known yet)".into());
    };
    let missing: Vec<&str> = needs
        .iter()
        .filter(|n| !n.met(c))
        .map(|n| n.label())
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "the connected adapter cannot do this: no {}",
            missing.join(", no ")
        ))
    }
}

/// What the adapter can do, as one line (top bar tooltip).
pub fn summary(caps: Option<&Capabilities>) -> String {
    let Some(c) = caps else {
        return "not probed yet".into();
    };
    let have: Vec<&str> = Need::ALL
        .iter()
        .filter(|n| **n != Need::Diag && n.met(c))
        .map(|n| n.label())
        .collect();
    if have.is_empty() {
        "nothing reported".into()
    } else {
        have.join(", ")
    }
}

/// Can the action run with the connected adapter? When not, draws a short
/// warning line saying why; the panel disables its button on `false`.
pub fn gate(ui: &mut Ui, host: &Host<'_>, needs: &[Need]) -> bool {
    match check(host.capabilities().as_ref(), needs) {
        Ok(()) => true,
        Err(why) => {
            let t = host.tokens();
            ui.label(
                RichText::new(format!("{}  {why}", icons::WARNING))
                    .font(font(UI_MEDIUM, 12.5))
                    .color(t.warn),
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gating_says_what_is_missing() {
        let elm = Capabilities {
            can: true,
            read_memory: true,
            battery_voltage: true,
            ..Default::default()
        };
        check(Some(&elm), &[Need::Diag, Need::ReadMemory]).unwrap();
        let e = check(Some(&elm), &[Need::Diag, Need::WriteMemory, Need::KLine]).unwrap_err();
        assert!(e.contains("memory write") && e.contains("K-line"), "{e}");
        assert!(check(None, &[]).unwrap_err().contains("connect"));
        let doip = Capabilities {
            doip: true,
            ..Default::default()
        };
        check(Some(&doip), &[Need::Diag]).unwrap();
        assert!(check(Some(&doip), &[Need::Can]).is_err());
        assert_eq!(summary(Some(&elm)), "CAN bus, memory read, battery voltage");
        assert_eq!(summary(None), "not probed yet");
    }
}
