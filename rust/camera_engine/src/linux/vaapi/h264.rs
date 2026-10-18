// H.264 decoding on VA-API. The bitstream is parsed and the decoding process (POC,
// references, lists) runs in codec::h264; the driver only gets the filled-in parameter
// buffers and the slice data. Decoded pictures stay on the GPU as NV12 surfaces
use std::{collections::HashMap, ffi::c_void, mem, path::Path, sync::Arc};

use super::{
    api::Display,
    surface::{DecodedFrame, Surface, SurfacePool},
    sys::*,
};
use crate::{
    codec::h264::{
        bits::unescape,
        dpb::{CurrentPicture, Dpb, RefPicture},
        params::{parse_pps, parse_sps, Pps, Sps},
        slice::{parse_slice_header, SliceHeader, MAX_REFS},
    },
    rtsp::h264::{nal_type, split_annex_b, NAL_PPS, NAL_SPS},
};

const NAL_SLICE: u8 = 1;
const NAL_IDR: u8 = 5;
// Frames a consumer (preview, encoder) may hold on top of what the decoder keeps
const CONSUMER_SURFACES: u32 = 4;
// Reordering slack when the stream does not declare its depth (learned at run time)
const UNDECLARED_REORDER: u32 = 4;

// Config and context for one stream format; recreated when an IDR changes it
struct Session {
    display: Arc<Display>,
    config: VAConfigID,
    context: VAContextID,
    pool: Arc<SurfacePool>,
    profile: VAProfile,
    surface_count: u32,
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            (self.display.api.destroy_context)(self.display.raw, self.context);
            (self.display.api.destroy_config)(self.display.raw, self.config);
        }
    }
}

pub(crate) struct H264Decoder {
    display: Arc<Display>,
    sps: HashMap<u32, Sps>,
    pps: HashMap<u32, Pps>,
    dpb: Dpb<DecodedFrame>,
    session: Option<Session>,
    // Decoding (re)starts at an IDR: before the first one, or after an error
    waiting_for_idr: bool,
}

impl H264Decoder {
    pub fn new(render_node: &Path) -> Result<Self, String> {
        Ok(H264Decoder {
            display: Display::open(render_node)?,
            sps: HashMap::new(),
            pps: HashMap::new(),
            dpb: Dpb::default(),
            session: None,
            waiting_for_idr: true,
        })
    }

    // One access unit (Annex B, every slice of one picture, parameter sets allowed).
    // Returns the pictures ready to show, in display order: none, one, or several
    pub fn decode(
        &mut self,
        access_unit: &[u8],
        timestamp_hns: i64,
    ) -> Result<Vec<DecodedFrame>, String> {
        let result = self.decode_access_unit(access_unit, timestamp_hns);
        if result.is_err() {
            // The references may now be wrong: drop them and resync at the next IDR
            self.dpb = Dpb::default();
            self.waiting_for_idr = true;
        }
        result
    }

    // Pictures still held for reordering, at the end of the stream
    pub fn flush(&mut self) -> Vec<DecodedFrame> {
        self.dpb.flush()
    }

    // Forgets the references: decoding resumes at the next IDR. For a caller that stopped
    // feeding access units for a while
    pub fn reset(&mut self) {
        self.dpb = Dpb::default();
        self.waiting_for_idr = true;
    }

