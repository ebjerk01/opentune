//! # opentune-logdef
//!
//! Parse **RomRaider-style `logger.xml`** live-data definitions into an
//! OpenTune live-data parameter model, so OpenTune's logging engine is
//! data-driven. The parser describes *how* a parameter is read and scaled; the
//! [`live`] module then polls the selected parameters over a `RawDiag`
//! transport (UDS `0x22` DIDs, OBD-II mode `0x01` PIDs and UDS `0x23` memory
//! reads) and applies their linear conversions. The [`fast`] module's
//! [`LiveSession`] packs UDS DID and memory channels into dynamically defined
//! DIDs (UDS `0x2C`) and reads them with one request per record, falling back
//! to per-channel polling when the ECU refuses.
//!
//! ## Formats
//!
//! Two open, community-maintained dialects are both "RomRaider-style" and both
//! map onto the same [`LogDef`] model. [`parse`] dispatches on the root element:
//!
//! * **`<logger>`** — the canonical RomRaider logger format described by
//!   `romraider-logger.dtd`: `<protocols><protocol><parameters>/<ecuparams>…`
//!   with `<address>` reads and `<conversions><conversion>` scalings. FastECU
//!   emits this format too (e.g. its Mitsubishi MUT/DMA and Cdbg examples).
//! * **`<ecus>`** — the EcuFlash / FastECU "logprotocols" format:
//!   `<logprotocols><logprotocol><ecu><parameter offset=… expr=… metric=…/>`,
//!   with `<ecu_tools><convert_factor>` named unit conversions and `<alt>`
//!   per-variant alternate addresses.
//!
//! ## Parameter kinds
//!
//! A [`LogParam`]'s [`ProtocolKind`] records which service it is read with and
//! therefore how its [`Addressing`] is interpreted:
//!
//! * [`ProtocolKind::Obd2Mode01`] — OBD-II service `0x01`; addressing is a
//!   single [PID byte](Addressing::ObdPid).
//! * [`ProtocolKind::UdsMode22`] — UDS service `0x22`; addressing is a 2-byte
//!   [DID](Addressing::UdsDid).
//! * [`ProtocolKind::EcuMode23`] — direct memory read ("mode 23" / RomRaider
//!   `<address>`); addressing is a [raw memory address](Addressing::Memory).
//!   Address-read protocols with no OBD/UDS mode (MUT/DMA, Cdbg, …) classify
//!   here.
//! * [`ProtocolKind::SubaruSsm`] — Subaru Select Monitor; addressing is a raw
//!   memory address. (The SSM transport framing is documented in the reference
//!   `ssm2-protocol.txt`; it is **not** implemented here — this is a def parser.)
//!
//! ## Conversions
//!
//! Each `<conversion>` (or Format-B inline `expr`/`metric`) is normalized so
//! the raw value is the single uppercase variable `X` and then fitted with
//! [`opentune_xdf::math::linear_from_equation`] — the exact fitter the XDF and
//! EcuFlash importers use. A linear equation becomes
//! [`ConversionFit::Linear`] `{ factor, offset }`; anything non-linear (`x*x`,
//! or a `GetLogParam("…")` cross-parameter reference) is flagged
//! [`ConversionFit::NonLinear`] rather than silently approximated.
//!
//! ## Text channels
//!
//! A parameter whose [`LogParam::value_kind`] is not
//! [`ValueKind::Number`] (identification DIDs such as `F187`, `F18C`, `F190`)
//! is decoded as ASCII, packed BCD or hex text into [`Sample::text`], with
//! `raw`/`value` left empty. A `storagetype` of `string`/`ascii`, `bcd` or
//! `hex`/`bytes` in a `logger.xml` sets it; a pack's JSON logger sets
//! `"value_kind": "text"` (or `bcd`/`hex`) directly.
//!
//! ## What v1 does not do (recorded, never silent)
//!
//! * Dependencies (`<depends>`/`<dependson>` refs and `GetLogParam` references)
//!   are **captured** in [`LogParam::depends_on`] but not resolved.
//! * `<dtcodes>` (diagnostic trouble codes) and SSM fast-poll packing hints
//!   (Format-B `byte`/`bit` response layout) are not modeled; `<dtcodes>`
//!   presence is noted in [`LogDef::warnings`].
//! * Unknown `storagetype`, missing/unparsable addresses and unrecognised
//!   `mode`s are surfaced as warnings rather than dropped.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod convert;
mod model;
mod parse;

pub use model::{
    Addressing, AltAddress, Conversion, ConversionFit, ConvertFactor, EcuGroup, LogDef, LogParam,
    ProtocolGroup, ProtocolKind, SourceFormat, ValueKind,
};
pub use parse::parse;

pub mod live;
pub use live::{decode_text, poll_once, poll_to_csv, Sample, CSV_HEADER};

