// H.264 encoding on VA-API, tuned for live streaming like the Windows encoder: constant
// bitrate, an IDR every second (or on request), P frames referencing only the previous one,
// so every picture goes out as soon as it is encoded. Input frames are the decoders' NV12
// surfaces on the same VA display, read in place
use std::{ffi::c_void, mem, path::Path, ptr, slice, sync::Arc};

use super::{
    api::Display,
    surface::{DecodedFrame, Surface, SurfacePool},
    sys::*,
};
use crate::{
    codec::h264::headers::{level_for, Profile, StreamSettings},
    media::EncodedFrame,
};

const LOG2_MAX_FRAME_NUM: u32 = 8;
const LOG2_MAX_POC_LSB: u32 = 9;
const SLICE_TYPE_P: u8 = 0;
const SLICE_TYPE_I: u8 = 2;
const START_CODE: [u8; 4] = [0, 0, 0, 1];

#[derive(Debug, Clone, Copy)]
pub(crate) struct EncoderSettings {
    pub width: u32,
    pub height: u32,
    pub frame_rate: (u32, u32),
    pub bitrate: u32,
    // Fixed function encoder (VDENC on Intel) instead of the shader based one
    pub low_power: bool,
}

// The picture the next P frame predicts from
struct Reference {
    surface: Surface,
    frame_num: u32,
    poc: i32,
}

pub(crate) struct H264Encoder {
    display: Arc<Display>,
    config: VAConfigID,
    context: VAContextID,
    coded_buffer: VABufferID,
    // Reconstructed pictures: the one being encoded and the reference
    reconstructed: Arc<SurfacePool>,
    settings: EncoderSettings,
    profile: Profile,
    level_idc: u8,
    // SPS and PPS written by us; without driver support for that, the driver writes its own
    packed_headers: bool,
    gop: u32,
    frames_since_idr: u32,
    idr_pic_id: u16,
    reference: Option<Reference>,
}

impl H264Encoder {
    pub fn new(render_node: &Path, settings: EncoderSettings) -> Result<Self, String> {
        let display = Display::shared(render_node)?;
        let api = display.api;
        let entrypoint = if settings.low_power {
            VAEntrypointEncSliceLP
        } else {
            VAEntrypointEncSlice
        };
        // Main as on Windows; Constrained Baseline for encoders without CABAC
        let (profile, va_profile) = [
            (Profile::Main, VAProfileH264Main),
            (
                Profile::ConstrainedBaseline,
                VAProfileH264ConstrainedBaseline,
            ),
        ]
        .into_iter()
        .find(|(_, va_profile)| display.entrypoints(*va_profile).contains(&entrypoint))
        .ok_or("the GPU has no H.264 encoder")?;

        let supports_cbr = display
            .config_attribute(va_profile, entrypoint, VAConfigAttribRateControl)
            .is_some_and(|modes| modes & VA_RC_CBR != 0);
        if !supports_cbr {
            return Err("the H.264 encoder has no constant bitrate mode".to_string());
        }
        let wanted_headers = VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_PICTURE;
        let packed_headers = display
            .config_attribute(va_profile, entrypoint, VAConfigAttribEncPackedHeaders)
            .is_some_and(|headers| headers & wanted_headers == wanted_headers);

        let mut attributes = vec![
            VAConfigAttrib {
                type_: VAConfigAttribRTFormat,
                value: VA_RT_FORMAT_YUV420,
            },
            VAConfigAttrib {
                type_: VAConfigAttribRateControl,
                value: VA_RC_CBR,
            },
        ];
        if packed_headers {
            attributes.push(VAConfigAttrib {
                type_: VAConfigAttribEncPackedHeaders,
                value: wanted_headers,
            });
        }

        let (width, height) = (
            settings.width.div_ceil(16) * 16,
            settings.height.div_ceil(16) * 16,
        );
        let fps = settings.frame_rate.0 as f64 / settings.frame_rate.1.max(1) as f64;
        let mut encoder = H264Encoder {
            display: display.clone(),
            config: VA_INVALID_ID,
            context: VA_INVALID_ID,
            coded_buffer: VA_INVALID_ID,
            reconstructed: SurfacePool::new(
                display.clone(),
                VA_RT_FORMAT_YUV420,
                VA_FOURCC_NV12,
                width,
                height,
                2,
            )?,
            settings,
            profile,
            level_idc: level_for(settings.width, settings.height, fps, settings.bitrate),
            packed_headers,
            // One keyframe per second, so clients recover quickly from packet loss
            gop: (fps.round() as u32).max(1),
            frames_since_idr: 0,
            idr_pic_id: 0,
            reference: None,
        };

        // From here on Drop releases whatever was created
        unsafe {
            api.check(
                "vaCreateConfig",
                (api.create_config)(
                    display.raw,
                    va_profile,
                    entrypoint,
                    attributes.as_mut_ptr(),
                    attributes.len() as i32,
                    &mut encoder.config,
                ),
            )?;
            let mut targets = encoder.reconstructed.ids.clone();
            api.check(
                "vaCreateContext",
                (api.create_context)(
                    display.raw,
                    encoder.config,
                    width as i32,
                    height as i32,
                    VA_PROGRESSIVE as i32,
                    targets.as_mut_ptr(),
                    targets.len() as i32,
                    &mut encoder.context,
                ),
            )?;
            // Room for an uncompressed picture: no sane bitrate makes a frame larger
            api.check(
                "vaCreateBuffer",
                (api.create_buffer)(
                    display.raw,
                    encoder.context,
                    VAEncCodedBufferType,
                    width * height * 3 / 2 + 64 * 1024,
                    1,
                    ptr::null_mut(),
                    &mut encoder.coded_buffer,
                ),
            )?;
        }
        Ok(encoder)
    }

