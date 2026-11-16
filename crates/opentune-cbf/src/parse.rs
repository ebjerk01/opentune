//! CBF binary → [`CbfFile`] model.
//!
//! The container/ECU/DTC/service/variant layout follows OpenVehicleDiag's
//! `CBFParser` (GPL-3.0). This is a focused re-implementation: it fully decodes
//! the ECU-global DTC pool (the project's primary interest) and the ECU-variant
//! structure, and extracts diagnostic-service *metadata*. It deliberately does
//! not reconstruct service request/response parameter layouts (presentations /
//! freeze-frame environments), which the upstream tool uses for live diagnostic
//! sessions and which are not needed to populate a DTC database.

use crate::error::Result;
use crate::header::{check_stub_header, CffHeader, CtfHeader, CtfLanguage, STUB_HEADER_SIZE};
use crate::model::{
    CbfFile, DiagService, Dtc, Ecu, EcuVariant, Protocol, ServiceType, VariantPattern,
};
use crate::raf::Raf;
use crate::reader::{
    clamp_off, read_bitflag_dump, read_bitflag_string, read_primitive, CaesarPrimitive, PoolTuple,
    MAX_PREALLOC,
};

/// Parse a CBF file from an in-memory byte buffer.
pub fn parse_bytes(data: &[u8]) -> Result<CbfFile> {
    let mut reader = Raf::from_bytes(data);

    // Stub header.
    reader.seek(0);
    let header = reader.read_bytes(STUB_HEADER_SIZE)?;
    let cbf_version = check_stub_header(&header)?;

    // The CFF header is preceded by its own size word.
    let cff_header_size = clamp_off(reader.read_i32()?);
    let _cff_header_bytes = reader.read_bytes(cff_header_size)?;

    let cff = CffHeader::new(&mut reader)?;
    let ctf_offset = cff.base_addr.saturating_add(clamp_off(cff.ctf_offset));
    let ctf = CtfHeader::new(&mut reader, ctf_offset, clamp_off(cff.cff_header_size))?;
    let lang = ctf.language(0)?.clone();

    // ECU table: `ecu_count` 32-bit offsets, each relative to the table. Real CBF
    // files describe one ECU family; clamp the count so a hostile value cannot
    // drive an unbounded (potentially quadratic) re-parse of the shared pools.
    let ecu_table_offset = clamp_off(cff.ecu_offset).saturating_add(cff.base_addr);
    let ecu_count = clamp_off(cff.ecu_count).min(MAX_PREALLOC);
    let mut ecus = Vec::with_capacity(ecu_count);
    for i in 0..ecu_count {
        reader.seek(ecu_table_offset.saturating_add(i.saturating_mul(4)));
        let ecu_offset = clamp_off(reader.read_i32()?);
        let ecu = parse_ecu(
            &mut reader,
            &lang,
            &cff,
            ecu_table_offset.saturating_add(ecu_offset),
        )?;
        ecus.push(ecu);
    }

    Ok(CbfFile {
        cbf_version,
        source_xml: cff.xml_string.clone(),
        ecus,
    })
}

/// A `(offset, count, entry_size, size)` block descriptor pointing at a pool of
/// fixed-size records elsewhere in the file.
#[derive(Debug, Clone, Copy, Default)]
struct Block {
    block_offset: usize,
    entry_count: usize,
    entry_size: usize,
}

impl Block {
    fn new(reader: &mut Raf, bitflags: &mut u32, relative_offset: usize) -> Result<Self> {
        let block_offset = read_primitive(bitflags, reader, 0i32)?
            .to_usize()
            .saturating_add(relative_offset);
        let entry_count = read_primitive(bitflags, reader, 0i32)?.to_usize();
        let entry_size = read_primitive(bitflags, reader, 0i32)?.to_usize();
        let _block_size = read_primitive(bitflags, reader, 0i32)?.to_usize();
        Ok(Self {
            block_offset,
            entry_count,
            entry_size,
        })
    }

    /// Read this block's raw pool bytes.
    fn read_pool(&self, reader: &mut Raf) -> Result<Vec<u8>> {
        reader.seek(self.block_offset);
        // `entry_count * entry_size` can be astronomically large on a hostile
        // descriptor; saturate so it becomes a clean end-of-data error in
        // `read_bytes` rather than overflowing.
        reader.read_bytes(self.entry_count.saturating_mul(self.entry_size))
    }
}