pub mod fast;
pub use fast::{
    DynamicDidInfo, FastLogConfig, LiveSession, LiveStatus, LogMode, DEFAULT_DYNAMIC_DID,
    DYNAMIC_DID_RANGE,
};

/// Error parsing a RomRaider-style logger document.
#[derive(Debug)]
pub enum Error {
    /// The XML did not parse.
    Xml(String),
    /// The root element was neither `<logger>` nor `<ecus>`.
    UnknownRoot(String),
    /// The document parsed but contained no protocols/parameters.
    Empty,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Xml(s) => write!(f, "XML parse error: {s}"),
            Error::UnknownRoot(r) => {
                write!(f, "unknown root element <{r}>: expected <logger> or <ecus>")
            }
            Error::Empty => write!(f, "no protocols found in document"),
        }
    }
}
impl std::error::Error for Error {}

/// Crate result type.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use opentune_core::ecudef::{DataType, Endian};

    // ---- Format A: canonical RomRaider <logger> ----

    const SSM_LOGGER: &str = r#"<?xml version="1.0"?>
<logger version="test">
  <protocols>
    <protocol id="SSM" baud="4800">
      <parameters>
        <parameter id="P8" name="Coolant Temperature" desc="ECT">
          <address length="1">0x000008</address>
          <conversions>
            <conversion units="C" expr="x-40" format="0" storagetype="uint8" endian="big" gauge_min="-40" gauge_max="215" gauge_step="10"/>
            <conversion units="F" expr="(x-40)*9/5+32" format="0"/>
          </conversions>
        </parameter>
        <parameter id="P200" name="Dependent" desc="needs P8">
          <depends>
            <ref parameter="P8"/>
          </depends>
          <conversions>
            <conversion units="x" expr="x" format="0"/>
          </conversions>
        </parameter>
      </parameters>
      <switches>
        <switch id="S1" name="AC Switch" desc="AC on" byte="0x61" bit="3"/>
      </switches>
      <ecuparams>
        <ecuparam id="E1" name="Feedback Knock" desc="per-ecu">
          <ecu id="2F12785207">
            <address length="1">0x0000D3</address>
          </ecu>
          <ecu id="AA12345678">
            <address length="1">0x0000E0</address>
          </ecu>
          <conversions>
            <conversion units="count" expr="x" format="0"/>
          </conversions>
        </ecuparam>
      </ecuparams>
    </protocol>
  </protocols>
</logger>"#;

    #[test]
    fn parses_ssm_logger_shape() {
        let def = parse(SSM_LOGGER).unwrap();
        assert_eq!(def.format, SourceFormat::RomRaiderLogger);
        assert_eq!(def.version.as_deref(), Some("test"));
        assert_eq!(def.protocols.len(), 1);
        let proto = &def.protocols[0];
        assert_eq!(proto.id, "SSM");
        assert_eq!(proto.kind, ProtocolKind::SubaruSsm);
        assert_eq!(proto.baud, Some(4800));

        // Standard group (id None) + one per-ECU group per <ecu> id.
        let std_group = proto.ecus.iter().find(|g| g.id.is_none()).unwrap();
        // P8, P200 (depends), and the switch S1.
        assert_eq!(std_group.params.len(), 3);
        let ecu_ids: Vec<_> = proto.ecus.iter().filter_map(|g| g.id.as_deref()).collect();
        assert!(ecu_ids.contains(&"2F12785207") && ecu_ids.contains(&"AA12345678"));
    }

    #[test]
    fn ssm_memory_addressing_and_linear_conversion() {
        let def = parse(SSM_LOGGER).unwrap();
        let (_p, _e, p8) = def.params().find(|(_, _, p)| p.id == "P8").unwrap();
        assert_eq!(
            p8.addressing,
            Addressing::Memory {
                address: 0x08,
                length: 1,
                bit: None
            }
        );
        assert_eq!(p8.storagetype, Some(DataType::U8));
        assert_eq!(p8.endian, Endian::Big);
        assert_eq!(p8.units, "C");
        assert_eq!(p8.min, Some(-40.0));
        assert_eq!(p8.max, Some(215.0));
        assert_eq!(p8.conversions.len(), 2);
        match p8.conversion.as_ref().unwrap().fit {
            ConversionFit::Linear { factor, offset } => {
                assert!((factor - 1.0).abs() < 1e-12 && (offset + 40.0).abs() < 1e-12);
            }
            ref other => panic!("expected linear, got {other:?}"),
        }
        // Second conversion (°F) is still affine.
        assert!(matches!(
            p8.conversions[1].fit,
            ConversionFit::Linear { .. }
        ));
    }

    #[test]
    fn depends_is_flagged_not_resolved() {
        let def = parse(SSM_LOGGER).unwrap();
        let (_p, _e, dep) = def.params().find(|(_, _, p)| p.id == "P200").unwrap();
        assert_eq!(dep.depends_on, vec!["P8".to_string()]);
        // Depends-only parameter has no address of its own.
        assert_eq!(dep.addressing, Addressing::Unknown);
    }

    #[test]
    fn switch_is_single_bit_flag() {
        let def = parse(SSM_LOGGER).unwrap();
        let (_p, _e, sw) = def.params().find(|(_, _, p)| p.id == "S1").unwrap();
        assert_eq!(
            sw.addressing,
            Addressing::Memory {
                address: 0x61,
                length: 1,
                bit: Some(3)
            }
        );
    }

    #[test]
    fn ecuparam_expands_per_ecu() {
        let def = parse(SSM_LOGGER).unwrap();
        let e1: Vec<_> = def.params().filter(|(_, _, p)| p.id == "E1").collect();
        assert_eq!(e1.len(), 2);
        let addrs: Vec<_> = e1.iter().map(|(_, _, p)| p.addressing.clone()).collect();
        assert!(addrs.contains(&Addressing::Memory {
            address: 0xD3,
            length: 1,
            bit: None
        }));
        assert!(addrs.contains(&Addressing::Memory {
            address: 0xE0,
            length: 1,
            bit: None
        }));
    }

    // ---- OBD / UDS modes ----

    const OBD_LOGGER: &str = r#"<logger>
  <protocols>
    <protocol id="OBD" baud="500000">
      <parameters mode="1">
        <parameter id="P0C" name="Engine RPM (OBD)" desc="pid 0C">
          <address>0x0C</address>
          <conversions><conversion units="rpm" expr="x/4" format="0"/></conversions>
        </parameter>
      </parameters>
      <parameters mode="22">
        <parameter id="D2001" name="DID 2001" desc="uds did">
          <address length="2">0x2001</address>
          <conversions><conversion units="raw" expr="x" format="0"/></conversions>
        </parameter>
      </parameters>
    </protocol>
  </protocols>