    // Encodes one frame, waiting for the GPU. An IDR when asked, or once per GOP
    pub fn encode(
        &mut self,
        frame: &DecodedFrame,
        force_idr: bool,
    ) -> Result<EncodedFrame, String> {
        if !Arc::ptr_eq(frame.display(), &self.display) {
            return Err("the frame comes from another GPU than the encoder's".to_string());
        }
        let (_, _, width, height) = frame.visible;
        if (width, height) != (self.settings.width, self.settings.height) {
            return Err(format!(
                "frame of {width}x{height} for an encoder of {}x{}",
                self.settings.width, self.settings.height
            ));
        }

        let idr = force_idr || self.frames_since_idr >= self.gop || self.reference.is_none();
        if idr {
            self.frames_since_idr = 0;
            self.idr_pic_id = self.idr_pic_id.wrapping_add(1);
            self.reference = None;
        }
        let frame_num = self.frames_since_idr % (1 << LOG2_MAX_FRAME_NUM);
        // Two per frame, as for a field pair; it restarts at every IDR
        let poc = 2 * self.frames_since_idr as i32;
        let reconstructed = self
            .reconstructed
            .acquire()
            .ok_or("no free reconstructed surface")?;

        let stream = StreamSettings {
            profile: self.profile,
            level_idc: self.level_idc,
            width: self.settings.width,
            height: self.settings.height,
            log2_max_frame_num: LOG2_MAX_FRAME_NUM,
            log2_max_pic_order_cnt_lsb: LOG2_MAX_POC_LSB,
            max_num_ref_frames: 1,
            full_range: frame.full_range,
            bt709: frame.bt709,
            frame_rate: self.settings.frame_rate,
        };
        let mut buffers = Vec::new();
        let submitted = self.submit(
            frame.surface_id(),
            &reconstructed,
            &stream,
            idr,
            frame_num,
            poc,
            &mut buffers,
        );
        for buffer in buffers {
            unsafe { (self.display.api.destroy_buffer)(self.display.raw, buffer) };
        }
        submitted?;
        let data = self.read_coded(frame.surface_id())?;

        self.reference = Some(Reference {
            surface: reconstructed,
            frame_num,
            poc,
        });
        self.frames_since_idr += 1;
        Ok(EncodedFrame {
            data,
            timestamp_hns: frame.timestamp_hns,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn submit(
        &self,
        input: VASurfaceID,
        reconstructed: &Surface,
        stream: &StreamSettings,
        idr: bool,
        frame_num: u32,
        poc: i32,
        buffers: &mut Vec<VABufferID>,
    ) -> Result<(), String> {
        let display = &self.display;
        let api = display.api;
        let context = self.context;
        let mut create = |kind: VABufferType, data: &[u8]| -> Result<VABufferID, String> {
            let mut buffer = VA_INVALID_ID;
            unsafe {
                api.check(
                    "vaCreateBuffer",
                    (api.create_buffer)(
                        display.raw,
                        context,
                        kind,
                        data.len() as u32,
                        1,
                        data.as_ptr() as *mut c_void,
                        &mut buffer,
                    ),
                )?;
            }
            buffers.push(buffer);
            Ok(buffer)
        };

        let mut render = Vec::new();
        if idr {
            render.push(create(
                VAEncSequenceParameterBufferType,
                bytes_of(&self.sequence_parameters(stream)),
            )?);
            for misc in self.rate_control() {
                render.push(create(VAEncMiscParameterBufferType, &misc)?);
            }
            if self.packed_headers {
                for (kind, nal) in [
                    (VAEncPackedHeaderSequence, stream.sps()),
                    (VAEncPackedHeaderPicture, stream.pps()),
                ] {
                    let data = [START_CODE.as_slice(), &nal].concat();
                    let header = VAEncPackedHeaderParameterBuffer {
                        type_: kind,
                        bit_length: data.len() as u32 * 8,
                        has_emulation_bytes: 1,
                        ..Default::default()
                    };
                    render.push(create(
                        VAEncPackedHeaderParameterBufferType,
                        bytes_of(&header),
                    )?);
                    render.push(create(VAEncPackedHeaderDataBufferType, &data)?);
                }
            }
        }
        let picture = self.picture_parameters(reconstructed, idr, frame_num, poc);
        render.push(create(VAEncPictureParameterBufferType, bytes_of(&picture))?);
        let slice = self.slice_parameters(stream, idr, poc);
        render.push(create(VAEncSliceParameterBufferType, bytes_of(&slice))?);

        unsafe {
            api.check(
                "vaBeginPicture",
                (api.begin_picture)(display.raw, context, input),
            )?;
            let rendered = api.check(
                "vaRenderPicture",
                (api.render_picture)(
                    display.raw,
                    context,
                    render.as_mut_ptr(),
                    render.len() as i32,
                ),
            );
            // Always ends the picture, even after a failed render, so the context stays usable
            let ended = api.check("vaEndPicture", (api.end_picture)(display.raw, context));
            rendered.and(ended)
        }
    }

    // Waits for the picture and copies the bitstream out of the coded buffer
    fn read_coded(&self, input: VASurfaceID) -> Result<Vec<u8>, String> {
        let display = &self.display;
        let api = display.api;
        unsafe {
            api.check("vaSyncSurface", (api.sync_surface)(display.raw, input))?;
            let mut segment: *mut c_void = ptr::null_mut();
            api.check(
                "vaMapBuffer",
                (api.map_buffer)(display.raw, self.coded_buffer, &mut segment),
            )?;
            let mut data = Vec::new();
            let mut overflow = false;
            while !segment.is_null() {
                let current = &*(segment as *const VACodedBufferSegment);
                overflow |= current.status & VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK != 0;
                data.extend_from_slice(slice::from_raw_parts(
                    current.buf as *const u8,
                    current.size as usize,
                ));
                segment = current.next;
            }
            (api.unmap_buffer)(display.raw, self.coded_buffer);
            if overflow {
                return Err("the encoded picture did not fit its buffer".to_string());
            }
            Ok(data)
        }
    }

    fn sequence_parameters(&self, stream: &StreamSettings) -> VAEncSequenceParameterBufferH264 {
        let (numerator, denominator) = self.settings.frame_rate;
        let mut parameters = VAEncSequenceParameterBufferH264 {
            level_idc: self.level_idc,
            intra_period: self.gop,
            intra_idr_period: self.gop,
            ip_period: 1,
            bits_per_second: self.settings.bitrate,
            max_num_ref_frames: stream.max_num_ref_frames,
            picture_width_in_mbs: stream.width_in_mbs() as u16,
            picture_height_in_mbs: stream.height_in_mbs() as u16,
            vui_parameters_present_flag: 1,
            num_units_in_tick: denominator,
            time_scale: numerator * 2,
            ..Default::default()
        };
        if let Some((right, bottom)) = stream.crop() {
            parameters.frame_cropping_flag = 1;
            parameters.frame_crop_right_offset = right;
            parameters.frame_crop_bottom_offset = bottom;
        }
        let seq = unsafe { &mut parameters.seq_fields.bits };
        seq.set_chroma_format_idc(1);
        seq.set_frame_mbs_only_flag(1);
        seq.set_direct_8x8_inference_flag(1);
        seq.set_log2_max_frame_num_minus4(LOG2_MAX_FRAME_NUM - 4);
        seq.set_pic_order_cnt_type(0);
        seq.set_log2_max_pic_order_cnt_lsb_minus4(LOG2_MAX_POC_LSB - 4);
        let vui = unsafe { &mut parameters.vui_fields.bits };
        vui.set_timing_info_present_flag(1);
        vui.set_bitstream_restriction_flag(1);
        vui.set_motion_vectors_over_pic_boundaries_flag(1);
        vui.set_log2_max_mv_length_horizontal(16);
        vui.set_log2_max_mv_length_vertical(16);
        parameters
    }

    // Rate control, buffer model and frame rate: VAEncMiscParameterBuffer headers followed
    // by their payload
    fn rate_control(&self) -> [Vec<u8>; 3] {
        let bitrate = self.settings.bitrate;
        let mut rate_control = VAEncMiscParameterRateControl {
            bits_per_second: bitrate,
            target_percentage: 100,
            window_size: 1000,
            ..Default::default()
        };
        // A skipped frame would freeze the picture; better a short burst over the target
        unsafe { rate_control.rc_flags.bits.set_disable_frame_skip(1) };
        // One second of buffer, starting three quarters full
        let hrd = VAEncMiscParameterHRD {
            buffer_size: bitrate,
            initial_buffer_fullness: bitrate / 4 * 3,
            ..Default::default()
        };
        let (numerator, denominator) = self.settings.frame_rate;
        let frame_rate = VAEncMiscParameterFrameRate {
            framerate: numerator | (denominator << 16),
            ..Default::default()
        };
        [
            misc(VAEncMiscParameterTypeRateControl, bytes_of(&rate_control)),
            misc(VAEncMiscParameterTypeHRD, bytes_of(&hrd)),
            misc(VAEncMiscParameterTypeFrameRate, bytes_of(&frame_rate)),
        ]
    }

    fn picture_parameters(
        &self,
        reconstructed: &Surface,
        idr: bool,
        frame_num: u32,
        poc: i32,
    ) -> VAEncPictureParameterBufferH264 {
        let mut parameters = VAEncPictureParameterBufferH264 {
            CurrPic: VAPictureH264 {
                picture_id: reconstructed.id,
                frame_idx: frame_num,
                flags: 0,
                TopFieldOrderCnt: poc,
                BottomFieldOrderCnt: poc,
                va_reserved: [0; 4],
            },
            ReferenceFrames: [invalid_picture(); 16],
            coded_buf: self.coded_buffer,
            frame_num: frame_num as u16,
            pic_init_qp: 26,
            ..Default::default()
        };
        if let Some(reference) = &self.reference {
            parameters.ReferenceFrames[0] = reference_picture(reference);
        }
        let fields = unsafe { &mut parameters.pic_fields.bits };
        fields.set_idr_pic_flag(idr as u32);
        fields.set_reference_pic_flag(1);
        fields.set_entropy_coding_mode_flag(self.profile.cabac() as u32);
        fields.set_deblocking_filter_control_present_flag(1);
        parameters
    }

    fn slice_parameters(
        &self,
        stream: &StreamSettings,
        idr: bool,
        poc: i32,
    ) -> VAEncSliceParameterBufferH264 {
        let mut parameters = VAEncSliceParameterBufferH264 {
            macroblock_address: 0,
            num_macroblocks: stream.width_in_mbs() * stream.height_in_mbs(),
            macroblock_info: VA_INVALID_ID,
            slice_type: if idr { SLICE_TYPE_I } else { SLICE_TYPE_P },
            idr_pic_id: self.idr_pic_id,
            pic_order_cnt_lsb: (poc % (1 << LOG2_MAX_POC_LSB)) as u16,
            RefPicList0: [invalid_picture(); 32],
            RefPicList1: [invalid_picture(); 32],
            ..Default::default()
        };
        if let (false, Some(reference)) = (idr, &self.reference) {
            parameters.RefPicList0[0] = reference_picture(reference);
        }
        parameters
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        let display = &self.display;
        unsafe {
            if self.coded_buffer != VA_INVALID_ID {
                (display.api.destroy_buffer)(display.raw, self.coded_buffer);
            }
            if self.context != VA_INVALID_ID {
                (display.api.destroy_context)(display.raw, self.context);
            }
            if self.config != VA_INVALID_ID {
                (display.api.destroy_config)(display.raw, self.config);
            }
        }
    }
}

fn reference_picture(reference: &Reference) -> VAPictureH264 {
    VAPictureH264 {
        picture_id: reference.surface.id,
        frame_idx: reference.frame_num,
        flags: VA_PICTURE_H264_SHORT_TERM_REFERENCE,
        TopFieldOrderCnt: reference.poc,
        BottomFieldOrderCnt: reference.poc,
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

fn misc(kind: VAEncMiscParameterType, payload: &[u8]) -> Vec<u8> {
    [&kind.to_ne_bytes()[..], payload].concat()
}

// The bytes of a plain C parameter struct, as VA-API buffers take them
fn bytes_of<T: Copy>(value: &T) -> &[u8] {
    unsafe { slice::from_raw_parts(value as *const T as *const u8, mem::size_of::<T>()) }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    use super::*;
    use crate::{
        codec::h264::{bits::unescape, params::parse_sps},
        linux::{gpu, vaapi::H264Decoder, vaapi::JpegDecoder},
        rtsp::h264::{nal_type, split_annex_b, NAL_IDR, NAL_SPS},
    };

    fn ffmpeg(args: &str, input: &[u8]) -> Vec<u8> {
        let mut child = Command::new("ffmpeg")
            .args("-hide_banner -loglevel error".split_whitespace())
            .args(args.split_whitespace())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("ffmpeg not found");
        let mut stdin = child.stdin.take().unwrap();
        let input = input.to_vec();
        // Written from another thread: ffmpeg's output would fill the pipe meanwhile
        let writer = std::thread::spawn(move || stdin.write_all(&input));
        let output = child.wait_with_output().unwrap();
        writer.join().unwrap().unwrap();
        assert!(output.status.success(), "ffmpeg {args}");
        output.stdout
    }

    // Pictures of a moving test pattern, as an MJPEG camera would send them
    fn camera_pictures(size: &str, seconds: u32) -> Vec<Vec<u8>> {
        let stream = ffmpeg(
            &format!(
                "-f lavfi -i testsrc2=size={size}:rate=30 -t {seconds} -c:v mjpeg -q:v 2 \
                 -pix_fmt yuvj422p -f mjpeg -"
            ),
            &[],
        );
        let mut pictures = Vec::new();
        let mut start = 0;
        for end in 1..stream.len() {
            if stream[end - 1] == 0xff && stream[end] == 0xd9 {
                pictures.push(stream[start..=end].to_vec());
                start = end + 1;
            }
        }
        pictures
    }

    fn render_node() -> std::path::PathBuf {
        gpu::probe()
            .into_iter()
            .find(|gpu| {
                gpu.vaapi
                    .as_ref()
                    .is_ok_and(|caps| caps.jpeg_decode && caps.h264_encode)
            })
            .expect("no GPU decodes JPEG and encodes H.264 with VA-API")
            .render_node
    }

    fn luma_psnr(a: &[u8], b: &[u8]) -> f64 {
        let squared: f64 = a
            .iter()
            .zip(b)
            .map(|(&x, &y)| (x as f64 - y as f64).powi(2))
            .sum();
        10.0 * (255.0f64.powi(2) / (squared / a.len() as f64)).log10()
    }

    #[test]
    #[ignore = "needs ffmpeg and a VA-API GPU, run with: cargo test -- --ignored --nocapture"]
    fn encodes_decoded_camera_frames() {
        let (width, height, bitrate) = (1280, 720, 4_000_000);
        let pictures = camera_pictures("1280x720", 3);
        let node = render_node();
        let mut decoder = JpegDecoder::new(&node).unwrap();
        let mut encoder = H264Encoder::new(
            &node,
            EncoderSettings {
                width,
                height,
                frame_rate: (30, 1),
                bitrate,
                low_power: false,
            },
        )
        .unwrap();
        println!(
            "profile {:?}, level {}, packed headers {}",
            encoder.profile, encoder.level_idc, encoder.packed_headers
        );

        let forced = 45;
        let started = std::time::Instant::now();
        let mut units = Vec::new();
        let mut idr_frames = Vec::new();
        for (index, picture) in pictures.iter().enumerate() {
            let frame = decoder.decode(picture, index as i64 * 333_333).unwrap();
            let encoded = encoder.encode(&frame, index == forced).unwrap();
            assert_eq!(encoded.timestamp_hns, frame.timestamp_hns);
            let nals: Vec<u8> = split_annex_b(&encoded.data)
                .into_iter()
                .map(nal_type)
                .collect();
            if nals.contains(&NAL_IDR) {
                assert!(nals.contains(&NAL_SPS), "frame {index}: IDR without SPS");
                idr_frames.push(index);
            }
            units.push(encoded.data);
        }
        let stream = units.concat();
        println!(
            "{} frames, {:.2} ms per frame (JPEG decode included), {} bytes",
            pictures.len(),
            started.elapsed().as_secs_f64() * 1000.0 / pictures.len() as f64,
            stream.len()
        );
        // One IDR per second, and the one asked for
        assert_eq!(idr_frames, [0, 30, 45, 75]);

        // Our parameter sets, not the driver's
        let sps_nal = split_annex_b(&stream)
            .into_iter()
            .find(|nal| nal_type(nal) == NAL_SPS)
            .unwrap();
        let sps = parse_sps(&unescape(&sps_nal[1..])).unwrap();
        assert_eq!(sps.visible_rect(), (0, 0, width, height));
        assert!(sps.full_range);
        assert_eq!(sps.matrix_coefficients, 6);
        assert_eq!(sps.max_num_reorder_frames, Some(0));

        // Constant bitrate: a few percent around the target over the whole clip
        let actual = stream.len() as f64 * 8.0 / (pictures.len() as f64 / 30.0);
        println!(
            "bitrate {:.2} Mbit/s for {:.2}",
            actual / 1e6,
            bitrate as f64 / 1e6
        );
        assert!(
            (actual / bitrate as f64 - 1.0).abs() < 0.15,
            "bitrate {actual} far from {bitrate}"
        );

        // Decodes in ffmpeg, close to the input pictures
        let decoded = ffmpeg("-f h264 -i - -f rawvideo -pix_fmt gray -", &stream);
        let source = ffmpeg(
            "-f mjpeg -i - -f rawvideo -pix_fmt gray -",
            &pictures.concat(),
        );
        let frame_size = (width * height) as usize;
        assert_eq!(decoded.len(), source.len(), "frame count");
        let psnr: Vec<f64> = decoded
            .chunks(frame_size)
            .zip(source.chunks(frame_size))
            .map(|(a, b)| luma_psnr(a, b))
            .collect();
        let worst = psnr.iter().copied().fold(f64::MAX, f64::min);
        let mean = psnr.iter().sum::<f64>() / psnr.len() as f64;
        println!("luma PSNR: mean {mean:.1} dB, worst {worst:.1} dB");
        assert!(worst > 30.0, "picture quality too low");

        // And in our own decoder, identical to ffmpeg's decoding
        let mut h264 = H264Decoder::new(&node).unwrap();
        let mut ours = Vec::new();
        for (index, unit) in units.iter().enumerate() {
            for frame in h264.decode(unit, index as i64).unwrap() {
                let nv12 = frame.read_nv12().unwrap();
                ours.extend_from_slice(&nv12[..frame_size]);
            }
        }
        for frame in h264.flush() {
            ours.extend_from_slice(&frame.read_nv12().unwrap()[..frame_size]);
        }
        assert!(ours == decoded, "our decoder differs from ffmpeg");
    }
}