fn parse_ecu(
    reader: &mut Raf,
    lang: &CtfLanguage,
    cff: &CffHeader,
    base_addr: usize,
) -> Result<Ecu> {
    reader.seek(base_addr);
    let mut bitflags = reader.read_u32()?;
    let _bitflags_ext = reader.read_u16()?;
    let _unk0 = reader.read_i32()?;

    let qualifier = read_bitflag_string(&mut bitflags, reader, base_addr)?;
    let name = lang.get_string(read_primitive(&mut bitflags, reader, -1i32)?);
    let description = lang.get_string(read_primitive(&mut bitflags, reader, -1i32)?);
    let _xml_version = read_bitflag_string(&mut bitflags, reader, base_addr)?;
    let _iface_block_count = read_primitive(&mut bitflags, reader, 0i32)?;
    let _iface_table_offset = read_primitive(&mut bitflags, reader, 0i32)?;
    let _sub_iface_count = read_primitive(&mut bitflags, reader, 0i32)?;
    let _sub_iface_offset = read_primitive(&mut bitflags, reader, 0i32)?;
    let _class_name = read_bitflag_string(&mut bitflags, reader, base_addr)?;
    let _unk7 = read_bitflag_string(&mut bitflags, reader, base_addr)?;
    let _unk8 = read_bitflag_string(&mut bitflags, reader, base_addr)?;

    // Pools are stored after the string pool; their offsets are relative to this
    // anchor.
    let data_buffer_offset_relative = clamp_off(cff.string_pool_size)
        .saturating_add(STUB_HEADER_SIZE)
        .saturating_add(clamp_off(cff.cff_header_size))
        .saturating_add(4);

    let _ignition_required = read_primitive(&mut bitflags, reader, 0i16)? > 0;
    let _unk2 = read_primitive(&mut bitflags, reader, 0i16)?;
    let _unk_block_count = read_primitive(&mut bitflags, reader, 0i16)?;
    let _unk_block_offset = read_primitive(&mut bitflags, reader, 0i32)?;
    let _sgml_source = read_primitive(&mut bitflags, reader, 0i16)?;
    let _unk6_relative_offset = read_primitive(&mut bitflags, reader, 0i32)?;

    // The three pools we care about, read in file order.
    let ecu_variant_blk = Block::new(reader, &mut bitflags, data_buffer_offset_relative)?;
    let diag_job_blk = Block::new(reader, &mut bitflags, data_buffer_offset_relative)?;
    let dtc_blk = Block::new(reader, &mut bitflags, data_buffer_offset_relative)?;
    // Remaining blocks (env, vc_domain, presentations, ...) switch to the
    // extended bit-flag word and describe parameter layouts we do not decode, so
    // parsing stops here.

    let dtcs = create_dtcs(reader, lang, &dtc_blk)?;
    let services = create_services(reader, lang, &diag_job_blk)?;
    let variants = create_variants(reader, lang, &ecu_variant_blk, &dtcs, &services)?;

    Ok(Ecu {
        qualifier,
        name,
        description,
        dtcs,
        services,
        variants,
    })
}

fn create_dtcs(reader: &mut Raf, lang: &CtfLanguage, blk: &Block) -> Result<Vec<Dtc>> {
    let pool = blk.read_pool(reader)?;
    let mut pool_reader = Raf::from_bytes(&pool);
    let mut res = Vec::with_capacity(blk.entry_count.min(MAX_PREALLOC));
    for i in 0..blk.entry_count {
        let offset = clamp_off(pool_reader.read_i32()?);
        let _size = pool_reader.read_i32()?;
        let _crc = pool_reader.read_i32()?;
        let dtc_base = offset.saturating_add(blk.block_offset);
        res.push(parse_dtc(reader, lang, dtc_base, i)?);
    }
    Ok(res)
}

fn parse_dtc(
    reader: &mut Raf,
    lang: &CtfLanguage,
    base_addr: usize,
    pool_index: usize,
) -> Result<Dtc> {
    reader.seek(base_addr);
    let mut bitflags = reader.read_u16()? as u32;
    let qualifier = read_bitflag_string(&mut bitflags, reader, base_addr)?;
    let description = lang.get_string(read_primitive(&mut bitflags, reader, -1i32)?);
    let reference = lang.get_string(read_primitive(&mut bitflags, reader, -1i32)?);
    Ok(Dtc {
        qualifier,
        description,
        reference,
        pool_index,
    })
}

fn create_services(reader: &mut Raf, lang: &CtfLanguage, blk: &Block) -> Result<Vec<DiagService>> {
    let pool = blk.read_pool(reader)?;
    let mut pool_reader = Raf::from_bytes(&pool);
    let mut res = Vec::with_capacity(blk.entry_count.min(MAX_PREALLOC));
    for i in 0..blk.entry_count {
        let offset = clamp_off(pool_reader.read_i32()?);
        let _size = pool_reader.read_i32()?;
        let _crc = pool_reader.read_i32()?;
        let _config = pool_reader.read_u16()?;
        let service_base = offset.saturating_add(blk.block_offset);
        res.push(parse_service(reader, lang, service_base, i)?);
    }
    Ok(res)
}