</logger>"#;

    #[test]
    fn obd_mode01_is_pid_and_uds_mode22_is_did() {
        let def = parse(OBD_LOGGER).unwrap();
        let (_p, _e, pid) = def.params().find(|(_, _, p)| p.id == "P0C").unwrap();
        assert_eq!(pid.protocol, ProtocolKind::Obd2Mode01);
        assert_eq!(pid.addressing, Addressing::ObdPid { pid: 0x0C });
        let (_p, _e, did) = def.params().find(|(_, _, p)| p.id == "D2001").unwrap();
        assert_eq!(did.protocol, ProtocolKind::UdsMode22);
        assert_eq!(did.addressing, Addressing::UdsDid { did: 0x2001 });
    }

    // ---- Format B: EcuFlash / FastECU <ecus> ----

    const ECUS_SSM: &str = r##"<ecus>
  <ecu_tools>
    <convert_factor type="temp" name="Convert Celsius to Fahrenheit" metric="F" expr="([value]*9/5)+32"/>
    <convert_factor type="inj" name="Injector ms to Duty %" metric="%" expr="GetLogParam(&quot;Engine Speed&quot;)*([value]/1200)"/>
  </ecu_tools>
  <logprotocols>
    <logprotocol type="SSM" default="ssmbase">
      <ecu id="base" name="SSM base" type="ssmbase">
        <parameter id="Engine Speed" offset="#000E" storagetype="uint16" decimals="0" bit="1" byte="1" expr="[value]/4" metric="RPM" desc="rpm">
          <alt id="Alt A" offset="#20118"/>
          <alt id="Alt B" offset="#20120"/>
        </parameter>
        <parameter id="Coolant Temperature" offset="#0008" storagetype="uint8" decimals="2" type="temp" bit="7" byte="1" expr="([value]-40)" metric="B C" desc="ect"/>
        <parameter id="Injector Duty" offset="#0021" storagetype="uint8" type="inj" expr="GetLogParam(&quot;Engine Speed&quot;)*([value]/1200)" metric="%" desc="dep"/>
      </ecu>
    </logprotocol>
  </logprotocols>