    fn decode_access_unit(
        &mut self,
        access_unit: &[u8],
        timestamp_hns: i64,
    ) -> Result<Vec<DecodedFrame>, String> {
        let mut slices: Vec<(SliceHeader, &[u8])> = Vec::new();
        let mut parameters: Option<(Sps, Pps)> = None;

        for nal in split_annex_b(access_unit) {
            match nal_type(nal) {
                NAL_SPS => {
                    let sps = parse_sps(&unescape(&nal[1..])).map_err(|e| e.to_string())?;
                    self.sps.insert(sps.id, sps);
                }
                NAL_PPS => {
                    let sps = &self.sps;
                    let pps = parse_pps(&unescape(&nal[1..]), |id| sps.get(&id).cloned())
                        .map_err(|e| e.to_string())?;
                    self.pps.insert(pps.id, pps);
                }
                NAL_SLICE | NAL_IDR => {
                    let (sps, pps) = (&self.sps, &self.pps);
                    let (header, slice_sps, slice_pps) = parse_slice_header(
                        &unescape(nal),
                        |id| sps.get(&id).cloned(),
                        |id| pps.get(&id).cloned(),
                    )
                    .map_err(|e| e.to_string())?;
                    // Redundant copies are only for error recovery
                    if header.redundant_pic_cnt == 0 {
                        parameters.get_or_insert((slice_sps, slice_pps));
                        slices.push((header, nal));
                    }
                }
                // SEI, delimiters, filler: not needed to decode
                _ => {}
            }
        }

        let Some((sps, pps)) = parameters else {
            return Ok(Vec::new());
        };
        let first = &slices[0].0;
        if self.waiting_for_idr {
            if !first.idr {
                return Ok(Vec::new());
            }
            self.waiting_for_idr = false;
        }

        let mut output = Vec::new();
        if first.idr {
            output.extend(self.prepare_session(&sps)?);
        }
        let session = self.session.as_ref().ok_or("no decoding session")?;
        let surface = session
            .pool
            .acquire()
            .ok_or("every surface is still in use (frames held for too long?)")?;

        let (current, earlier) = self
            .dpb
            .begin_picture(first, &sps)
            .map_err(|e| e.to_string())?;
        output.extend(earlier);

        let mut buffers = Vec::new();
        let decoded = self.submit(
            session,
            &surface,
            &sps,
            &pps,
            &current,
            &slices,
            &mut buffers,
        );
        let display = &self.display;
        for buffer in buffers {
            unsafe { (display.api.destroy_buffer)(display.raw, buffer) };
        }
        decoded?;

        let frame = DecodedFrame {
            surface: Arc::new(surface),
            timestamp_hns,
            visible: sps.visible_rect(),
            full_range: sps.full_range,
            bt709: match sps.matrix_coefficients {
                1 => true,
                5 | 6 => false,
                // Unspecified: the usual convention, HD content is BT.709
                _ => sps.visible_rect().3 >= 720,
            },
        };
        output.extend(self.dpb.finish_picture(current, frame, &sps));
        Ok(output)
    }

    // (Re)creates the config, context and surfaces when the stream format changes.
    // Returns the pictures of the previous sequence still waiting for output
    fn prepare_session(&mut self, sps: &Sps) -> Result<Vec<DecodedFrame>, String> {
        let profile = self.profile_for(sps)?;
        let (width, height) = sps.coded_size();
        let reorder = sps.max_num_reorder_frames.unwrap_or(UNDECLARED_REORDER);
        let surface_count = sps.max_num_ref_frames.max(1) + reorder + 1 + CONSUMER_SURFACES;

        if let Some(session) = &self.session {
            if session.profile == profile
                && (session.pool.width, session.pool.height) == (width, height)
                && session.surface_count >= surface_count
            {
                return Ok(Vec::new());
            }
        }

        // Frames of the old size stay valid: their pool lives until they are dropped
        let pending = self.dpb.flush();
        self.dpb = Dpb::default();
        self.session = None;
        self.session = Some(self.create_session(profile, width, height, surface_count)?);
        Ok(pending)
    }

    fn profile_for(&self, sps: &Sps) -> Result<VAProfile, String> {
        if sps.chroma_format_idc != 1
            || sps.bit_depth_luma_minus8 != 0
            || sps.bit_depth_chroma_minus8 != 0
        {
            return Err("only 8 bit 4:2:0 H.264 is decoded".to_string());
        }
        let wanted = match sps.profile_idc {
            66 => VAProfileH264ConstrainedBaseline,
            77 => VAProfileH264Main,
            100 => VAProfileH264High,
            other => return Err(format!("H.264 profile {other} is not supported")),
        };
        let available = self.display.profiles();
        // High decoders accept Main and Constrained Baseline streams too
        [wanted, VAProfileH264High]
            .into_iter()
            .find(|profile| available.contains(profile))
            .ok_or_else(|| format!("the GPU cannot decode H.264 profile {}", sps.profile_idc))
    }

