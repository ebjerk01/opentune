//! The flashable-ECU model at the wire level (raw requests, no client).
use opentune_core::protocol::DiagTransport;
use opentune_udssim::flash::crc16_ccitt_false;
use opentune_udssim::{FlashConfig, FlashRegion, SimTransport, UdsServer};

const BASE: u32 = 0x8000;

fn server() -> UdsServer {
    let mut s = UdsServer::new();
    s.enable_flash(FlashConfig {
        regions: vec![FlashRegion::new(BASE, vec![0x55; 0x40])],
        max_block_len: 0x12, // 16 data bytes per TransferData
        ..FlashConfig::default()
    });
    s
}

fn range(addr: u32, len: u32) -> Vec<u8> {
    let mut v = vec![0x44];
    v.extend_from_slice(&addr.to_be_bytes());
    v.extend_from_slice(&len.to_be_bytes());
    v
}

/// Extended → programming session, unlock level 0x11 (seed XOR 0xA5).
fn unlock(s: &mut UdsServer) {
    assert_eq!(s.handle(&[0x10, 0x03])[0], 0x50);
    assert_eq!(s.handle(&[0x10, 0x02])[0], 0x50);
    let seed = s.handle(&[0x27, 0x11]);
    let mut key = vec![0x27, 0x12];
    key.extend(seed[2..].iter().map(|b| b ^ 0xA5));
    assert_eq!(s.handle(&key), vec![0x67, 0x12]);
    assert_eq!(s.unlocked_level(), Some(0x11));
}

fn erase(s: &mut UdsServer, addr: u32, len: u32) -> Vec<u8> {
    let mut r = vec![0x31, 0x01, 0xFF, 0x00];
    r.extend(range(addr, len));
    s.handle(&r)
}

fn download(s: &mut UdsServer, addr: u32, len: u32) -> Vec<u8> {
    let mut r = vec![0x34, 0x00];
    r.extend(range(addr, len));
    s.handle(&r)
}

#[test]
fn programming_session_needs_extended_first() {
    let mut s = server();
    assert_eq!(s.handle(&[0x10, 0x02]), vec![0x7F, 0x10, 0x22]);
    assert_eq!(s.handle(&[0x10, 0x03])[0], 0x50);
    assert_eq!(s.handle(&[0x10, 0x02])[0], 0x50);
}

#[test]
fn erase_and_download_need_programming_and_security() {
    let mut s = server();
    assert_eq!(erase(&mut s, BASE, 0x20), vec![0x7F, 0x31, 0x22]);
    s.handle(&[0x10, 0x03]);
    s.handle(&[0x10, 0x02]);
    assert_eq!(erase(&mut s, BASE, 0x20), vec![0x7F, 0x31, 0x33]);
    assert_eq!(download(&mut s, BASE, 0x20), vec![0x7F, 0x34, 0x33]);
}

#[test]
fn download_requires_erased_memory_then_programs_it() {
    let mut s = server();
    unlock(&mut s);
    assert_eq!(download(&mut s, BASE, 0x20), vec![0x7F, 0x34, 0x70]);
    assert_eq!(
        erase(&mut s, BASE, 0x20),
        vec![0x71, 0x01, 0xFF, 0x00, 0x00]
    );
    assert_eq!(s.flash().unwrap().read(BASE, 4).unwrap(), vec![0xFF; 4]);
    // Unsupported dataFormatIdentifier.
    let mut r = vec![0x34, 0x11];
    r.extend(range(BASE, 0x20));
    assert_eq!(s.handle(&r), vec![0x7F, 0x34, 0x31]);
    assert_eq!(download(&mut s, BASE, 0x20), vec![0x74, 0x20, 0x00, 0x12]);
    let data: Vec<u8> = (0..0x20).collect();
    // Too long for maxNumberOfBlockLength.
    let mut big = vec![0x36, 0x01];
    big.extend(&data[..17]);
    assert_eq!(s.handle(&big), vec![0x7F, 0x36, 0x13]);
    for (i, chunk) in data.chunks(16).enumerate() {
        let mut td = vec![0x36, i as u8 + 1];
        td.extend_from_slice(chunk);
        assert_eq!(s.handle(&td), vec![0x76, i as u8 + 1]);
    }
    assert_eq!(s.handle(&[0x37]), vec![0x77]);
    assert_eq!(s.flash().unwrap().read(BASE, 0x20).unwrap(), data);
    // Not yet confirmed by the checksum routine.
    assert!(!s.flash().unwrap().is_valid());
    let mut chk = vec![0x31, 0x01, 0x02, 0x02];
    chk.extend(range(BASE, 0x20));
    chk.extend(crc16_ccitt_false(&data).to_be_bytes());
    assert_eq!(s.handle(&chk), vec![0x71, 0x01, 0x02, 0x02, 0x00]);
    assert!(s.flash().unwrap().is_valid());
    assert_eq!(
        s.handle(&[0x31, 0x01, 0xFF, 0x01]),
        vec![0x71, 0x01, 0xFF, 0x01, 0x00]
    );
    // A wrong CRC fails.
    let n = chk.len();
    chk[n - 1] ^= 1;
    assert_eq!(s.handle(&chk)[4], 0x01);
    // Memory survives a reset.
    assert_eq!(s.handle(&[0x11, 0x01]), vec![0x51, 0x01]);
    assert_eq!(s.flash().unwrap().read(BASE, 0x20).unwrap(), data);
    // A second download into programmed (not erased) memory is refused.
    unlock(&mut s);
    assert_eq!(download(&mut s, BASE, 0x20), vec![0x7F, 0x34, 0x70]);
}