</ecus>"##;

    #[test]
    fn parses_ecuflash_logprotocols() {
        let def = parse(ECUS_SSM).unwrap();
        assert_eq!(def.format, SourceFormat::EcuFlashLogProtocols);
        assert_eq!(def.convert_factors.len(), 2);
        assert_eq!(def.protocols.len(), 1);
        let proto = &def.protocols[0];
        assert_eq!(proto.id, "SSM");
        assert_eq!(proto.kind, ProtocolKind::SubaruSsm);
        let base = &proto.ecus[0];
        assert_eq!(base.name.as_deref(), Some("SSM base"));
        assert_eq!(base.params.len(), 3);
    }

    #[test]
    fn ecuflash_param_address_conversion_and_alts() {
        let def = parse(ECUS_SSM).unwrap();
        let (_p, _e, rpm) = def
            .params()
            .find(|(_, _, p)| p.id == "Engine Speed")
            .unwrap();
        assert_eq!(
            rpm.addressing,
            Addressing::Memory {
                address: 0x0E,
                length: 2,
                bit: None
            }
        );
        assert_eq!(rpm.storagetype, Some(DataType::U16));
        assert_eq!(rpm.units, "RPM");
        // [value] normalized to X and fitted linear.
        assert_eq!(rpm.conversion.as_ref().unwrap().expr, "X/4");
        assert!(matches!(
            rpm.conversion.as_ref().unwrap().fit,
            ConversionFit::Linear { .. }
        ));
        assert_eq!(rpm.alternates.len(), 2);
        assert_eq!(
            rpm.alternates[0].addressing,
            Addressing::Memory {
                address: 0x20118,
                length: 2,
                bit: None
            }
        );
    }

    #[test]
    fn ecuflash_convert_group_and_dependency() {
        let def = parse(ECUS_SSM).unwrap();
        let (_p, _e, ect) = def
            .params()
            .find(|(_, _, p)| p.id == "Coolant Temperature")
            .unwrap();
        assert_eq!(ect.convert_group.as_deref(), Some("temp"));
        // GetLogParam reference captured as a dependency and flagged non-linear.
        let (_p, _e, inj) = def
            .params()
            .find(|(_, _, p)| p.id == "Injector Duty")
            .unwrap();
        assert_eq!(inj.depends_on, vec!["Engine Speed".to_string()]);
        assert!(matches!(
            inj.conversion.as_ref().unwrap().fit,
            ConversionFit::NonLinear { .. }
        ));
        // The convert_factor with a GetLogParam expr is also flagged.
        let inj_cf = def
            .convert_factors
            .iter()
            .find(|c| c.kind == "inj")
            .unwrap();
        assert!(matches!(inj_cf.fit, ConversionFit::NonLinear { .. }));
    }

    // ---- Errors & serde ----

    #[test]
    fn text_storagetypes_set_the_value_kind() {
        let xml = r##"<logger><protocols><protocol id="UDS">
          <parameters mode="22">
            <parameter id="VIN" name="VIN" desc="">
              <address length="17">0xF190</address>
              <conversions><conversion units="" expr="x" storagetype="string"/></conversions>
            </parameter>
            <parameter id="RPM" name="RPM" desc="">
              <address length="2">0xF40C</address>
              <conversions><conversion units="rpm" expr="x/4" storagetype="uint16"/></conversions>
            </parameter>
          </parameters></protocol></protocols></logger>"##;
        let def = parse(xml).unwrap();
        let kind = |id: &str| {
            def.params()
                .find(|(_, _, p)| p.id == id)
                .unwrap()
                .2
                .value_kind
        };
        assert_eq!(kind("VIN"), ValueKind::Text);
        assert_eq!(kind("RPM"), ValueKind::Number);

        let ecus = r##"<ecus><logprotocols><logprotocol type="SSM"><ecu id="b">
          <parameter id="Date" offset="#0100" storagetype="bcd" expr="[value]" metric=""/>
        </ecu></logprotocol></logprotocols></ecus>"##;
        let def = parse(ecus).unwrap();
        assert_eq!(def.params().next().unwrap().2.value_kind, ValueKind::Bcd);
        assert!(
            def.warnings.iter().all(|w| !w.contains("storagetype")),
            "{:?}",
            def.warnings
        );

        // Pack JSON: absent = number (and not written back), `ascii` aliases text.
        let (_, _, p) = parse(SSM_LOGGER)
            .unwrap()
            .params()
            .next()
            .map(|(a, b, c)| (a.clone(), b.clone(), c.clone()))
            .unwrap();
        let mut v = serde_json::to_value(&p).unwrap();
        assert!(v.get("value_kind").is_none());
        v["value_kind"] = "ascii".into();
        let back: LogParam = serde_json::from_value(v).unwrap();
        assert_eq!(back.value_kind, ValueKind::Text);
        assert_eq!(serde_json::to_value(&back).unwrap()["value_kind"], "text");
    }

    #[test]
    fn unknown_root_errors() {
        assert!(matches!(parse("<nope/>"), Err(Error::UnknownRoot(_))));
        assert!(matches!(parse("<<<"), Err(Error::Xml(_))));
    }

    #[test]
    fn model_roundtrips_through_json() {
        let def = parse(SSM_LOGGER).unwrap();
        let json = serde_json::to_string(&def).unwrap();
        let back: LogDef = serde_json::from_str(&json).unwrap();
        assert_eq!(def, back);
    }
}
