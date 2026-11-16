//! Real J2534 PassThru DLL binding (Windows-only).
//!
//! Compiled only under `#[cfg(all(windows, feature = "passthru-ffi"))]`. It
//! loads a vendor PassThru DLL with `libloading` and implements the [`PassThru`]
//! backend over the SAE J2534-1 C ABI. This has **not** been exercised against
//! real hardware; it is the documented place the Windows FFI lives so the rest
//! of the crate stays portable and testable on Linux.
//!
//! The C `PASSTHRU_MSG` layout and the `extern "system"` (stdcall on win32)
//! function signatures follow SAE J2534-1.

use crate::codec::{PassThruMsg, STATUS_NOERROR};
use crate::PassThru;
use libloading::{Library, Symbol};
use opentune_core::{Error, Result};

const MAX_DATA: usize = 4128;

#[repr(C)]
#[derive(Clone, Copy)]
struct CPassThruMsg {
    protocol_id: u32,
    rx_status: u32,
    tx_flags: u32,
    timestamp: u32,
    data_size: u32,
    extra_data_index: u32,
    data: [u8; MAX_DATA],
}

impl CPassThruMsg {
    fn zeroed() -> Self {
        Self {
            protocol_id: 0,
            rx_status: 0,
            tx_flags: 0,
            timestamp: 0,
            data_size: 0,
            extra_data_index: 0,
            data: [0u8; MAX_DATA],
        }
    }

    fn from_msg(m: &PassThruMsg) -> Self {
        let mut c = Self::zeroed();
        c.protocol_id = m.protocol_id;
        c.rx_status = m.rx_status;
        c.tx_flags = m.tx_flags;
        let n = m.data.len().min(MAX_DATA);
        c.data[..n].copy_from_slice(&m.data[..n]);
        c.data_size = n as u32;
        c
    }

    fn to_msg(self) -> PassThruMsg {
        let n = (self.data_size as usize).min(MAX_DATA);
        PassThruMsg {
            protocol_id: self.protocol_id,
            rx_status: self.rx_status,
            tx_flags: self.tx_flags,
            data: self.data[..n].to_vec(),
        }
    }
}

type FnOpen = unsafe extern "system" fn(*const core::ffi::c_void, *mut u32) -> i32;
type FnClose = unsafe extern "system" fn(u32) -> i32;
type FnConnect = unsafe extern "system" fn(u32, u32, u32, u32, *mut u32) -> i32;
type FnDisconnect = unsafe extern "system" fn(u32) -> i32;
type FnReadMsgs = unsafe extern "system" fn(u32, *mut CPassThruMsg, *mut u32, u32) -> i32;
type FnWriteMsgs = unsafe extern "system" fn(u32, *mut CPassThruMsg, *mut u32, u32) -> i32;
type FnStartFilter = unsafe extern "system" fn(
    u32,
    u32,
    *mut CPassThruMsg,
    *mut CPassThruMsg,
    *mut CPassThruMsg,
    *mut u32,
) -> i32;

/// A loaded PassThru DLL plus the open device/channel handles.
pub struct PassThruDll {
    lib: Library,
    device_id: u32,
    channel_id: u32,
}

impl PassThruDll {
    /// Load the DLL at `path` and `PassThruOpen` the device.
    pub fn load(path: &str) -> Result<Self> {
        // SAFETY: loading a user-specified native library; the caller controls
        // the path (from ConnectConfig options["library"]).
        let lib = unsafe { Library::new(path) }
            .map_err(|e| Error::Config(format!("cannot load J2534 DLL {path}: {e}")))?;
        let mut dll = Self {
            lib,
            device_id: 0,
            channel_id: 0,
        };
        let mut device_id: u32 = 0;
        let rc = unsafe {
            let open: Symbol<FnOpen> = dll
                .lib
                .get(b"PassThruOpen\0")
                .map_err(|e| Error::Config(format!("PassThruOpen missing: {e}")))?;
            open(core::ptr::null(), &mut device_id)
        };
        check(rc, "PassThruOpen")?;
        dll.device_id = device_id;
        Ok(dll)
    }

    unsafe fn sym<T>(&self, name: &[u8]) -> Result<Symbol<'_, T>> {
        self.lib
            .get(name)
            .map_err(|e| Error::Config(format!("J2534 symbol missing: {e}")))
    }
}

fn check(rc: i32, what: &str) -> Result<()> {
    if rc as u32 == STATUS_NOERROR {
        Ok(())
    } else {
        Err(Error::Protocol(format!(
            "{what} failed: J2534 err 0x{rc:X}"
        )))
    }
}

impl PassThru for PassThruDll {
    fn connect(&mut self, protocol_id: u32, flags: u32, baud: u32) -> Result<()> {
        let mut channel: u32 = 0;
        let rc = unsafe {
            let connect: Symbol<FnConnect> = self.sym(b"PassThruConnect\0")?;
            connect(self.device_id, protocol_id, flags, baud, &mut channel)
        };
        check(rc, "PassThruConnect")?;
        self.channel_id = channel;
        Ok(())
    }

    fn start_msg_filter(
        &mut self,
        filter_type: u32,
        mask: &PassThruMsg,
        pattern: &PassThruMsg,
        flow_control: Option<&PassThruMsg>,
    ) -> Result<()> {
        let mut c_mask = CPassThruMsg::from_msg(mask);
        let mut c_pat = CPassThruMsg::from_msg(pattern);
        let mut c_flow = flow_control.map(CPassThruMsg::from_msg);
        let mut msg_id: u32 = 0;
        let rc = unsafe {
            let start: Symbol<FnStartFilter> = self.sym(b"PassThruStartMsgFilter\0")?;
            let flow_ptr = c_flow
                .as_mut()
                .map(|f| f as *mut CPassThruMsg)
                .unwrap_or(core::ptr::null_mut());
            start(
                self.channel_id,
                filter_type,
                &mut c_mask,
                &mut c_pat,
                flow_ptr,
                &mut msg_id,
            )
        };
        check(rc, "PassThruStartMsgFilter")
    }

    fn write_msg(&mut self, msg: &PassThruMsg, timeout_ms: u64) -> Result<()> {
        let mut c = CPassThruMsg::from_msg(msg);
        let mut num: u32 = 1;
        let rc = unsafe {
            let write: Symbol<FnWriteMsgs> = self.sym(b"PassThruWriteMsgs\0")?;
            write(self.channel_id, &mut c, &mut num, timeout_ms as u32)
        };
        check(rc, "PassThruWriteMsgs")
    }

    fn read_msg(&mut self, timeout_ms: u64) -> Result<PassThruMsg> {
        let mut c = CPassThruMsg::zeroed();
        let mut num: u32 = 1;
        let rc = unsafe {
            let read: Symbol<FnReadMsgs> = self.sym(b"PassThruReadMsgs\0")?;
            read(self.channel_id, &mut c, &mut num, timeout_ms as u32)
        };
        if num == 0 {
            return Err(Error::Timeout(timeout_ms));
        }
        check(rc, "PassThruReadMsgs")?;
        Ok(c.to_msg())
    }

    fn close(&mut self) -> Result<()> {
        unsafe {
            if let Ok(disc) = self.sym::<FnDisconnect>(b"PassThruDisconnect\0") {
                let _ = disc(self.channel_id);
            }
            if let Ok(close) = self.sym::<FnClose>(b"PassThruClose\0") {
                let _ = close(self.device_id);
            }
        }
        Ok(())
    }
}
