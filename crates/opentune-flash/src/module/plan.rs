//! Dry run of a module flash: every request [`flash_module`](super::flash_module)
//! would send, in order, **without sending anything**.
//!
//! The plan assumes the ECU answers every request positively, is locked (so a
//! key is sent), allows ReadMemoryByAddress (backup + read-back), and reports
//! `assumed_max_block_len` as its `maxNumberOfBlockLength`. Under those
//! assumptions the plan is exactly the run's request sequence; render both
//! with [`describe_request`] to compare.
use super::{
    check_record, erase_record, payload_len, read_chunks, selection, validate, ModuleFlashOptions,
    ModuleImage, ModulePhase,
};
use opentune_core::protocol::uds::encode_address_and_length;
use opentune_core::Result;

/// One planned request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStep {
    /// The step it belongs to.
    pub phase: ModulePhase,
    /// The block, for per-block steps.
    pub block: Option<usize>,
    /// The request bytes (for SendKey: without the key, which depends on the
    /// ECU's seed).
    pub request: Vec<u8>,
    /// The key bytes follow at run time.
    pub key_placeholder: bool,
}

impl PlanStep {
    /// The request as [`describe_request`] renders the real one.
    pub fn render(&self) -> String {
        if self.key_placeholder {
            format!("{} <key>", hex(&self.request))
        } else {
            describe_request(&self.request)
        }
    }
}

/// A module-flash dry run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModulePlan {
    /// The requests, in order.
    pub steps: Vec<PlanStep>,
    /// The assumptions the plan rests on.
    pub assumptions: Vec<String>,
}

impl ModulePlan {
    /// Every step rendered (one line each).
    pub fn lines(&self) -> Vec<String> {
        self.steps.iter().map(PlanStep::render).collect()
    }
}

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Render a request compactly: TransferData as `36 <counter> <N bytes>`,
/// SendKey as `27 <level> <key>`, anything else as hex.
pub fn describe_request(req: &[u8]) -> String {
    match req {
        [0x36, seq, data @ ..] => format!("36 {seq:02X} <{} bytes>", data.len()),
        [0x27, level, _, ..] if level & 0x01 == 0 => format!("27 {level:02X} <key>"),
        _ => hex(req),
    }
}

/// Plan a module flash of `image` under `opts` (structural refusals apply, as
/// in the run). `assumed_max_block_len` stands in for the ECU's answer to
/// RequestDownload. A resume state is not consulted: the plan is a fresh run.
pub fn plan_module_flash(
    image: &ModuleImage,
    opts: &ModuleFlashOptions,
    assumed_max_block_len: usize,
) -> Result<ModulePlan> {
    let p = &opts.protocol;
    let sel = selection(image, opts)?;
    let groups = validate(image, &sel, opts)?;
    let mut steps = Vec::new();
    let mut push = |phase, block, request: Vec<u8>, key_placeholder| {
        steps.push(PlanStep {
            phase,
            block,
            request,
            key_placeholder,
        })
    };
    for &did in p.identity.part.iter().chain(&p.identity.software) {
        push(
            ModulePhase::Identify,
            None,
            vec![0x22, (did >> 8) as u8, did as u8],
            false,
        );
    }
    if let Some(s) = p.extended_session {
        push(ModulePhase::Session, None, vec![0x10, s], false);
    }
    let read = |addr: u32, n: u32| -> Result<Vec<u8>> {
        let mut r = vec![0x23, p.alfid];
        r.extend(encode_address_and_length(p.alfid, addr, n)?);
        Ok(r)
    };
    for g in &groups {
        for (addr, n) in read_chunks(g.erase, p.read_chunk) {
            push(ModulePhase::Backup, None, read(addr, n)?, false);
        }
    }
    push(
        ModulePhase::Session,
        None,
        vec![0x10, p.programming_session],
        false,
    );
    push(
        ModulePhase::Security,
        None,
        vec![0x27, p.security_level],
        false,
    );
    push(
        ModulePhase::Security,
        None,
        vec![0x27, p.security_level.wrapping_add(1)],
        true,
    );
    let n = payload_len(p, assumed_max_block_len);
    for g in &groups {
        let mut erase = vec![
            0x31,
            0x01,
            (p.erase_routine >> 8) as u8,
            p.erase_routine as u8,
        ];
        erase.extend(erase_record(p, g.erase)?);
        push(ModulePhase::Erase, Some(g.blocks[0]), erase, false);
        for &i in &g.blocks {
            let b = &image.blocks[i];
            let mut dl = vec![0x34, 0x00, p.alfid];
            dl.extend(encode_address_and_length(
                p.alfid,
                b.address,
                b.data.len() as u32,
            )?);
            push(ModulePhase::Download, Some(i), dl, false);
            let mut seq: u8 = 0x01;
            for chunk in b.data.chunks(n) {
                let mut td = vec![0x36, seq];
                td.extend_from_slice(chunk);
                push(ModulePhase::Download, Some(i), td, false);
                seq = seq.wrapping_add(1);
            }
            push(ModulePhase::TransferExit, Some(i), vec![0x37], false);
            for (addr, len) in read_chunks(b.region(), p.read_chunk) {
                push(ModulePhase::Verify, Some(i), read(addr, len)?, false);
            }
            if let Some(rid) = p.check_routine {
                let mut c = vec![0x31, 0x01, (rid >> 8) as u8, rid as u8];
                c.extend(check_record(p, b)?);
                push(ModulePhase::Check, Some(i), c, false);
            }
        }
    }
    if let Some(rid) = p.dependency_routine {
        push(
            ModulePhase::Dependencies,
            None,
            vec![0x31, 0x01, (rid >> 8) as u8, rid as u8],
            false,
        );
    }
    if let Some(kind) = p.reset_type {
        push(ModulePhase::Reset, None, vec![0x11, kind], false);
    }
    let assumptions = vec![
        format!(
            "the ECU reports maxNumberOfBlockLength {assumed_max_block_len} → {n} data bytes per TransferData"
        ),
        "the ECU is locked (a key is sent) and answers every request positively".into(),
        "ReadMemoryByAddress is allowed (backup + read-back verify); otherwise those reads drop out"
            .into(),
        "a fresh run: a resume state would skip finished erase regions after confirming them"
            .into(),
        "the voltage gate runs on the adapter between identification and the backup (no request)"
            .into(),
    ];
    Ok(ModulePlan { steps, assumptions })
}
