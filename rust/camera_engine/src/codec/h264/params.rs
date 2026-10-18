// Sequence and picture parameter sets (ITU-T H.264 sections 7.3.2.1 and 7.3.2.2)
use super::{bits::BitReader, ParseError};

// Scaling lists are kept in the order they are coded (zig-zag), which is what VA-API takes
pub type ScalingList4x4 = [[u8; 16]; 6];
pub type ScalingList8x8 = [[u8; 64]; 2];

const FLAT_4X4: [u8; 16] = [16; 16];
const FLAT_8X8: [u8; 64] = [16; 64];

// Table 7-3 and 7-4, zig-zag order
const DEFAULT_4X4_INTRA: [u8; 16] = [
    6, 13, 13, 20, 20, 20, 28, 28, 28, 28, 32, 32, 32, 37, 37, 42,
];
const DEFAULT_4X4_INTER: [u8; 16] = [
    10, 14, 14, 20, 20, 20, 24, 24, 24, 24, 27, 27, 27, 30, 30, 34,
];
const DEFAULT_8X8_INTRA: [u8; 64] = [
    6, 10, 10, 13, 11, 13, 16, 16, 16, 16, 18, 18, 18, 18, 18, 23, 23, 23, 23, 23, 23, 25, 25, 25,
    25, 25, 25, 25, 27, 27, 27, 27, 27, 27, 27, 27, 29, 29, 29, 29, 29, 29, 29, 31, 31, 31, 31, 31,
    31, 33, 33, 33, 33, 33, 36, 36, 36, 36, 38, 38, 38, 40, 40, 42,
];
const DEFAULT_8X8_INTER: [u8; 64] = [
    9, 13, 13, 15, 13, 15, 17, 17, 17, 17, 19, 19, 19, 19, 19, 21, 21, 21, 21, 21, 21, 22, 22, 22,
    22, 22, 22, 22, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 27, 27, 27, 27, 27,
    27, 28, 28, 28, 28, 28, 30, 30, 30, 30, 32, 32, 32, 33, 33, 35,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sps {
    pub id: u32,
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub chroma_format_idc: u32,
    pub separate_colour_plane: bool,
    pub bit_depth_luma_minus8: u32,
    pub bit_depth_chroma_minus8: u32,
    // seq_scaling_matrix_present_flag: decides the PPS fall-back rule (A or B)
    pub scaling_matrix_present: bool,
    pub scaling_4x4: ScalingList4x4,
    pub scaling_8x8: ScalingList8x8,
    pub log2_max_frame_num: u32,
    pub pic_order_cnt_type: u32,
    pub log2_max_pic_order_cnt_lsb: u32,
    pub delta_pic_order_always_zero: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: Vec<i32>,
    pub max_num_ref_frames: u32,
    pub gaps_in_frame_num_allowed: bool,
    pub width_in_mbs: u32,
    pub height_in_map_units: u32,
    pub frame_mbs_only: bool,
    pub mb_adaptive_frame_field: bool,
    pub direct_8x8_inference: bool,
    // Pixels to remove on each side: left, right, top, bottom
    pub crop: [u32; 4],
    pub full_range: bool,
    pub matrix_coefficients: u8,
    // From the VUI bitstream restriction, when present
    pub max_num_reorder_frames: Option<u32>,
    pub max_dec_frame_buffering: Option<u32>,
}

impl Sps {
    pub fn max_frame_num(&self) -> u32 {
        1 << self.log2_max_frame_num
    }

    pub fn height_in_mbs(&self) -> u32 {
        self.height_in_map_units * if self.frame_mbs_only { 1 } else { 2 }
    }

    pub fn coded_size(&self) -> (u32, u32) {
        (self.width_in_mbs * 16, self.height_in_mbs() * 16)
    }

    // Visible picture after the frame cropping: (x, y, width, height)
    pub fn visible_rect(&self) -> (u32, u32, u32, u32) {
        let (width, height) = self.coded_size();
        let [left, right, top, bottom] = self.crop;
        (
            left,
            top,
            width.saturating_sub(left + right),
            height.saturating_sub(top + bottom),
        )
    }

    pub fn chroma_array_type(&self) -> u32 {
        if self.separate_colour_plane {
            0
        } else {
            self.chroma_format_idc
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pps {
    pub id: u32,
    pub sps_id: u32,
    pub entropy_coding_mode: bool,
    pub bottom_field_pic_order_in_frame_present: bool,
    pub num_ref_idx_l0_default_active: u32,
    pub num_ref_idx_l1_default_active: u32,
    pub weighted_pred: bool,
    pub weighted_bipred_idc: u32,
    pub pic_init_qp_minus26: i32,
    pub pic_init_qs_minus26: i32,
    pub chroma_qp_index_offset: i32,
    pub deblocking_filter_control_present: bool,
    pub constrained_intra_pred: bool,
    pub redundant_pic_cnt_present: bool,
    pub transform_8x8_mode: bool,
    pub second_chroma_qp_index_offset: i32,
    // Final lists for pictures using this PPS, fallbacks already applied
    pub scaling_4x4: ScalingList4x4,
    pub scaling_8x8: ScalingList8x8,
}

// rbsp of a NAL unit of type 7, without the NAL header byte
pub fn parse_sps(rbsp: &[u8]) -> Result<Sps, ParseError> {
    let mut r = BitReader::new(rbsp);
    let profile_idc = r.bits(8)? as u8;
    let constraint_flags = r.bits(8)? as u8;
    let level_idc = r.bits(8)? as u8;
    let id = r.ue_max(31, "seq_parameter_set_id")?;

    let mut sps = Sps {
        id,
        profile_idc,
        constraint_flags,
        level_idc,
        chroma_format_idc: 1,
        separate_colour_plane: false,
        bit_depth_luma_minus8: 0,
        bit_depth_chroma_minus8: 0,
        scaling_matrix_present: false,
        scaling_4x4: [FLAT_4X4; 6],
        scaling_8x8: [FLAT_8X8; 2],
        log2_max_frame_num: 0,
        pic_order_cnt_type: 0,
        log2_max_pic_order_cnt_lsb: 0,
        delta_pic_order_always_zero: false,
        offset_for_non_ref_pic: 0,
        offset_for_top_to_bottom_field: 0,
        offset_for_ref_frame: Vec::new(),
        max_num_ref_frames: 0,
        gaps_in_frame_num_allowed: false,
        width_in_mbs: 0,
        height_in_map_units: 0,
        frame_mbs_only: true,
        mb_adaptive_frame_field: false,
        direct_8x8_inference: false,
        crop: [0; 4],
        full_range: false,
        // Unspecified
        matrix_coefficients: 2,
        max_num_reorder_frames: None,
        max_dec_frame_buffering: None,
    };

    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        sps.chroma_format_idc = r.ue_max(3, "chroma_format_idc")?;
        if sps.chroma_format_idc == 3 {
            sps.separate_colour_plane = r.bit()?;
        }
        sps.bit_depth_luma_minus8 = r.ue_max(6, "bit_depth_luma_minus8")?;
        sps.bit_depth_chroma_minus8 = r.ue_max(6, "bit_depth_chroma_minus8")?;
        // qpprime_y_zero_transform_bypass_flag
        r.bit()?;
        sps.scaling_matrix_present = r.bit()?;
        if sps.scaling_matrix_present {
            let count = if sps.chroma_format_idc == 3 { 12 } else { 8 };
            read_scaling_matrix(
                &mut r,
                count,
                &mut sps.scaling_4x4,
                &mut sps.scaling_8x8,
                // Fall-back rule A: the defaults
                (&DEFAULT_4X4_INTRA, &DEFAULT_4X4_INTER),
                (&DEFAULT_8X8_INTRA, &DEFAULT_8X8_INTER),
            )?;
        }
    }

    sps.log2_max_frame_num = r.ue_max(12, "log2_max_frame_num_minus4")? + 4;
    sps.pic_order_cnt_type = r.ue_max(2, "pic_order_cnt_type")?;
    match sps.pic_order_cnt_type {
        0 => {
            sps.log2_max_pic_order_cnt_lsb = r.ue_max(12, "log2_max_pic_order_cnt_lsb_minus4")? + 4
        }
        1 => {
            sps.delta_pic_order_always_zero = r.bit()?;
            sps.offset_for_non_ref_pic = r.se()?;
            sps.offset_for_top_to_bottom_field = r.se()?;
            let cycle = r.ue_max(255, "num_ref_frames_in_pic_order_cnt_cycle")?;
            for _ in 0..cycle {
                sps.offset_for_ref_frame.push(r.se()?);
            }
        }
        _ => {}
    }

    sps.max_num_ref_frames = r.ue_max(16, "max_num_ref_frames")?;
    sps.gaps_in_frame_num_allowed = r.bit()?;
    sps.width_in_mbs = r.ue_max(1023, "pic_width_in_mbs_minus1")? + 1;
    sps.height_in_map_units = r.ue_max(1023, "pic_height_in_map_units_minus1")? + 1;
    sps.frame_mbs_only = r.bit()?;
    if !sps.frame_mbs_only {
        sps.mb_adaptive_frame_field = r.bit()?;
    }
    sps.direct_8x8_inference = r.bit()?;

    if r.bit()? {
        // CropUnitX/Y for 4:2:0 frames, the only layout decoded
        let unit_x = if sps.chroma_array_type() == 0 { 1 } else { 2 };
        let unit_y = (if sps.chroma_array_type() == 1 { 2 } else { 1 })
            * if sps.frame_mbs_only { 1 } else { 2 };
        sps.crop = [
            r.ue()? * unit_x,
            r.ue()? * unit_x,
            r.ue()? * unit_y,
            r.ue()? * unit_y,
        ];
    }

    if r.bit()? {
        read_vui(&mut r, &mut sps)?;
    }

    Ok(sps)
}

// rbsp of a NAL unit of type 8, without the NAL header byte. Needs its SPS for the
// chroma format and the scaling list fall-backs
pub fn parse_pps(rbsp: &[u8], sps_of: impl Fn(u32) -> Option<Sps>) -> Result<Pps, ParseError> {
    let mut r = BitReader::new(rbsp);
    let id = r.ue_max(255, "pic_parameter_set_id")?;
    let sps_id = r.ue_max(31, "seq_parameter_set_id")?;
    let sps = sps_of(sps_id).ok_or(ParseError::MissingParameterSet("SPS"))?;

    let entropy_coding_mode = r.bit()?;
    let bottom_field_pic_order_in_frame_present = r.bit()?;
    if r.ue()? != 0 {
        return Err(ParseError::Unsupported("slice groups (FMO)".to_string()));
    }
    let num_ref_idx_l0_default_active = r.ue_max(31, "num_ref_idx_l0_default_active_minus1")? + 1;
    let num_ref_idx_l1_default_active = r.ue_max(31, "num_ref_idx_l1_default_active_minus1")? + 1;
    let weighted_pred = r.bit()?;
    let weighted_bipred_idc = r.bits(2)?;
    let pic_init_qp_minus26 = r.se()?;
    let pic_init_qs_minus26 = r.se()?;
    let chroma_qp_index_offset = r.se()?;
    let deblocking_filter_control_present = r.bit()?;
    let constrained_intra_pred = r.bit()?;
    let redundant_pic_cnt_present = r.bit()?;

    let mut pps = Pps {
        id,
        sps_id,
        entropy_coding_mode,
        bottom_field_pic_order_in_frame_present,
        num_ref_idx_l0_default_active,
        num_ref_idx_l1_default_active,
        weighted_pred,
        weighted_bipred_idc,
        pic_init_qp_minus26,
        pic_init_qs_minus26,
        chroma_qp_index_offset,
        deblocking_filter_control_present,
        constrained_intra_pred,
        redundant_pic_cnt_present,
        transform_8x8_mode: false,
        second_chroma_qp_index_offset: chroma_qp_index_offset,
        scaling_4x4: sps.scaling_4x4,
        scaling_8x8: sps.scaling_8x8,
    };

    if r.more_rbsp_data() {
        pps.transform_8x8_mode = r.bit()?;
        if r.bit()? {
            let count = 6 + if pps.transform_8x8_mode {
                if sps.chroma_format_idc == 3 {
                    6
                } else {
                    2
                }
            } else {
                0
            };
            // 7.4.2.2: rule B (the sequence lists) only when the SPS has a matrix itself,
            // otherwise rule A (the defaults), as for the SPS
            let (sps_4x4, sps_8x8) = (sps.scaling_4x4, sps.scaling_8x8);
            let (fallback_4x4, fallback_8x8) = if sps.scaling_matrix_present {
                ((&sps_4x4[0], &sps_4x4[3]), (&sps_8x8[0], &sps_8x8[1]))
            } else {
                (
                    (&DEFAULT_4X4_INTRA, &DEFAULT_4X4_INTER),
                    (&DEFAULT_8X8_INTRA, &DEFAULT_8X8_INTER),
                )
            };
            read_scaling_matrix(
                &mut r,
                count,
                &mut pps.scaling_4x4,
                &mut pps.scaling_8x8,
                fallback_4x4,
                fallback_8x8,
            )?;
        }
        pps.second_chroma_qp_index_offset = r.se()?;
    }

    Ok(pps)
}

// Lists 0-5 are 4x4 (intra Y, Cb, Cr, inter Y, Cb, Cr), 6-7 are 8x8 luma (intra, inter);
// 8-11 (4:4:4 chroma 8x8) are parsed and dropped, that format is not decoded
fn read_scaling_matrix(
    r: &mut BitReader,
    count: usize,
    lists_4x4: &mut ScalingList4x4,
    lists_8x8: &mut ScalingList8x8,
    fallback_4x4: (&[u8; 16], &[u8; 16]),
    fallback_8x8: (&[u8; 64], &[u8; 64]),
) -> Result<(), ParseError> {
    for i in 0..count {
        let present = r.bit()?;
        if i < 6 {
            let default = if i < 3 {
                &DEFAULT_4X4_INTRA
            } else {
                &DEFAULT_4X4_INTER
            };
            lists_4x4[i] = if present {
                read_scaling_list(r)?.unwrap_or(*default)
            } else if i == 0 {
                *fallback_4x4.0
            } else if i == 3 {
                *fallback_4x4.1
            } else {
                lists_4x4[i - 1]
            };
        } else if i < 8 {
            let default = if i == 6 {
                &DEFAULT_8X8_INTRA
            } else {
                &DEFAULT_8X8_INTER
            };
            lists_8x8[i - 6] = if present {
                read_scaling_list(r)?.unwrap_or(*default)
            } else if i == 6 {
                *fallback_8x8.0
            } else {
                *fallback_8x8.1
            };
        } else if present {
            read_scaling_list::<64>(r)?;
        }
    }
    Ok(())
}

// 7.3.2.1.1.1; None when useDefaultScalingMatrixFlag is set
fn read_scaling_list<const N: usize>(r: &mut BitReader) -> Result<Option<[u8; N]>, ParseError> {
    let mut list = [0u8; N];
    let mut last_scale = 8i32;
    let mut next_scale = 8i32;
    for (j, value) in list.iter_mut().enumerate() {
        if next_scale != 0 {
            let delta = r.se()?;
            next_scale = (last_scale + delta + 256).rem_euclid(256);
            if j == 0 && next_scale == 0 {
                return Ok(None);
            }
        }
        *value = if next_scale == 0 {
            last_scale
        } else {
            next_scale
        } as u8;
        last_scale = *value as i32;
    }
    Ok(Some(list))
}

// E.1.1, read up to the bitstream restriction (reordering depth)
fn read_vui(r: &mut BitReader, sps: &mut Sps) -> Result<(), ParseError> {
    if r.bit()? {
        // aspect_ratio_idc, 255 = Extended_SAR
        if r.bits(8)? == 255 {
            r.skip(32)?;
        }
    }
    if r.bit()? {
        // overscan_appropriate_flag
        r.skip(1)?;
    }
    if r.bit()? {
        // video_format
        r.skip(3)?;
        sps.full_range = r.bit()?;
        if r.bit()? {
            // colour_primaries, transfer_characteristics
            r.skip(16)?;
            sps.matrix_coefficients = r.bits(8)? as u8;
        }
    }
    if r.bit()? {
        // chroma_sample_loc_type_top_field / bottom_field
        r.ue()?;
        r.ue()?;
    }
    if r.bit()? {
        // num_units_in_tick, time_scale, fixed_frame_rate_flag
        r.skip(65)?;
    }
    let nal_hrd = r.bit()?;
    if nal_hrd {
        skip_hrd(r)?;
    }
    let vcl_hrd = r.bit()?;
    if vcl_hrd {
        skip_hrd(r)?;
    }
    if nal_hrd || vcl_hrd {
        // low_delay_hrd_flag
        r.skip(1)?;
    }
    // pic_struct_present_flag
    r.skip(1)?;
    if r.bit()? {
        // motion_vectors_over_pic_boundaries_flag
        r.skip(1)?;
        // max_bytes_per_pic_denom, max_bits_per_mb_denom, log2_max_mv_length h/v
        for _ in 0..4 {
            r.ue()?;
        }
        sps.max_num_reorder_frames = Some(r.ue_max(16, "max_num_reorder_frames")?);
        sps.max_dec_frame_buffering = Some(r.ue_max(16, "max_dec_frame_buffering")?);
    }
    Ok(())
}

// E.1.2
fn skip_hrd(r: &mut BitReader) -> Result<(), ParseError> {
    let cpb_count = r.ue_max(31, "cpb_cnt_minus1")? + 1;
    // bit_rate_scale, cpb_size_scale
    r.skip(8)?;
    for _ in 0..cpb_count {
        r.ue()?;
        r.ue()?;
        r.skip(1)?;
    }
    // initial_cpb_removal_delay_length_minus1, cpb_removal_delay_length_minus1,
    // dpb_output_delay_length_minus1, time_offset_length
    r.skip(20)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::h264::testing::BitWriter;

    // 1920x1080 High profile: 68 MB rows coded, 8 rows cropped at the bottom
    fn high_1080p_sps(scaling: bool) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.bits(100, 8).bits(0, 8).bits(40, 8).ue(0);
        w.ue(1).ue(0).ue(0).bits(0, 1);
        w.bits(scaling as u32, 1);
        if scaling {
            // List 0 explicit (all 16 -> deltas 8 then 0), list 1 absent (copies 0),
            // list 3 uses the default (first delta brings next_scale to 0), rest absent
            w.bits(1, 1).se(8);
            for _ in 1..16 {
                w.se(0);
            }
            w.bits(0, 1).bits(0, 1);
            w.bits(1, 1).se(-8);
            for _ in 4..8 {
                w.bits(0, 1);
            }
        }
        w.ue(0).ue(0).ue(2).ue(4).bits(0, 1);
        w.ue(119).ue(67).bits(1, 1).bits(1, 1);
        w.bits(1, 1).ue(0).ue(0).ue(0).ue(4);
        w.bits(0, 1);
        w.finish()
    }

    #[test]
    fn parses_sps_size_cropping_and_poc() {
        let sps = parse_sps(&high_1080p_sps(false)).unwrap();
        assert_eq!(sps.profile_idc, 100);
        assert_eq!(sps.coded_size(), (1920, 1088));
        assert_eq!(sps.visible_rect(), (0, 0, 1920, 1080));
        assert_eq!(sps.log2_max_frame_num, 4);
        assert_eq!(sps.pic_order_cnt_type, 0);
        assert_eq!(sps.log2_max_pic_order_cnt_lsb, 6);
        assert_eq!(sps.max_num_ref_frames, 4);
        assert_eq!(sps.scaling_4x4, [FLAT_4X4; 6]);
    }

    #[test]
    fn applies_scaling_list_fallbacks() {
        let sps = parse_sps(&high_1080p_sps(true)).unwrap();
        assert_eq!(sps.scaling_4x4[0], [16; 16]);
        // Absent: copies the previous list
        assert_eq!(sps.scaling_4x4[1], [16; 16]);
        assert_eq!(sps.scaling_4x4[2], [16; 16]);
        assert_eq!(sps.scaling_4x4[3], DEFAULT_4X4_INTER);
        assert_eq!(sps.scaling_4x4[5], DEFAULT_4X4_INTER);
        // Absent 8x8 lists fall back to the defaults (rule A)
        assert_eq!(sps.scaling_8x8, [DEFAULT_8X8_INTRA, DEFAULT_8X8_INTER]);

        // PPS without its own matrix inherits them; with one, rule B uses the SPS lists
        let mut w = BitWriter::default();
        w.ue(0).ue(0).bits(1, 1).bits(0, 1).ue(0).ue(0).ue(0);
        w.bits(0, 1)
            .bits(0, 2)
            .se(0)
            .se(0)
            .se(-2)
            .bits(1, 1)
            .bits(0, 1)
            .bits(0, 1);
        w.bits(1, 1).bits(1, 1);
        for _ in 0..8 {
            w.bits(0, 1);
        }
        w.se(3);
        let pps = parse_pps(&w.finish(), |_| Some(sps.clone())).unwrap();
        assert!(pps.transform_8x8_mode);
        assert_eq!(pps.chroma_qp_index_offset, -2);
        assert_eq!(pps.second_chroma_qp_index_offset, 3);
        assert_eq!(pps.scaling_4x4[3], sps.scaling_4x4[3]);
        assert_eq!(pps.scaling_8x8, sps.scaling_8x8);
    }

    #[test]
    fn pps_matrix_over_an_sps_without_one_falls_back_to_the_defaults() {
        // What x264 writes with cqm=jvt: a PPS matrix flag with every list absent
        let sps = parse_sps(&high_1080p_sps(false)).unwrap();
        let mut w = BitWriter::default();
        w.ue(0).ue(0).bits(1, 1).bits(0, 1).ue(0).ue(0).ue(0);
        w.bits(0, 1).bits(0, 2).se(0).se(0).se(0).bits(1, 1).bits(0, 1).bits(0, 1);
        w.bits(0, 1).bits(1, 1);
        for _ in 0..6 {
            w.bits(0, 1);
        }
        w.se(0);
        let pps = parse_pps(&w.finish(), |_| Some(sps.clone())).unwrap();
        assert_eq!(pps.scaling_4x4[0], DEFAULT_4X4_INTRA);
        assert_eq!(pps.scaling_4x4[2], DEFAULT_4X4_INTRA);
        assert_eq!(pps.scaling_4x4[3], DEFAULT_4X4_INTER);
        assert_eq!(pps.scaling_4x4[5], DEFAULT_4X4_INTER);
    }

    #[test]
    fn pps_without_extension_reuses_the_chroma_offset() {
        let sps = parse_sps(&high_1080p_sps(false)).unwrap();
        let mut w = BitWriter::default();
        w.ue(3).ue(0).bits(0, 1).bits(0, 1).ue(0).ue(2).ue(0);
        w.bits(1, 1)
            .bits(0, 2)
            .se(-4)
            .se(0)
            .se(1)
            .bits(1, 1)
            .bits(0, 1)
            .bits(0, 1);
        let pps = parse_pps(&w.finish(), |_| Some(sps.clone())).unwrap();
        assert_eq!(pps.id, 3);
        assert_eq!(pps.num_ref_idx_l0_default_active, 3);
        assert!(pps.weighted_pred);
        assert_eq!(pps.pic_init_qp_minus26, -4);
        assert_eq!(pps.second_chroma_qp_index_offset, 1);
        assert!(!pps.transform_8x8_mode);
    }
}
