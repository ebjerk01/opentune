//! BLE GATT link over [`btleplug`] (feature `btleplug`, off by default).
//!
//! Scans for the bridge's service (`0xABF0`), connects to the device with the
//! requested advertised name (or address), subscribes to the notify
//! characteristic (`0xABF2`) and writes frames to `0xABF1`. btleplug is async;
//! this link owns a small tokio runtime and presents the blocking [`Link`]
//! interface the rest of the driver uses.
//!
//! **Unverified**: compiled, never run against a real bridge.
use crate::codec::{
    uuid16_to_u128, DATA_NOTIFY_UUID16, DATA_WRITE_UUID16, DEFAULT_ATT_MTU, SERVICE_UUID16,
};
use crate::link::{Framing, Link};
use btleplug::api::{
    Central, CharPropFlags, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use opentune_core::{Error, Result};
use std::sync::mpsc;
use std::time::Duration;
use uuid::Uuid;

/// What to connect to.
#[derive(Debug, Clone)]
pub struct BleOptions {
    /// Advertised GAP name (the firmware default is `BLE_TO_ISOTP20`).
    pub name: String,
    /// Peripheral address or platform id; takes precedence over `name`.
    pub address: Option<String>,
    /// How long to scan before giving up.
    pub scan_timeout: Duration,
    /// ATT MTU to assume instead of the negotiated one.
    pub att_mtu: Option<usize>,
}

/// A connected bridge over BLE.
pub struct BleLink {
    rt: tokio::runtime::Runtime,
    peripheral: Peripheral,
    write_char: Characteristic,
    write_type: WriteType,
    rx: mpsc::Receiver<Vec<u8>>,
    att_payload: usize,
    label: String,
}

fn ble_err(e: btleplug::Error) -> Error {
    Error::Io(std::io::Error::other(format!("BLE: {e}")))
}

fn other(msg: String) -> Error {
    Error::Io(std::io::Error::other(msg))
}

fn uuid16(short: u16) -> Uuid {
    Uuid::from_u128(uuid16_to_u128(short))
}

async fn find(adapter: &Adapter, opts: &BleOptions) -> Result<Option<Peripheral>> {
    for p in adapter.peripherals().await.map_err(ble_err)? {
        if let Some(addr) = &opts.address {
            if p.address().to_string().eq_ignore_ascii_case(addr)
                || p.id().to_string().eq_ignore_ascii_case(addr)
            {
                return Ok(Some(p));
            }
            continue;
        }
        let props = p.properties().await.map_err(ble_err)?.unwrap_or_default();
        let name = props.local_name.or(props.advertisement_name);
        if name.as_deref() == Some(opts.name.as_str()) {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

impl BleLink {
    /// Scan, connect and subscribe.
    pub fn connect(opts: &BleOptions) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        let (tx, rx) = mpsc::channel();
        let notify_uuid = uuid16(DATA_NOTIFY_UUID16);
        let write_uuid = uuid16(DATA_WRITE_UUID16);
        let (peripheral, write_char, label) = rt.block_on(async {
            let manager = Manager::new().await.map_err(ble_err)?;
            let adapter = manager
                .adapters()
                .await
                .map_err(ble_err)?
                .into_iter()
                .next()
                .ok_or_else(|| other("BLE: no Bluetooth adapter found".into()))?;
            adapter
                .start_scan(ScanFilter {
                    services: vec![uuid16(SERVICE_UUID16)],
                })
                .await
                .map_err(ble_err)?;
            let deadline = tokio::time::Instant::now() + opts.scan_timeout;
            let found = loop {
                if let Some(p) = find(&adapter, opts).await? {
                    break Some(p);
                }
                if tokio::time::Instant::now() >= deadline {
                    break None;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            };
            let _ = adapter.stop_scan().await;
            let want = opts.address.as_deref().unwrap_or(&opts.name);
            let p = found.ok_or_else(|| {
                other(format!(
                    "BLE: no ISO-TP bridge '{want}' found within {} ms",
                    opts.scan_timeout.as_millis()
                ))
            })?;
            p.connect().await.map_err(ble_err)?;
            p.discover_services().await.map_err(ble_err)?;
            let chars = p.characteristics();
            let write_char = chars
                .iter()
                .find(|c| c.uuid == write_uuid)
                .cloned()
                .ok_or_else(|| other("BLE: write characteristic 0xABF1 missing".into()))?;
            let notify_char = chars
                .iter()
                .find(|c| c.uuid == notify_uuid)
                .cloned()
                .ok_or_else(|| other("BLE: notify characteristic 0xABF2 missing".into()))?;
            let mut stream = p.notifications().await.map_err(ble_err)?;
            p.subscribe(&notify_char).await.map_err(ble_err)?;
            tokio::spawn(async move {
                while let Some(n) = stream.next().await {
                    if n.uuid == notify_uuid && tx.send(n.value).is_err() {
                        break;
                    }
                }
            });
            let label = format!("BLE {want} ({})", p.address());
            Ok::<_, Error>((p, write_char, label))
        })?;
        let write_type = if write_char
            .properties
            .contains(CharPropFlags::WRITE_WITHOUT_RESPONSE)
        {
            WriteType::WithoutResponse
        } else {
            WriteType::WithResponse
        };
        let mtu = opts
            .att_mtu
            .unwrap_or(peripheral.mtu() as usize)
            .max(DEFAULT_ATT_MTU);
        Ok(Self {
            rt,
            peripheral,
            write_char,
            write_type,
            rx,
            att_payload: mtu - 3,
            label,
        })
    }
}

impl Link for BleLink {
    fn framing(&self) -> Framing {
        Framing::Ble {
            att_payload: self.att_payload,
        }
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.rt
            .block_on(
                self.peripheral
                    .write(&self.write_char, bytes, self.write_type),
            )
            .map_err(ble_err)
    }

    fn read(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let r = if timeout.is_zero() {
            self.rx.try_recv().map_err(|e| match e {
                mpsc::TryRecvError::Empty => None,
                mpsc::TryRecvError::Disconnected => Some(()),
            })
        } else {
            self.rx.recv_timeout(timeout).map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => None,
                mpsc::RecvTimeoutError::Disconnected => Some(()),
            })
        };
        match r {
            Ok(v) => Ok(Some(v)),
            Err(None) => Ok(None),
            Err(Some(())) => Err(other(
                "BLE: notification stream ended (bridge disconnected?)".into(),
            )),
        }
    }

    fn describe(&self) -> String {
        self.label.clone()
    }
}

impl Drop for BleLink {
    fn drop(&mut self) {
        let _ = self.rt.block_on(self.peripheral.disconnect());
    }
}