#[test]
fn block_counter_repeat_is_acknowledged_without_rewrite() {
    let mut s = server();
    unlock(&mut s);
    erase(&mut s, BASE, 0x20);
    download(&mut s, BASE, 0x20);
    assert_eq!(s.handle(&[0x36, 0x01, 1, 2]), vec![0x76, 0x01]);
    // Repeat of the previous counter: positive, not written again.
    assert_eq!(s.handle(&[0x36, 0x01, 9, 9]), vec![0x76, 0x01]);
    assert_eq!(s.flash().unwrap().stats().duplicate_blocks, 1);
    assert_eq!(s.flash().unwrap().read(BASE, 2).unwrap(), vec![1, 2]);
    // Skipping ahead is wrongBlockSequenceCounter.
    assert_eq!(s.handle(&[0x36, 0x03, 0]), vec![0x7F, 0x36, 0x73]);
    // Exit before everything arrived is a sequence error.
    assert_eq!(s.handle(&[0x37]), vec![0x7F, 0x37, 0x24]);
}

#[test]
fn power_loss_drops_the_response_and_reboots() {
    let mut s = server();
    s.flash_mut().unwrap().config.faults.power_loss_after_blocks = Some(1);
    unlock(&mut s);
    erase(&mut s, BASE, 0x20);
    download(&mut s, BASE, 0x20);
    let mut t = SimTransport::new(s);
    let err = t.request(&[0x36, 0x01, 0xAB], 100).unwrap_err();
    assert!(matches!(err, opentune_core::Error::Timeout(100)));
    let s = t.server();
    assert_eq!(s.session(), 0x01);
    assert!(!s.is_unlocked());
    let f = s.flash().unwrap();
    assert!(!f.transfer_active() && !f.is_valid());
    assert_eq!(
        f.read(BASE, 1).unwrap(),
        vec![0xAB],
        "the block was written"
    );
}

#[test]
fn slow_erase_answers_response_pending_first() {
    let mut s = server();
    s.flash_mut().unwrap().config.erase_pending = 2;
    unlock(&mut s);
    let mut t = SimTransport::new(s);
    let mut r = vec![0x31, 0x01, 0xFF, 0x00];
    r.extend(range(BASE, 0x10));
    let resp = t.request(&r, 100).unwrap();
    assert_eq!(resp, vec![0x71, 0x01, 0xFF, 0x00, 0x00]);
    assert_eq!(t.trace().pending_count(), 2);
    let lines = t.trace().to_lines();
    let n = lines.len();
    assert!(lines[n - 2].starts_with("<= 7F 31 78"), "{lines:?}");
    assert!(lines[n - 1].starts_with("<- 71"), "{lines:?}");
}

#[test]
fn unreadable_flash_refuses_reads_and_generic_memory_still_works() {
    let mut s = server();
    s.flash_mut().unwrap().config.readable = false;
    s.set_memory(0x100, vec![1, 2, 3, 4]);
    let mut r = vec![0x23];
    r.extend(range(BASE, 4));
    assert_eq!(s.handle(&r), vec![0x7F, 0x23, 0x31]);
    let mut r = vec![0x23];
    r.extend(range(0x100, 2));
    assert_eq!(s.handle(&r), vec![0x63, 1, 2]);
}

#[test]
fn without_a_flash_model_downloads_behave_as_before() {
    let mut s = UdsServer::new();
    assert!(s.flash().is_none());
    assert_eq!(
        s.handle(&[0x34, 0x00, 0x44, 0, 0, 0x10, 0, 0, 0, 0, 4]),
        vec![0x74, 0x20, 0x04, 0x02]
    );
    assert_eq!(s.handle(&[0x36, 0x01, 0xAA]), vec![0x76, 0x01]);
}