fn parse_service(
    reader: &mut Raf,
    lang: &CtfLanguage,
    base_addr: usize,
    pool_index: usize,
) -> Result<DiagService> {
    reader.seek(base_addr);
    let mut bitflags = reader.read_u32()?;
    let _bitflags_ext = reader.read_u32()?;

    let qualifier = read_bitflag_string(&mut bitflags, reader, base_addr)?;
    let name = lang.get_string(read_primitive(&mut bitflags, reader, -1i32)?);
    let description = lang.get_string(read_primitive(&mut bitflags, reader, -1i32)?);
    let data_class_service_type = read_primitive(&mut bitflags, reader, 0u16)?;
    // The remaining fields describe request/response parameter layouts, which
    // this parser does not reconstruct.

    Ok(DiagService {
        qualifier,
        name,
        description,
        service_type: ServiceType::from_raw(data_class_service_type),
        pool_index,
    })
}

fn create_variants(
    reader: &mut Raf,
    lang: &CtfLanguage,
    blk: &Block,
    global_dtcs: &[Dtc],
    global_services: &[DiagService],
) -> Result<Vec<EcuVariant>> {
    let pool = blk.read_pool(reader)?;
    let mut pool_reader = Raf::from_bytes(&pool);
    let mut res = Vec::with_capacity(blk.entry_count.min(MAX_PREALLOC));
    for _ in 0..blk.entry_count {
        let offset = clamp_off(pool_reader.read_i32()?);
        let size = clamp_off(pool_reader.read_i32()?);
        let _config = pool_reader.read_u16()?;
        let variant_base = offset.saturating_add(blk.block_offset);
        res.push(parse_variant(
            reader,
            lang,
            variant_base,
            size,
            global_dtcs,
            global_services,
        )?);
    }
    Ok(res)
}

#[derive(Debug, Copy, Clone, Default)]
struct DtcPoolBound {
    actual_index: i32,
}

fn parse_variant(
    reader: &mut Raf,
    lang: &CtfLanguage,
    base_addr: usize,
    block_size: usize,
    global_dtcs: &[Dtc],
    global_services: &[DiagService],
) -> Result<EcuVariant> {
    // The variant record is copied out and read from its own base (offset 0).
    reader.seek(base_addr);
    let block = reader.read_bytes(block_size)?;
    let mut tmp = Raf::from_bytes(&block);

    let mut bitflags = tmp.read_u32()?;
    let _skip = tmp.read_u32()?;

    let qualifier = read_bitflag_string(&mut bitflags, &mut tmp, 0)?;
    let name = lang.get_string(read_primitive(&mut bitflags, &mut tmp, -1i32)?);
    let description = lang.get_string(read_primitive(&mut bitflags, &mut tmp, -1i32)?);
    let _unk_str1 = read_bitflag_string(&mut bitflags, &mut tmp, 0)?;
    let _unk_str2 = read_bitflag_string(&mut bitflags, &mut tmp, 0)?;
    let _unk1 = read_primitive(&mut bitflags, &mut tmp, 0i32)?;

    let matching_parent = PoolTuple::new_int(&mut tmp, &mut bitflags)?;
    let _subsection_b = PoolTuple::new_int(&mut tmp, &mut bitflags)?;
    let _com_params = PoolTuple::new_int(&mut tmp, &mut bitflags)?;
    let _diag_service_code = PoolTuple::new_int(&mut tmp, &mut bitflags)?;
    let diag_services = PoolTuple::new_int(&mut tmp, &mut bitflags)?;
    let dtc = PoolTuple::new_int(&mut tmp, &mut bitflags)?;
    let _environment_ctx = PoolTuple::new_int(&mut tmp, &mut bitflags)?;
    let _xref = PoolTuple::new_int(&mut tmp, &mut bitflags)?;
    let _vc_domain = PoolTuple::new_int(&mut tmp, &mut bitflags)?;
    let _negative_response_name = read_bitflag_string(&mut bitflags, &mut tmp, 0)?;
    let _unk_byte = read_primitive(&mut bitflags, &mut tmp, 0u8)?;

    // Which global services this variant exposes.
    tmp.seek(diag_services.offset);
    let mut service_idxs = Vec::with_capacity(diag_services.count.min(MAX_PREALLOC));
    for _ in 0..diag_services.count {
        service_idxs.push(tmp.read_i32()?);
    }
    let service_qualifiers = service_idxs
        .iter()
        .filter_map(|idx| {
            global_services
                .iter()
                .find(|s| s.pool_index == *idx as usize)
                .map(|s| s.qualifier.clone())
        })
        .collect();

    // Which global DTCs this variant exposes.
    tmp.seek(dtc.offset);
    let mut dtc_bounds = Vec::with_capacity(dtc.count.min(MAX_PREALLOC));
    for _ in 0..dtc.count {
        let actual_index = tmp.read_i32()?;
        let _xref_start = tmp.read_i32()?;
        let _xref_count = tmp.read_i32()?;
        dtc_bounds.push(DtcPoolBound { actual_index });
    }
    let dtc_qualifiers = dtc_bounds
        .iter()
        .filter_map(|b| {
            global_dtcs
                .iter()
                .find(|d| d.pool_index == b.actual_index as usize)
                .map(|d| d.qualifier.clone())
        })
        .collect();

    // Variant patterns are read from the main reader at absolute addresses.
    let patterns = create_variant_patterns(reader, base_addr, &matching_parent)?;

    Ok(EcuVariant {
        qualifier,
        name,
        description,
        patterns,
        dtc_qualifiers,
        service_qualifiers,
    })
}

