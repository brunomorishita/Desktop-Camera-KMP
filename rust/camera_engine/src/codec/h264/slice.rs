// Slice header (ITU-T H.264 section 7.3.3)
use super::{
    bits::BitReader,
    params::{Pps, Sps},
    ParseError,
};

pub const MAX_REFS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceType {
    P,
    B,
    I,
    Sp,
    Si,
}

impl SliceType {
    pub fn is_intra(self) -> bool {
        matches!(self, SliceType::I | SliceType::Si)
    }

    pub fn list_count(self) -> usize {
        match self {
            SliceType::B => 2,
            SliceType::P | SliceType::Sp => 1,
            SliceType::I | SliceType::Si => 0,
        }
    }

    // Value of slice_type % 5, what VA-API and the spec tables use
    pub fn code(self) -> u8 {
        match self {
            SliceType::P => 0,
            SliceType::B => 1,
            SliceType::I => 2,
            SliceType::Sp => 3,
            SliceType::Si => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListModification {
    // modification_of_pic_nums_idc 0 and 1: abs_diff_pic_num_minus1 + 1, signed
    ShortTerm(i32),
    // modification_of_pic_nums_idc 2
    LongTerm(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryOp {
    // 1: difference_of_pic_nums_minus1 + 1
    UnmarkShortTerm(u32),
    // 2: long_term_pic_num
    UnmarkLongTerm(u32),
    // 3: (difference_of_pic_nums_minus1 + 1, long_term_frame_idx)
    ShortTermToLongTerm(u32, u32),
    // 4: max_long_term_frame_idx_plus1
    SetMaxLongTermIdx(u32),
    // 5
    UnmarkAll,
    // 6: long_term_frame_idx
    CurrentToLongTerm(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WeightEntry {
    pub luma_weight: i16,
    pub luma_offset: i16,
    pub chroma_weight: [i16; 2],
    pub chroma_offset: [i16; 2],
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PredWeightTable {
    pub luma_log2_denom: u32,
    pub chroma_log2_denom: u32,
    // Entries without explicit weights hold the defaults (2^denom, 0)
    pub weights: [Vec<WeightEntry>; 2],
    // Whether any entry of the list was coded explicitly
    pub luma_flags: [bool; 2],
    pub chroma_flags: [bool; 2],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceHeader {
    pub nal_ref_idc: u8,
    pub idr: bool,
    pub first_mb: u32,
    pub slice_type: SliceType,
    pub pps_id: u32,
    pub frame_num: u32,
    pub idr_pic_id: u32,
    pub pic_order_cnt_lsb: u32,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub redundant_pic_cnt: u32,
    pub direct_spatial_mv_pred: bool,
    pub num_ref_idx_active: [u32; 2],
    pub list_modifications: [Vec<ListModification>; 2],
    pub pred_weight_table: Option<PredWeightTable>,
    pub no_output_of_prior_pics: bool,
    pub long_term_reference: bool,
    // None: sliding window marking
    pub memory_ops: Option<Vec<MemoryOp>>,
    pub cabac_init_idc: u32,
    pub slice_qp_delta: i32,
    pub disable_deblocking_filter_idc: u32,
    pub slice_alpha_c0_offset_div2: i32,
    pub slice_beta_offset_div2: i32,
    // Where slice_data() starts, in bits from the NAL header byte, emulation prevention
    // bytes not counted (slice_data_bit_offset of VA-API)
    pub header_bits: usize,
}

// `rbsp` is the whole NAL unit unescaped, header byte included
pub fn parse_slice_header(
    rbsp: &[u8],
    sps_of: impl Fn(u32) -> Option<Sps>,
    pps_of: impl Fn(u32) -> Option<Pps>,
) -> Result<(SliceHeader, Sps, Pps), ParseError> {
    let mut r = BitReader::new(rbsp);
    // forbidden_zero_bit
    r.skip(1)?;
    let nal_ref_idc = r.bits(2)? as u8;
    let nal_unit_type = r.bits(5)?;
    let idr = nal_unit_type == 5;

    let first_mb = r.ue()?;
    let slice_type = match r.ue_max(9, "slice_type")? % 5 {
        0 => SliceType::P,
        1 => SliceType::B,
        2 => SliceType::I,
        3 => SliceType::Sp,
        _ => SliceType::Si,
    };
    let pps_id = r.ue_max(255, "pic_parameter_set_id")?;
    let pps = pps_of(pps_id).ok_or(ParseError::MissingParameterSet("PPS"))?;
    let sps = sps_of(pps.sps_id).ok_or(ParseError::MissingParameterSet("SPS"))?;

    if sps.separate_colour_plane {
        return Err(ParseError::Unsupported(
            "separate colour planes".to_string(),
        ));
    }
    let frame_num = r.bits(sps.log2_max_frame_num)?;
    if !sps.frame_mbs_only && r.bit()? {
        return Err(ParseError::Unsupported(
            "interlaced (field) pictures".to_string(),
        ));
    }

    let idr_pic_id = if idr {
        r.ue_max(65535, "idr_pic_id")?
    } else {
        0
    };
    let mut pic_order_cnt_lsb = 0;
    let mut delta_pic_order_cnt_bottom = 0;
    let mut delta_pic_order_cnt = [0; 2];
    if sps.pic_order_cnt_type == 0 {
        pic_order_cnt_lsb = r.bits(sps.log2_max_pic_order_cnt_lsb)?;
        if pps.bottom_field_pic_order_in_frame_present {
            delta_pic_order_cnt_bottom = r.se()?;
        }
    }
    if sps.pic_order_cnt_type == 1 && !sps.delta_pic_order_always_zero {
        delta_pic_order_cnt[0] = r.se()?;
        if pps.bottom_field_pic_order_in_frame_present {
            delta_pic_order_cnt[1] = r.se()?;
        }
    }
    let redundant_pic_cnt = if pps.redundant_pic_cnt_present {
        r.ue_max(127, "redundant_pic_cnt")?
    } else {
        0
    };
    let direct_spatial_mv_pred = slice_type == SliceType::B && r.bit()?;

    let mut num_ref_idx_active = [
        pps.num_ref_idx_l0_default_active,
        pps.num_ref_idx_l1_default_active,
    ];
    if slice_type.list_count() > 0 && r.bit()? {
        num_ref_idx_active[0] = r.ue_max(31, "num_ref_idx_l0_active_minus1")? + 1;
        if slice_type == SliceType::B {
            num_ref_idx_active[1] = r.ue_max(31, "num_ref_idx_l1_active_minus1")? + 1;
        }
    }
    for unused in num_ref_idx_active.iter_mut().skip(slice_type.list_count()) {
        *unused = 0;
    }

    let mut list_modifications = [Vec::new(), Vec::new()];
    for modifications in list_modifications.iter_mut().take(slice_type.list_count()) {
        if r.bit()? {
            loop {
                match r.ue_max(3, "modification_of_pic_nums_idc")? {
                    0 => modifications.push(ListModification::ShortTerm(-(r.ue()? as i32 + 1))),
                    1 => modifications.push(ListModification::ShortTerm(r.ue()? as i32 + 1)),
                    2 => modifications.push(ListModification::LongTerm(r.ue()?)),
                    _ => break,
                }
                if modifications.len() > MAX_REFS + 1 {
                    return Err(ParseError::Invalid("too many list modifications"));
                }
            }
        }
    }

    let explicit_weights = (pps.weighted_pred
        && matches!(slice_type, SliceType::P | SliceType::Sp))
        || (pps.weighted_bipred_idc == 1 && slice_type == SliceType::B);
    let pred_weight_table = if explicit_weights {
        Some(read_pred_weight_table(
            &mut r,
            &sps,
            slice_type,
            num_ref_idx_active,
        )?)
    } else {
        None
    };

    let mut no_output_of_prior_pics = false;
    let mut long_term_reference = false;
    let mut memory_ops = None;
    if nal_ref_idc != 0 {
        if idr {
            no_output_of_prior_pics = r.bit()?;
            long_term_reference = r.bit()?;
        } else if r.bit()? {
            let mut ops = Vec::new();
            loop {
                let op = match r.ue_max(6, "memory_management_control_operation")? {
                    0 => break,
                    1 => MemoryOp::UnmarkShortTerm(r.ue()? + 1),
                    2 => MemoryOp::UnmarkLongTerm(r.ue()?),
                    3 => MemoryOp::ShortTermToLongTerm(r.ue()? + 1, r.ue()?),
                    4 => MemoryOp::SetMaxLongTermIdx(r.ue()?),
                    5 => MemoryOp::UnmarkAll,
                    _ => MemoryOp::CurrentToLongTerm(r.ue()?),
                };
                ops.push(op);
                if ops.len() > 66 {
                    return Err(ParseError::Invalid("too many memory management operations"));
                }
            }
            memory_ops = Some(ops);
        }
    }

    let cabac_init_idc = if pps.entropy_coding_mode && !slice_type.is_intra() {
        r.ue_max(2, "cabac_init_idc")?
    } else {
        0
    };
    let slice_qp_delta = r.se()?;
    if matches!(slice_type, SliceType::Sp | SliceType::Si) {
        if slice_type == SliceType::Sp {
            // sp_for_switch_flag
            r.skip(1)?;
        }
        // slice_qs_delta
        r.se()?;
    }

    let mut disable_deblocking_filter_idc = 0;
    let mut slice_alpha_c0_offset_div2 = 0;
    let mut slice_beta_offset_div2 = 0;
    if pps.deblocking_filter_control_present {
        disable_deblocking_filter_idc = r.ue_max(2, "disable_deblocking_filter_idc")?;
        if disable_deblocking_filter_idc != 1 {
            slice_alpha_c0_offset_div2 = r.se()?;
            slice_beta_offset_div2 = r.se()?;
        }
    }

    let header = SliceHeader {
        nal_ref_idc,
        idr,
        first_mb,
        slice_type,
        pps_id,
        frame_num,
        idr_pic_id,
        pic_order_cnt_lsb,
        delta_pic_order_cnt_bottom,
        delta_pic_order_cnt,
        redundant_pic_cnt,
        direct_spatial_mv_pred,
        num_ref_idx_active,
        list_modifications,
        pred_weight_table,
        no_output_of_prior_pics,
        long_term_reference,
        memory_ops,
        cabac_init_idc,
        slice_qp_delta,
        disable_deblocking_filter_idc,
        slice_alpha_c0_offset_div2,
        slice_beta_offset_div2,
        header_bits: r.position(),
    };
    Ok((header, sps, pps))
}

// 7.3.3.2
fn read_pred_weight_table(
    r: &mut BitReader,
    sps: &Sps,
    slice_type: SliceType,
    num_ref_idx_active: [u32; 2],
) -> Result<PredWeightTable, ParseError> {
    let mut table = PredWeightTable {
        luma_log2_denom: r.ue_max(7, "luma_log2_weight_denom")?,
        ..Default::default()
    };
    let has_chroma = sps.chroma_array_type() != 0;
    if has_chroma {
        table.chroma_log2_denom = r.ue_max(7, "chroma_log2_weight_denom")?;
    }

    for (list, &active) in num_ref_idx_active
        .iter()
        .enumerate()
        .take(slice_type.list_count())
    {
        for _ in 0..active {
            let mut entry = WeightEntry {
                luma_weight: 1 << table.luma_log2_denom,
                luma_offset: 0,
                chroma_weight: [1 << table.chroma_log2_denom; 2],
                chroma_offset: [0; 2],
            };
            if r.bit()? {
                entry.luma_weight = r.se()? as i16;
                entry.luma_offset = r.se()? as i16;
                table.luma_flags[list] = true;
            }
            if has_chroma && r.bit()? {
                for component in 0..2 {
                    entry.chroma_weight[component] = r.se()? as i16;
                    entry.chroma_offset[component] = r.se()? as i16;
                }
                table.chroma_flags[list] = true;
            }
            table.weights[list].push(entry);
        }
    }
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::h264::{
        params::{parse_pps, parse_sps},
        testing::BitWriter,
    };

    // 640x480 Main profile, POC type 0 with 8 bit LSB, 16 frame numbers
    fn parameter_sets() -> (Sps, Pps) {
        let mut w = BitWriter::default();
        w.bits(77, 8).bits(0, 8).bits(30, 8).ue(0);
        w.ue(0).ue(0).ue(4).ue(3).bits(0, 1);
        w.ue(39).ue(29).bits(1, 1).bits(1, 1).bits(0, 1).bits(0, 1);
        let sps = parse_sps(&w.finish()).unwrap();

        let mut w = BitWriter::default();
        w.ue(0).ue(0).bits(1, 1).bits(0, 1).ue(0).ue(2).ue(0);
        w.bits(0, 1)
            .bits(0, 2)
            .se(0)
            .se(0)
            .se(0)
            .bits(1, 1)
            .bits(0, 1)
            .bits(0, 1);
        let sps_copy = sps.clone();
        let pps = parse_pps(&w.finish(), |_| Some(sps_copy.clone())).unwrap();
        (sps, pps)
    }

    fn parse(bytes: &[u8]) -> SliceHeader {
        let (sps, pps) = parameter_sets();
        parse_slice_header(bytes, |_| Some(sps.clone()), |_| Some(pps.clone()))
            .unwrap()
            .0
    }

    #[test]
    fn parses_an_idr_slice() {
        let mut w = BitWriter::default();
        // nal_ref_idc 3, type 5
        w.bits(0, 1).bits(3, 2).bits(5, 5);
        w.ue(0).ue(7).ue(0).bits(0, 4).ue(1).bits(0, 8);
        // dec_ref_pic_marking, slice_qp_delta, deblocking idc 0 with offsets
        w.bits(0, 1).bits(0, 1).se(-3).ue(0).se(1).se(-1);
        let header_end = w.len();
        let header = parse(&w.finish());

        assert!(header.idr);
        assert_eq!(header.slice_type, SliceType::I);
        assert_eq!(header.idr_pic_id, 1);
        assert_eq!(header.slice_qp_delta, -3);
        assert_eq!(header.num_ref_idx_active, [0, 0]);
        assert_eq!(header.memory_ops, None);
        assert_eq!(
            (
                header.slice_alpha_c0_offset_div2,
                header.slice_beta_offset_div2
            ),
            (1, -1)
        );
        assert_eq!(header.header_bits, header_end);
    }

    #[test]
    fn parses_list_modifications_and_memory_operations() {
        let mut w = BitWriter::default();
        // nal_ref_idc 2, type 1 (non-IDR)
        w.bits(0, 1).bits(2, 2).bits(1, 5);
        // P slice, frame_num 5, POC lsb 10, override to 1 active reference
        w.ue(0).ue(5).ue(0).bits(5, 4).bits(10, 8).bits(1, 1).ue(0);
        // Modifications: subtract 2, add 1, long-term 0, end
        w.bits(1, 1).ue(0).ue(1).ue(1).ue(0).ue(2).ue(0).ue(3);
        // Adaptive marking: mmco 1 (diff 0), mmco 3 (diff 1, idx 2), end
        w.bits(1, 1).ue(1).ue(0).ue(3).ue(1).ue(2).ue(0);
        // cabac_init_idc, slice_qp_delta, deblocking disabled
        w.ue(1).se(2).ue(1);
        let header = parse(&w.finish());

        assert_eq!(header.slice_type, SliceType::P);
        assert_eq!(header.frame_num, 5);
        assert_eq!(header.pic_order_cnt_lsb, 10);
        assert_eq!(header.num_ref_idx_active, [1, 0]);
        assert_eq!(
            header.list_modifications[0],
            [
                ListModification::ShortTerm(-2),
                ListModification::ShortTerm(1),
                ListModification::LongTerm(0)
            ]
        );
        assert_eq!(
            header.memory_ops,
            Some(vec![
                MemoryOp::UnmarkShortTerm(1),
                MemoryOp::ShortTermToLongTerm(2, 2)
            ])
        );
        assert_eq!(header.cabac_init_idc, 1);
        assert_eq!(header.disable_deblocking_filter_idc, 1);
    }
}