    fn create_session(
        &self,
        profile: VAProfile,
        width: u32,
        height: u32,
        surface_count: u32,
    ) -> Result<Session, String> {
        let display = self.display.clone();
        let api = display.api;
        unsafe {
            let mut attrib = VAConfigAttrib {
                type_: VAConfigAttribRTFormat,
                value: VA_RT_FORMAT_YUV420,
            };
            let mut config = VA_INVALID_ID;
            api.check(
                "vaCreateConfig",
                (api.create_config)(
                    display.raw,
                    profile,
                    VAEntrypointVLD,
                    &mut attrib,
                    1,
                    &mut config,
                ),
            )?;

            let pool = match SurfacePool::new(
                display.clone(),
                VA_RT_FORMAT_YUV420,
                VA_FOURCC_NV12,
                width,
                height,
                surface_count,
            ) {
                Ok(pool) => pool,
                Err(e) => {
                    (api.destroy_config)(display.raw, config);
                    return Err(e);
                }
            };

            let mut context = VA_INVALID_ID;
            let mut targets = pool.ids.clone();
            let status = (api.create_context)(
                display.raw,
                config,
                width as i32,
                height as i32,
                VA_PROGRESSIVE as i32,
                targets.as_mut_ptr(),
                targets.len() as i32,
                &mut context,
            );
            if let Err(e) = api.check("vaCreateContext", status) {
                (api.destroy_config)(display.raw, config);
                return Err(e);
            }

            Ok(Session {
                display,
                config,
                context,
                pool,
                profile,
                surface_count,
            })
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn submit(
        &self,
        session: &Session,
        surface: &Surface,
        sps: &Sps,
        pps: &Pps,
        current: &CurrentPicture,
        slices: &[(SliceHeader, &[u8])],
        buffers: &mut Vec<VABufferID>,
    ) -> Result<(), String> {
        let display = &self.display;
        let api = display.api;
        let context = session.context;

        let mut create =
            |kind: VABufferType, size: usize, data: *const c_void| -> Result<VABufferID, String> {
                let mut buffer = VA_INVALID_ID;
                unsafe {
                    api.check(
                        "vaCreateBuffer",
                        (api.create_buffer)(
                            display.raw,
                            context,
                            kind,
                            size as u32,
                            1,
                            data as *mut c_void,
                            &mut buffer,
                        ),
                    )?;
                }
                buffers.push(buffer);
                Ok(buffer)
            };

        let picture = picture_parameters(sps, pps, current, surface.id, self.dpb.references());
        let iq_matrix = VAIQMatrixBufferH264 {
            ScalingList4x4: pps.scaling_4x4.map(|list| to_raster(&list, &ZIGZAG_4X4)),
            ScalingList8x8: pps.scaling_8x8.map(|list| to_raster(&list, &ZIGZAG_8X8)),
            va_reserved: [0; 4],
        };
        let mut picture_buffers = [
            create(
                VAPictureParameterBufferType,
                mem::size_of_val(&picture),
                &picture as *const _ as _,
            )?,
            create(
                VAIQMatrixBufferType,
                mem::size_of_val(&iq_matrix),
                &iq_matrix as *const _ as _,
            )?,
        ];

        let mut slice_buffers = Vec::with_capacity(slices.len() * 2);
        for (header, nal) in slices {
            let lists = self
                .dpb
                .reference_lists(header, current, sps)
                .map_err(|e| e.to_string())?;
            let parameters = slice_parameters(header, nal.len(), &lists);
            slice_buffers.push(create(
                VASliceParameterBufferType,
                mem::size_of_val(&parameters),
                &parameters as *const _ as _,
            )?);
            slice_buffers.push(create(VASliceDataBufferType, nal.len(), nal.as_ptr() as _)?);
        }

        unsafe {
            api.check(
                "vaBeginPicture",
                (api.begin_picture)(display.raw, context, surface.id),
            )?;
            let rendered = api
                .check(
                    "vaRenderPicture",
                    (api.render_picture)(display.raw, context, picture_buffers.as_mut_ptr(), 2),
                )
                .and_then(|_| {
                    // Slice parameters and data must alternate, one pair per call
                    slice_buffers.chunks_mut(2).try_for_each(|pair| {
                        api.check(
                            "vaRenderPicture",
                            (api.render_picture)(display.raw, context, pair.as_mut_ptr(), 2),
                        )
                    })
                });
            // Always ends the picture, even after a failed render, so the context stays usable
            let ended = api.check("vaEndPicture", (api.end_picture)(display.raw, context));
            rendered.and(ended)
        }
    }
}

// Raster position of each coefficient in coding (zig-zag) order, frame scan (8.5.6)
const ZIGZAG_4X4: [usize; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];
const ZIGZAG_8X8: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

// The bitstream codes scaling lists in zig-zag order, VA-API takes them in raster order
fn to_raster<const N: usize>(list: &[u8; N], zigzag: &[usize; N]) -> [u8; N] {
    let mut raster = [0; N];
    for (coded, &position) in zigzag.iter().enumerate() {
        raster[position] = list[coded];
    }
    raster
}

fn va_picture(picture: &RefPicture<DecodedFrame>) -> VAPictureH264 {
    VAPictureH264 {
        picture_id: picture
            .handle
            .as_ref()
            .map_or(VA_INVALID_SURFACE, |frame| frame.surface.id),
        frame_idx: picture.long_term.unwrap_or(picture.frame_num),
        flags: if picture.is_long_term() {
            VA_PICTURE_H264_LONG_TERM_REFERENCE
        } else {
            VA_PICTURE_H264_SHORT_TERM_REFERENCE
        },
        TopFieldOrderCnt: picture.top_poc,
        BottomFieldOrderCnt: picture.bottom_poc,
        va_reserved: [0; 4],
    }
}

fn invalid_picture() -> VAPictureH264 {
    VAPictureH264 {
        picture_id: VA_INVALID_SURFACE,
        flags: VA_PICTURE_H264_INVALID,
        ..Default::default()
    }
}

fn picture_parameters<'a>(
    sps: &Sps,
    pps: &Pps,
    current: &CurrentPicture,
    surface: VASurfaceID,
    references: impl Iterator<Item = &'a RefPicture<DecodedFrame>>,
) -> VAPictureParameterBufferH264 {
    let mut parameters = VAPictureParameterBufferH264 {
        CurrPic: VAPictureH264 {
            picture_id: surface,
            frame_idx: current.frame_num,
            flags: 0,
            TopFieldOrderCnt: current.top_poc,
            BottomFieldOrderCnt: current.bottom_poc,
            va_reserved: [0; 4],
        },
        ReferenceFrames: [invalid_picture(); 16],
        picture_width_in_mbs_minus1: (sps.width_in_mbs - 1) as u16,
        picture_height_in_mbs_minus1: (sps.height_in_mbs() - 1) as u16,
        bit_depth_luma_minus8: sps.bit_depth_luma_minus8 as u8,
        bit_depth_chroma_minus8: sps.bit_depth_chroma_minus8 as u8,
        num_ref_frames: sps.max_num_ref_frames as u8,
        pic_init_qp_minus26: pps.pic_init_qp_minus26 as i8,
        pic_init_qs_minus26: pps.pic_init_qs_minus26 as i8,
        chroma_qp_index_offset: pps.chroma_qp_index_offset as i8,
        second_chroma_qp_index_offset: pps.second_chroma_qp_index_offset as i8,
        frame_num: current.frame_num as u16,
        ..Default::default()
    };
    for (slot, reference) in parameters.ReferenceFrames.iter_mut().zip(references) {
        *slot = va_picture(reference);
    }

    let seq = unsafe { &mut parameters.seq_fields.bits };
    seq.set_chroma_format_idc(sps.chroma_format_idc);
    seq.set_residual_colour_transform_flag(sps.separate_colour_plane as u32);
    seq.set_gaps_in_frame_num_value_allowed_flag(sps.gaps_in_frame_num_allowed as u32);
    seq.set_frame_mbs_only_flag(sps.frame_mbs_only as u32);
    seq.set_mb_adaptive_frame_field_flag(sps.mb_adaptive_frame_field as u32);
    seq.set_direct_8x8_inference_flag(sps.direct_8x8_inference as u32);
    seq.set_MinLumaBiPredSize8x8((sps.level_idc >= 31) as u32);
    seq.set_log2_max_frame_num_minus4(sps.log2_max_frame_num - 4);
    seq.set_pic_order_cnt_type(sps.pic_order_cnt_type);
    seq.set_log2_max_pic_order_cnt_lsb_minus4(sps.log2_max_pic_order_cnt_lsb.saturating_sub(4));
    seq.set_delta_pic_order_always_zero_flag(sps.delta_pic_order_always_zero as u32);

    let pic = unsafe { &mut parameters.pic_fields.bits };
    pic.set_entropy_coding_mode_flag(pps.entropy_coding_mode as u32);
    pic.set_weighted_pred_flag(pps.weighted_pred as u32);
    pic.set_weighted_bipred_idc(pps.weighted_bipred_idc);
    pic.set_transform_8x8_mode_flag(pps.transform_8x8_mode as u32);
    pic.set_field_pic_flag(0);
    pic.set_constrained_intra_pred_flag(pps.constrained_intra_pred as u32);
    pic.set_pic_order_present_flag(pps.bottom_field_pic_order_in_frame_present as u32);
    pic.set_deblocking_filter_control_present_flag(pps.deblocking_filter_control_present as u32);
    pic.set_redundant_pic_cnt_present_flag(pps.redundant_pic_cnt_present as u32);
    pic.set_reference_pic_flag(current.is_reference as u32);

    parameters
}

fn slice_parameters(
    header: &SliceHeader,
    data_size: usize,
    lists: &[Vec<Option<RefPicture<DecodedFrame>>>; 2],
) -> VASliceParameterBufferH264 {
    let mut parameters = VASliceParameterBufferH264 {
        slice_data_size: data_size as u32,
        slice_data_offset: 0,
        slice_data_flag: VA_SLICE_DATA_FLAG_ALL,
        slice_data_bit_offset: header.header_bits as u16,
        first_mb_in_slice: header.first_mb as u16,
        slice_type: header.slice_type.code(),
        direct_spatial_mv_pred_flag: header.direct_spatial_mv_pred as u8,
        num_ref_idx_l0_active_minus1: header.num_ref_idx_active[0].saturating_sub(1) as u8,
        num_ref_idx_l1_active_minus1: header.num_ref_idx_active[1].saturating_sub(1) as u8,
        cabac_init_idc: header.cabac_init_idc as u8,
        slice_qp_delta: header.slice_qp_delta as i8,
        disable_deblocking_filter_idc: header.disable_deblocking_filter_idc as u8,
        slice_alpha_c0_offset_div2: header.slice_alpha_c0_offset_div2 as i8,
        slice_beta_offset_div2: header.slice_beta_offset_div2 as i8,
        RefPicList0: [invalid_picture(); MAX_REFS],
        RefPicList1: [invalid_picture(); MAX_REFS],
        ..Default::default()
    };

    let targets = [&mut parameters.RefPicList0, &mut parameters.RefPicList1];
    for (target, list) in targets.into_iter().zip(lists) {
        for (slot, entry) in target.iter_mut().zip(list) {
            if let Some(picture) = entry {
                *slot = va_picture(picture);
            }
        }
    }

    if let Some(table) = &header.pred_weight_table {
        parameters.luma_log2_weight_denom = table.luma_log2_denom as u8;
        parameters.chroma_log2_weight_denom = table.chroma_log2_denom as u8;
        parameters.luma_weight_l0_flag = table.luma_flags[0] as u8;
        parameters.chroma_weight_l0_flag = table.chroma_flags[0] as u8;
        parameters.luma_weight_l1_flag = table.luma_flags[1] as u8;
        parameters.chroma_weight_l1_flag = table.chroma_flags[1] as u8;
        for (index, entry) in table.weights[0].iter().enumerate() {
            parameters.luma_weight_l0[index] = entry.luma_weight;
            parameters.luma_offset_l0[index] = entry.luma_offset;
            parameters.chroma_weight_l0[index] = entry.chroma_weight;
            parameters.chroma_offset_l0[index] = entry.chroma_offset;
        }
        for (index, entry) in table.weights[1].iter().enumerate() {
            parameters.luma_weight_l1[index] = entry.luma_weight;
            parameters.luma_offset_l1[index] = entry.luma_offset;
            parameters.chroma_weight_l1[index] = entry.chroma_weight;
            parameters.chroma_offset_l1[index] = entry.chroma_offset;
        }
    }

    parameters
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::{linux::gpu, rtsp::h264::NAL_AUD};

    // An access unit per buffer, split at the delimiters, like a camera hands them out
    fn access_units(stream: &[u8]) -> Vec<Vec<u8>> {
        let mut units: Vec<Vec<u8>> = Vec::new();
        for nal in split_annex_b(stream) {
            if nal_type(nal) == NAL_AUD || units.is_empty() {
                units.push(Vec::new());
            }
            let unit = units.last_mut().unwrap();
            unit.extend_from_slice(&[0, 0, 0, 1]);
            unit.extend_from_slice(nal);
        }
        units
    }

    fn ffmpeg(args: &str) {
        let status = Command::new("ffmpeg")
            .args("-hide_banner -loglevel error -y".split_whitespace())
            .args(args.split_whitespace())
            .status()
            .expect("ffmpeg not found");
        assert!(status.success(), "ffmpeg {args}");
    }

    // Decodes `stream` and compares every frame with ffmpeg's software decoder
    fn check_against_ffmpeg(render_node: &Path, name: &str, size: &str, encoder_args: &str) {
        let dir = std::env::temp_dir().join(format!("vaapi-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stream = dir.join("stream.h264");
        let reference = dir.join("reference.nv12");
        ffmpeg(&format!(
            "-f lavfi -i testsrc2=size={size}:rate=30 -t 2 -pix_fmt yuv420p -c:v libx264 \
             {encoder_args} -f h264 {}",
            stream.display()
        ));
        ffmpeg(&format!(
            "-i {} -f rawvideo -pix_fmt nv12 {}",
            stream.display(),
            reference.display()
        ));
        let units = access_units(&std::fs::read(&stream).unwrap());
        let reference = std::fs::read(&reference).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        // Each frame is read and released right away, as a real consumer does: holding them
        // all would exhaust the surface pool
        let mut decoder = H264Decoder::new(render_node).unwrap();
        let mut frames: Vec<(i64, Vec<u8>)> = Vec::new();
        let mut visible = (0, 0, 0, 0);
        let mut keep = |output: Vec<DecodedFrame>| {
            for frame in output {
                visible = frame.visible;
                frames.push((frame.timestamp_hns, frame.read_nv12().unwrap()));
            }
        };
        for (index, unit) in units.iter().enumerate() {
            keep(decoder.decode(unit, index as i64).unwrap());
        }
        keep(decoder.flush());

        let (_, _, width, height) = visible;
        let frame_size = (width * height * 3 / 2) as usize;
        let expected: Vec<&[u8]> = reference.chunks(frame_size).collect();
        let mismatches: Vec<usize> = frames
            .iter()
            .zip(&expected)
            .enumerate()
            .filter(|(_, ((_, frame), expected))| frame.as_slice() != **expected)
            .map(|(index, _)| index)
            .collect();
        println!(
            "{name}: {width}x{height}, {} decoded / {} expected, {} mismatching",
            frames.len(),
            expected.len(),
            mismatches.len()
        );
        assert_eq!(frames.len(), expected.len(), "{name}: frame count");
        assert!(
            mismatches.is_empty(),
            "{name}: frames {mismatches:?} differ from ffmpeg"
        );
        // Every access unit shown exactly once
        let mut timestamps: Vec<i64> = frames.iter().map(|(timestamp, _)| *timestamp).collect();
        timestamps.sort();
        timestamps.dedup();
        assert_eq!(
            timestamps.len(),
            frames.len(),
            "{name}: a frame was shown twice"
        );
    }

    fn render_node() -> std::path::PathBuf {
        gpu::probe()
            .into_iter()
            .find(|gpu| gpu.vaapi.as_ref().is_ok_and(|caps| caps.h264_decode))
            .expect("no GPU decodes H.264 with VA-API")
            .render_node
    }

    #[test]
    #[ignore = "needs ffmpeg and a VA-API GPU, run with: cargo test -- --ignored --nocapture"]
    fn decodes_like_ffmpeg() {
        let node = render_node();
        // Rows differ and columns do not: a transposed scaling list changes the picture
        let cqm_file = std::env::temp_dir().join(format!("vaapi-cqm-{}.txt", std::process::id()));
        let rows_4x4 = |base: u32| -> String {
            (0..4)
                .map(|row| format!("{0},{0},{0},{0},", base + 6 * row))
                .collect()
        };
        let rows_8x8 = |base: u32| -> String {
            (0..8)
                .map(|row| format!("{},", base + 3 * row).repeat(8))
                .collect()
        };
        let mut cqm = String::new();
        for name in ["INTRA4X4_LUMA", "INTRA4X4_CHROMAU", "INTRA4X4_CHROMAV"] {
            cqm += &format!("{name} =\n{}\n", rows_4x4(10));
        }
        for name in ["INTER4X4_LUMA", "INTER4X4_CHROMAU", "INTER4X4_CHROMAV"] {
            cqm += &format!("{name} =\n{}\n", rows_4x4(14));
        }
        cqm += &format!(
            "INTRA8X8_LUMA =\n{}\nINTER8X8_LUMA =\n{}\n",
            rows_8x8(12),
            rows_8x8(16)
        );
        std::fs::write(&cqm_file, cqm).unwrap();

        let cases = [
            // CAVLC, POC type 2
            (
                "baseline",
                "320x240",
                "-profile:v baseline -x264-params aud=1".to_string(),
            ),
            (
                "main-cabac-3-refs",
                "320x240",
                "-profile:v main -bf 0 -refs 3 -x264-params aud=1".to_string(),
            ),
            // POC type 0, B references and the MMCOs that unmark them
            (
                "high-b-pyramid",
                "320x240",
                "-profile:v high -bf 3 -b_strategy 0 -refs 4 -x264-params aud=1:b-pyramid=normal"
                    .to_string(),
            ),
            // Default matrices signalled in the PPS only, explicit weights in P and B slices
            (
                "high-weighted-8x8-cqm",
                "320x240",
                "-profile:v high -bf 2 -x264-params aud=1:cqm=jvt:weightp=2:weightb=1:8x8dct=1"
                    .to_string(),
            ),
            (
                "asymmetric-cqm",
                "320x240",
                format!(
                    "-profile:v high -bf 0 -x264-params aud=1:8x8dct=1:cqmfile={}",
                    cqm_file.display()
                ),
            ),
            (
                "four-slices",
                "320x240",
                "-profile:v main -x264-params aud=1:slices=4".to_string(),
            ),
            (
                "cropped-size",
                "360x202",
                "-profile:v high -x264-params aud=1".to_string(),
            ),
        ];

        let mut failures = Vec::new();
        for (name, size, encoder_args) in &cases {
            let result =
                std::panic::catch_unwind(|| check_against_ffmpeg(&node, name, size, encoder_args));
            if result.is_err() {
                failures.push(*name);
            }
        }
        std::fs::remove_file(&cqm_file).ok();
        assert!(failures.is_empty(), "failed: {failures:?}");
    }

    #[test]
    #[ignore = "needs ffmpeg and a VA-API GPU, run with: cargo test -- --ignored --nocapture"]
    fn exports_frames_as_dmabuf() {
        let dir = std::env::temp_dir().join(format!("vaapi-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stream = dir.join("stream.h264");
        ffmpeg(&format!(
            "-f lavfi -i testsrc2=size=640x480:rate=30 -t 0.2 -pix_fmt yuv420p -c:v libx264 \
             -x264-params aud=1 -f h264 {}",
            stream.display()
        ));
        let units = access_units(&std::fs::read(&stream).unwrap());
        std::fs::remove_dir_all(&dir).ok();

        let mut decoder = H264Decoder::new(&render_node()).unwrap();
        let frame = units
            .iter()
            .find_map(|unit| decoder.decode(unit, 0).unwrap().into_iter().next())
            .expect("no frame decoded");
        let exported = frame.export().unwrap();
        println!(
            "DRM format {:?}, {} object(s), modifier {:#x}, planes {:?}",
            std::str::from_utf8(&exported.drm_format.to_le_bytes()).unwrap(),
            exported.objects.len(),
            exported.objects[0].modifier,
            exported.planes
        );
        assert_eq!(&exported.drm_format.to_le_bytes(), b"NV12");
        assert_eq!(exported.planes.len(), 2);
    }
}