fn create_variant_patterns(
    reader: &mut Raf,
    variant_base: usize,
    matching_parent: &PoolTuple,
) -> Result<Vec<VariantPattern>> {
    let table_offset = variant_base.saturating_add(matching_parent.offset);
    let mut res = Vec::with_capacity(matching_parent.count.min(MAX_PREALLOC));
    for i in 0..matching_parent.count {
        reader.seek(table_offset.saturating_add(i.saturating_mul(4)));
        let ptn_offset = clamp_off(reader.read_i32()?);
        res.push(parse_variant_pattern(
            reader,
            ptn_offset.saturating_add(table_offset),
        )?);
    }
    Ok(res)
}

fn parse_variant_pattern(reader: &mut Raf, base_addr: usize) -> Result<VariantPattern> {
    reader.seek(base_addr);
    let mut bitflags = reader.read_u32()?;

    let unk_buffer_size = read_primitive(&mut bitflags, reader, 0i32)?;
    let _unk_buffer = read_bitflag_dump(
        &mut bitflags,
        reader,
        unk_buffer_size.max(0) as usize,
        base_addr,
    )?;
    let _unk3 = read_primitive(&mut bitflags, reader, 0i32)?;
    let _unk4 = read_primitive(&mut bitflags, reader, 0i32)?;
    let _unk5 = read_primitive(&mut bitflags, reader, 0i32)?;
    let vendor = read_bitflag_string(&mut bitflags, reader, base_addr)?;

    let kwp_vendor_id = read_primitive(&mut bitflags, reader, 0i16)? as i32;
    let _unk8 = read_primitive(&mut bitflags, reader, 0i16)?;
    let _unk9 = read_primitive(&mut bitflags, reader, 0i16)?;
    let _unk10 = read_primitive(&mut bitflags, reader, 0i16)?;
    let _unk11 = read_primitive(&mut bitflags, reader, 0u8)?;
    let _unk12 = read_primitive(&mut bitflags, reader, 0u8)?;
    let _unk13 = read_primitive(&mut bitflags, reader, 0u8)?;
    let _unk14 = read_primitive(&mut bitflags, reader, 0u8)?;
    let _unk15 = read_primitive(&mut bitflags, reader, 0u8)?;
    let _unk16 = read_bitflag_dump(&mut bitflags, reader, 5, base_addr)?;
    let _unk17 = read_primitive(&mut bitflags, reader, 0u8)?;
    let _unk18 = read_primitive(&mut bitflags, reader, 0u8)?;
    let _unk19 = read_primitive(&mut bitflags, reader, 0u8)?;
    let _unk20 = read_primitive(&mut bitflags, reader, 0u8)?;
    let _unk21 = read_bitflag_string(&mut bitflags, reader, base_addr)?;
    let _unk22 = read_primitive(&mut bitflags, reader, 0i32)?;
    let _unk23 = read_primitive(&mut bitflags, reader, 0i32)?;
    let uds_vendor_id = read_primitive(&mut bitflags, reader, 0i32)?;
    let _pattern_type = read_primitive(&mut bitflags, reader, 0i32)?;

    let (protocol, vendor_id) = if uds_vendor_id == 0 {
        (Protocol::Kwp, kwp_vendor_id.max(0) as u32)
    } else {
        (Protocol::Uds, uds_vendor_id.max(0) as u32)
    };

    Ok(VariantPattern {
        vendor,
        vendor_id,
        protocol,
    })
}
