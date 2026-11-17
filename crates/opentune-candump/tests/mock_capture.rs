//! End-to-end: mock driver monitor → capture loop → candump log + stats.
use opentune_candump::{run_capture, CaptureParams, CaptureStats, Format, FrameWriter};
use opentune_core::{Interface, MonitorOpts};
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;
use std::time::Duration;

fn capture(filter: Vec<u32>, format: Format) -> (String, CaptureStats) {
    let mut drv = opentune_driver_mock::MockDriver::new();
    let mon = drv.can_monitor().expect("mock exposes a monitor");
    let mut opts = MonitorOpts::default();
    // Virtual time: no sleeping, ~1 s of synthetic bus per 10 ms of wall time.
    opts.options.insert("mock_realtime".into(), "0".into());
    opts.options.insert("mock_frames".into(), "500".into());
    opts.filter_ids = filter;
    mon.start(&opts).unwrap();
    let mut w = FrameWriter::new(Vec::new(), format, &mon.channel(), mon.wall_base_us());
    let stats = Mutex::new(CaptureStats::new());
    let stop = AtomicBool::new(false);
    let params = CaptureParams {
        opts,
        duration: Some(Duration::from_millis(300)),
    };
    let out = run_capture(mon, &mut w, &stats, &stop, &params, &mut |_| {}).unwrap();
    let text = String::from_utf8(w.into_inner().unwrap()).unwrap();
    let stats = stats.into_inner().unwrap();
    assert_eq!(out.frames, stats.frames());
    assert_eq!(out.bytes, text.len() as u64);
    (text, stats)
}

#[test]
fn mock_traffic_is_logged_in_candump_format() {
    let (text, stats) = capture(vec![], Format::Candump);
    assert_eq!(stats.frames(), 500);
    let first = text.lines().next().unwrap();
    assert!(first.starts_with('('), "{first}");
    assert!(first.contains(") vcan0 "), "{first}");
    let snap = stats.snapshot(1_000_000, 256);
    let ids: Vec<u32> = snap.ids.iter().map(|r| r.id).collect();
    assert!(ids.contains(&0x280) && ids.contains(&0x1A0) && ids.contains(&0x5A0));
    let engine = snap.ids.iter().find(|r| r.id == 0x280).unwrap();
    // 0x280 is scheduled at 100 Hz in synthetic time.
    assert!((engine.hz - 100.0).abs() < 1.0, "hz = {}", engine.hz);
}

#[test]
fn filter_keeps_only_selected_ids() {
    let (text, stats) = capture(vec![0x5A0], Format::Csv);
    assert!(stats.frames() > 0);
    assert_eq!(stats.unique_ids(), 1);
    for line in text.lines().skip(1) {
        assert_eq!(line.split(',').nth(1), Some("5A0"), "{line}");
    }
}
