// Parameter sets of the streams we encode (sections 7.3.2.1, 7.3.2.2 and annex E). Hardware
// encoders take them as packed headers, so the stream carries what we chose: colour range
// and matrix of the camera, frame rate, and no reordering, which lets players show each
// picture as soon as it arrives
use super::bits::{escape, BitWriter};

const NAL_REF_IDC_HIGHEST: u8 = 3 << 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    // CAVLC, for decoders and encoders without CABAC
    ConstrainedBaseline,
    Main,
}

impl Profile {
    pub fn idc(self) -> u8 {
        match self {
            Profile::ConstrainedBaseline => 66,
            Profile::Main => 77,
        }
    }

    // constraint_set0 and constraint_set1: also decodable as Baseline and as Main
    fn constraint_flags(self) -> u8 {
        match self {
            Profile::ConstrainedBaseline => 0b1100_0000,
            Profile::Main => 0,
        }
    }

    pub fn cabac(self) -> bool {
        self == Profile::Main
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSettings {
    pub profile: Profile,
    pub level_idc: u8,
    // Visible size; the coded size is rounded up to whole macroblocks and cropped back
    pub width: u32,
    pub height: u32,
    pub log2_max_frame_num: u32,
    pub log2_max_pic_order_cnt_lsb: u32,
    pub max_num_ref_frames: u32,
    pub full_range: bool,
    // BT.709 (HD) or BT.601 (SD, JPEG)
    pub bt709: bool,
    pub frame_rate: (u32, u32),
}

impl StreamSettings {
    pub fn width_in_mbs(&self) -> u32 {
        self.width.div_ceil(16)
    }

    pub fn height_in_mbs(&self) -> u32 {
        self.height.div_ceil(16)
    }

    // Right and bottom crop in units of 2 luma samples (4:2:0 frames)
    pub fn crop(&self) -> Option<(u32, u32)> {
        let right = (self.width_in_mbs() * 16 - self.width) / 2;
        let bottom = (self.height_in_mbs() * 16 - self.height) / 2;
        (right != 0 || bottom != 0).then_some((right, bottom))
    }

    // (colour_primaries, transfer_characteristics, matrix_coefficients) of table E-3 to E-5
    fn colour_description(&self) -> (u32, u32, u32) {
        if self.bt709 {
            (1, 1, 1)
        } else {
            // SMPTE 170M, the BT.601 525 line system
            (6, 6, 6)
        }
    }

    // NAL unit of the SPS (header byte included, start code not)
    pub fn sps(&self) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.bits(self.profile.idc() as u32, 8)
            .bits(self.profile.constraint_flags() as u32, 8)
            .bits(self.level_idc as u32, 8)
            .ue(0)
            .ue(self.log2_max_frame_num - 4)
            // pic_order_cnt_type 0
            .ue(0)
            .ue(self.log2_max_pic_order_cnt_lsb - 4)
            .ue(self.max_num_ref_frames)
            .flag(false)
            .ue(self.width_in_mbs() - 1)
            .ue(self.height_in_mbs() - 1)
            // frame_mbs_only_flag, direct_8x8_inference_flag
            .flag(true)
            .flag(true);
        match self.crop() {
            Some((right, bottom)) => {
                w.flag(true).ue(0).ue(right).ue(0).ue(bottom);
            }
            None => {
                w.flag(false);
            }
        }

        // VUI
        w.flag(true);
        let (primaries, transfer, matrix) = self.colour_description();
        let (numerator, denominator) = self.frame_rate;
        // No aspect ratio, no overscan info
        w.flag(false).flag(false);
        // video_format 5 (unspecified), range, colour description
        w.flag(true)
            .bits(5, 3)
            .flag(self.full_range)
            .flag(true)
            .bits(primaries, 8)
            .bits(transfer, 8)
            .bits(matrix, 8);
        // No chroma location; timing: a frame lasts two ticks (one per field). Not fixed:
        // camera frames come when the camera delivers them
        w.flag(false)
            .flag(true)
            .bits(denominator, 32)
            .bits(numerator * 2, 32)
            .flag(false);
        // No HRD parameters, no picture structure
        w.flag(false).flag(false).flag(false);
        // Bitstream restriction: no reordering, so decoders output each picture right away
        w.flag(true)
            .flag(true)
            .ue(0)
            .ue(0)
            .ue(16)
            .ue(16)
            .ue(0)
            .ue(self.max_num_ref_frames);

        nal(NAL_SPS, &w.finish())
    }

    // NAL unit of the PPS (header byte included, start code not). Initial QP 26, one
    // reference by default, deblocking controlled per slice
    pub fn pps(&self) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.ue(0)
            .ue(0)
            .flag(self.profile.cabac())
            .flag(false)
            // One slice group, one active reference in each list
            .ue(0)
            .ue(0)
            .ue(0)
            // No weighted prediction
            .flag(false)
            .bits(0, 2)
            // pic_init_qp_minus26, pic_init_qs_minus26, chroma_qp_index_offset
            .se(0)
            .se(0)
            .se(0)
            .flag(true)
            .flag(false)
            .flag(false);
        nal(NAL_PPS, &w.finish())
    }
}

