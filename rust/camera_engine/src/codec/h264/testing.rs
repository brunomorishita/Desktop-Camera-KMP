// Test helpers: builds bitstreams syntax element by syntax element
#[derive(Default)]
pub struct BitWriter {
    bits: Vec<bool>,
}

impl BitWriter {
    pub fn len(&self) -> usize {
        self.bits.len()
    }

    pub fn bits(&mut self, value: u32, count: u32) -> &mut Self {
        for i in (0..count).rev() {
            self.bits.push((value >> i) & 1 == 1);
        }
        self
    }

    pub fn ue(&mut self, value: u32) -> &mut Self {
        let code = value as u64 + 1;
        let length = 64 - code.leading_zeros();
        self.bits(0, length - 1);
        for i in (0..length).rev() {
            self.bits.push((code >> i) & 1 == 1);
        }
        self
    }

    pub fn se(&mut self, value: i32) -> &mut Self {
        self.ue(if value > 0 {
            (2 * value - 1) as u32
        } else {
            (-2 * value) as u32
        })
    }

    // rbsp_trailing_bits included
    pub fn finish(&mut self) -> Vec<u8> {
        self.bits.push(true);
        while !self.bits.len().is_multiple_of(8) {
            self.bits.push(false);
        }
        self.bits
            .chunks(8)
            .map(|chunk| chunk.iter().fold(0u8, |byte, &bit| (byte << 1) | bit as u8))
            .collect()
    }
}

use super::{
    params::Sps,
    slice::{MemoryOp, SliceHeader, SliceType},
};

// Progressive 4:2:0 stream with the given frame_num range, POC type and reference count
pub fn sps(log2_max_frame_num: u32, pic_order_cnt_type: u32, max_num_ref_frames: u32) -> Sps {
    Sps {
        id: 0,
        profile_idc: 100,
        constraint_flags: 0,
        level_idc: 40,
        chroma_format_idc: 1,
        separate_colour_plane: false,
        bit_depth_luma_minus8: 0,
        bit_depth_chroma_minus8: 0,
        scaling_matrix_present: false,
        scaling_4x4: [[16; 16]; 6],
        scaling_8x8: [[16; 64]; 2],
        log2_max_frame_num,
        pic_order_cnt_type,
        log2_max_pic_order_cnt_lsb: 6,
        delta_pic_order_always_zero: false,
        offset_for_non_ref_pic: 0,
        offset_for_top_to_bottom_field: 0,
        offset_for_ref_frame: Vec::new(),
        max_num_ref_frames,
        gaps_in_frame_num_allowed: true,
        width_in_mbs: 40,
        height_in_map_units: 30,
        frame_mbs_only: true,
        mb_adaptive_frame_field: false,
        direct_8x8_inference: true,
        crop: [0; 4],
        full_range: false,
        matrix_coefficients: 2,
        max_num_reorder_frames: None,
        max_dec_frame_buffering: None,
    }
}

pub fn slice(slice_type: SliceType, frame_num: u32, poc_lsb: u32, reference: bool) -> SliceHeader {
    SliceHeader {
        nal_ref_idc: if reference { 1 } else { 0 },
        idr: false,
        first_mb: 0,
        slice_type,
        pps_id: 0,
        frame_num,
        idr_pic_id: 0,
        pic_order_cnt_lsb: poc_lsb,
        delta_pic_order_cnt_bottom: 0,
        delta_pic_order_cnt: [0; 2],
        redundant_pic_cnt: 0,
        direct_spatial_mv_pred: false,
        num_ref_idx_active: match slice_type {
            SliceType::B => [4, 4],
            SliceType::P => [4, 0],
            _ => [0, 0],
        },
        list_modifications: [Vec::new(), Vec::new()],
        pred_weight_table: None,
        no_output_of_prior_pics: false,
        long_term_reference: false,
        memory_ops: None,
        cabac_init_idc: 0,
        slice_qp_delta: 0,
        disable_deblocking_filter_idc: 0,
        slice_alpha_c0_offset_div2: 0,
        slice_beta_offset_div2: 0,
        header_bits: 0,
    }
}

pub fn idr() -> SliceHeader {
    SliceHeader {
        idr: true,
        nal_ref_idc: 3,
        ..slice(SliceType::I, 0, 0, true)
    }
}

pub fn with_memory_ops(mut header: SliceHeader, ops: Vec<MemoryOp>) -> SliceHeader {
    header.memory_ops = Some(ops);
    header
}
