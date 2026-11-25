//! The read-only allow-list shared by every read path: the desktop/native
//! app's `ReadOnlyDiag` and the drivers' own guards (the OpenPort 2.0 K-line
//! link) reference THIS list, so the safety rule cannot drift between them.
//! An allow-list, so an unknown service is refused too. A guard rail on the
//! service id, not a protocol parser.

/// The services a READ-ONLY path admits (everything else is refused unsent,
/// so no ECU change is possible without a confirmation).
///
/// * UDS / KWP2000: session control `10` (any session: it changes no data,
///   and every write service stays refused in it), security access `27`,
///   read by identifier `22`, read memory by address `23`, read DTCs `19`,
///   tester present `3E`, and the upload trio `35` requestUpload / `36`
///   transferData / `37` requestTransferExit (`34` requestDownload is
///   refused, so `36` can never carry a download).
/// * KWP2000 reads: `1A` read ECU identification, `21` read data by local
///   identifier, `17` / `18` read DTCs (status / by status), `12` read
///   freeze frame.
/// * OBD-II read modes: `01` `02` `03` `06` `07` `09` `0A` (never `04`
///   clear DTCs or `08` control of on-board systems).
/// * SSM (Subaru): `A0` read memory block, `A8` read address, `BF` ECU init
///   / identification.
///
/// Refused, for example: `2E` `3B` `3D` writes, `34` download, `31` / KWP `32`
/// routines, `14` / OBD `04` clear DTCs, `2F` / KWP `30` I/O control, `11`
/// reset, `28` communication control, `85` DTC setting, `2C` dynamic DID
/// definition, SSM `B0` / `B8` writes.
pub const READ_SERVICES: &[u8] = &[
    // UDS / KWP2000
    0x10, 0x27, 0x22, 0x23, 0x19, 0x3E, 0x35, 0x36, 0x37, // KWP2000 reads
    0x1A, 0x21, 0x17, 0x18, 0x12, // OBD-II read modes
    0x01, 0x02, 0x03, 0x06, 0x07, 0x09, 0x0A, // SSM reads
    0xA0, 0xA8, 0xBF,
];

/// `Err` (nothing to send) unless `req` is a read-path service; see
/// [`READ_SERVICES`].
pub fn refuse_ecu_changing(req: &[u8]) -> Result<(), String> {
    match req {
        [] => Err("empty request".into()),
        [sid, ..] if READ_SERVICES.contains(sid) => Ok(()),
        [sid, ..] => Err(format!(
            "read-only operation: service 0x{sid:02X} is not a read service and was refused; \
             nothing was sent (ECU changes go through confirm_ecu + mutate or actuate)"
        )),
    }
}

/// [`READ_SERVICES`] entries a KWP2000 K-line read path does NOT admit: the
/// upload trio (the K-line read path reads with `23`), and `A0`/`A8`/`BF`,
/// which are SSM reads but sit in the KWP2000 manufacturer-specific range,
/// where they could mean anything on another make's ECU.
pub const KLINE_EXCLUDED: &[u8] = &[0x35, 0x36, 0x37, 0xA0, 0xA8, 0xBF];

/// ISO 14230-2 link management a K-line link sends besides the reads:
/// StartCommunication / StopCommunication.
pub const KLINE_LINK_SERVICES: &[u8] = &[0x81, 0x82];

/// `startDiagnosticSession` sessions a K-line read path refuses even though
/// `10` is on the list: programming (KWP2000 `85`, UDS `02`). (The general
/// list allows any session, since some ECUs need one for an upload.)
pub const KLINE_REFUSED_SESSIONS: &[u8] = &[0x85, 0x02];

/// Is service `sid` allowed on a KWP2000 K-line read path? [`READ_SERVICES`]
/// minus [`KLINE_EXCLUDED`], plus [`KLINE_LINK_SERVICES`] — derived from the
/// shared list, never a copy of it.
pub fn kline_read_allowed(sid: u8) -> bool {
    (READ_SERVICES.contains(&sid) && !KLINE_EXCLUDED.contains(&sid))
        || KLINE_LINK_SERVICES.contains(&sid)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list is a safety rule: a change to it must be deliberate, so its
    /// exact contents are pinned here.
    #[test]
    fn read_services_are_pinned() {
        let mut got = READ_SERVICES.to_vec();
        got.sort_unstable();
        assert_eq!(
            got,
            [
                0x01, 0x02, 0x03, 0x06, 0x07, 0x09, 0x0A, 0x10, 0x12, 0x17, 0x18, 0x19, 0x1A, 0x21,
                0x22, 0x23, 0x27, 0x35, 0x36, 0x37, 0x3E, 0xA0, 0xA8, 0xBF
            ]
        );
        for bad in [
            0x04, 0x08, 0x11, 0x14, 0x28, 0x2C, 0x2E, 0x2F, 0x30, 0x31, 0x32, 0x34, 0x3B, 0x3D,
            0x85, 0xB0, 0xB8,
        ] {
            assert!(refuse_ecu_changing(&[bad]).is_err(), "{bad:02X}");
        }
        assert!(refuse_ecu_changing(&[]).is_err());
    }

    /// The K-line set is exactly the shared list minus its exclusions plus
    /// link management, and never admits a service the shared list refuses
    /// (other than StartCommunication / StopCommunication).
    #[test]
    fn kline_set_is_derived_from_the_shared_list() {
        let kline: Vec<u8> = (0..=255u8).filter(|&s| kline_read_allowed(s)).collect();
        assert_eq!(
            kline,
            [
                0x01, 0x02, 0x03, 0x06, 0x07, 0x09, 0x0A, 0x10, 0x12, 0x17, 0x18, 0x19, 0x1A, 0x21,
                0x22, 0x23, 0x27, 0x3E, 0x81, 0x82
            ]
        );
        for s in kline {
            assert!(
                READ_SERVICES.contains(&s) || KLINE_LINK_SERVICES.contains(&s),
                "{s:02X} is admitted on K-line but not on the shared list"
            );
        }
        for e in KLINE_EXCLUDED {
            assert!(
                READ_SERVICES.contains(e),
                "exclusion {e:02X} must name a listed service"
            );
        }
    }
}