fn nal(nal_type: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut nal = vec![NAL_REF_IDC_HIGHEST | nal_type];
    nal.extend(escape(rbsp));
    nal
}

// Lowest level (table A-1) that holds the picture size, rate and bitrate
pub fn level_for(width: u32, height: u32, fps: f64, bitrate: u32) -> u8 {
    // level_idc, MaxMBPS, MaxFS, MaxBR (kbit/s, Main profile)
    const LEVELS: [(u8, u32, u32, u32); 11] = [
        (21, 19_800, 792, 4_000),
        (22, 20_250, 1_620, 4_000),
        (30, 40_500, 1_620, 10_000),
        (31, 108_000, 3_600, 14_000),
        (32, 216_000, 5_120, 20_000),
        (40, 245_760, 8_192, 20_000),
        (41, 245_760, 8_192, 50_000),
        (42, 522_240, 8_704, 50_000),
        (50, 589_824, 22_080, 135_000),
        (51, 983_040, 36_864, 240_000),
        (52, 2_073_600, 36_864, 240_000),
    ];
    let frame_size = width.div_ceil(16) * height.div_ceil(16);
    let mb_rate = (frame_size as f64 * fps).ceil() as u32;
    LEVELS
        .iter()
        .find(|(_, max_mbps, max_fs, max_br)| {
            frame_size <= *max_fs && mb_rate <= *max_mbps && bitrate.div_ceil(1000) <= *max_br
        })
        .map_or(52, |level| level.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::h264::{
        bits::unescape,
        params::{parse_pps, parse_sps},
    };

    fn settings(width: u32, height: u32) -> StreamSettings {
        StreamSettings {
            profile: Profile::Main,
            level_idc: 31,
            width,
            height,
            log2_max_frame_num: 8,
            log2_max_pic_order_cnt_lsb: 9,
            max_num_ref_frames: 1,
            full_range: true,
            bt709: false,
            frame_rate: (30000, 1001),
        }
    }

    #[test]
    fn writes_parameter_sets_our_parser_reads_back() {
        for (width, height) in [(1280, 720), (360, 202)] {
            let settings = settings(width, height);
            let sps_nal = settings.sps();
            assert_eq!(sps_nal[0], 0x67);
            let sps = parse_sps(&unescape(&sps_nal[1..])).unwrap();
            assert_eq!(sps.profile_idc, 77);
            assert_eq!(sps.level_idc, 31);
            assert_eq!(sps.visible_rect(), (0, 0, width, height));
            assert_eq!(sps.log2_max_frame_num, 8);
            assert_eq!(sps.pic_order_cnt_type, 0);
            assert_eq!(sps.log2_max_pic_order_cnt_lsb, 9);
            assert_eq!(sps.max_num_ref_frames, 1);
            assert!(sps.frame_mbs_only);
            assert!(sps.full_range);
            assert_eq!(sps.matrix_coefficients, 6);
            assert_eq!(sps.max_num_reorder_frames, Some(0));
            assert_eq!(sps.max_dec_frame_buffering, Some(1));

            let pps_nal = settings.pps();
            assert_eq!(pps_nal[0], 0x68);
            let pps = parse_pps(&unescape(&pps_nal[1..]), |_| Some(sps.clone())).unwrap();
            assert!(pps.entropy_coding_mode);
            assert_eq!(pps.num_ref_idx_l0_default_active, 1);
            assert_eq!(pps.pic_init_qp_minus26, 0);
            assert!(pps.deblocking_filter_control_present);
            assert!(!pps.transform_8x8_mode);
        }
    }

    #[test]
    fn constrained_baseline_uses_cavlc() {
        let settings = StreamSettings {
            profile: Profile::ConstrainedBaseline,
            bt709: true,
            ..settings(640, 480)
        };
        let sps = parse_sps(&unescape(&settings.sps()[1..])).unwrap();
        assert_eq!((sps.profile_idc, sps.constraint_flags), (66, 0b1100_0000));
        assert_eq!(sps.matrix_coefficients, 1);
        let pps = parse_pps(&unescape(&settings.pps()[1..]), |_| Some(sps.clone())).unwrap();
        assert!(!pps.entropy_coding_mode);
    }

    #[test]
    fn picks_the_lowest_level_that_fits() {
        assert_eq!(level_for(640, 480, 30.0, 2_000_000), 30);
        assert_eq!(level_for(1280, 720, 30.0, 4_000_000), 31);
        // Same picture, a bitrate above level 3.1
        assert_eq!(level_for(1280, 720, 30.0, 16_000_000), 32);
        assert_eq!(level_for(1920, 1080, 30.0, 8_000_000), 40);
        assert_eq!(level_for(1920, 1080, 60.0, 8_000_000), 42);
    }
}
