// JPEG decoding on VA-API, for MJPEG cameras. Every picture is decoded on its own (no
// references), into a small pool of surfaces that consumers return when they drop the frame
use std::{ffi::c_void, mem, path::Path, sync::Arc};

use super::{
    api::Display,
    surface::{DecodedFrame, SurfacePool},
    sys::*,
};
use crate::codec::jpeg::{self, Frame, HuffmanTable, Subsampling};

// The picture being decoded plus what consumers hold: the encoder's queue (2) and the
// picture it encodes, each preview's queue (1) and the picture on screen, with room for
// two previews and some slack. A 720p NV12 surface is 1.4 MB
const SURFACE_COUNT: u32 = 10;

// Config, context and surfaces for one picture size and chroma layout
struct Session {
    display: Arc<Display>,
    config: VAConfigID,
    context: VAContextID,
    pool: Arc<SurfacePool>,
    layout: (u16, u16, Subsampling),
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            (self.display.api.destroy_context)(self.display.raw, self.context);
            (self.display.api.destroy_config)(self.display.raw, self.config);
        }
    }
}

pub(crate) struct JpegDecoder {
    display: Arc<Display>,
    session: Option<Session>,
}

impl JpegDecoder {
    pub fn new(render_node: &Path) -> Result<Self, String> {
        let display = Display::shared(render_node)?;
        if !display.profiles().contains(&VAProfileJPEGBaseline) {
            return Err("the GPU cannot decode JPEG".to_string());
        }
        Ok(JpegDecoder {
            display,
            session: None,
        })
    }

    pub fn decode(&mut self, data: &[u8], timestamp_hns: i64) -> Result<DecodedFrame, String> {
        let frame = jpeg::parse(data).map_err(|e| e.to_string())?;
        let layout = (
            frame.width,
            frame.height,
            frame.subsampling().map_err(|e| e.to_string())?,
        );
        if self
            .session
            .as_ref()
            .is_none_or(|session| session.layout != layout)
        {
            // Frames of the old layout stay valid: their pool lives until they are dropped
            self.session = None;
            self.session = Some(self.create_session(layout)?);
        }
        let session = self.session.as_ref().unwrap();
        let surface = session
            .pool
            .acquire()
            .ok_or("every surface is still in use (frames held for too long?)")?;

        let mut buffers = Vec::new();
        let decoded = self.submit(session, surface.id, &frame, &mut buffers);
        let display = &self.display;
        for buffer in buffers {
            unsafe { (display.api.destroy_buffer)(display.raw, buffer) };
        }
        decoded?;

        Ok(DecodedFrame {
            surface: Arc::new(surface),
            timestamp_hns,
            visible: (0, 0, frame.width as u32, frame.height as u32),
            // JFIF: full range BT.601, whatever the size
            full_range: true,
            bt709: false,
        })
    }

    fn create_session(&self, layout: (u16, u16, Subsampling)) -> Result<Session, String> {
        let (width, height) = (layout.0 as u32, layout.1 as u32);
        // NV12 whatever the chroma layout of the pictures: the decoder converts while it
        // writes, and the preview and the H.264 encoder both take NV12
        let rt_format = VA_RT_FORMAT_YUV420;
        let display = self.display.clone();
        let api = display.api;
        unsafe {
            let mut attrib = VAConfigAttrib {
                type_: VAConfigAttribRTFormat,
                value: rt_format,
            };
            let mut config = VA_INVALID_ID;
            api.check(
                "vaCreateConfig",
                (api.create_config)(
                    display.raw,
                    VAProfileJPEGBaseline,
                    VAEntrypointVLD,
                    &mut attrib,
                    1,
                    &mut config,
                ),
            )?;

            let pool = match SurfacePool::new(
                display.clone(),
                rt_format,
                VA_FOURCC_NV12,
                width,
                height,
                SURFACE_COUNT,
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
                layout,
            })
        }
    }

    fn submit(
        &self,
        session: &Session,
        surface: VASurfaceID,
        frame: &Frame,
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

        let picture = picture_parameters(frame);
        let quant = quant_tables(frame);
        let huffman = huffman_tables(frame)?;
        let slice = slice_parameters(frame);
        let mut parameters = [
            create(
                VAPictureParameterBufferType,
                mem::size_of_val(&picture),
                &picture as *const _ as _,
            )?,
            create(
                VAIQMatrixBufferType,
                mem::size_of_val(&quant),
                &quant as *const _ as _,
            )?,
            create(
                VAHuffmanTableBufferType,
                mem::size_of_val(&huffman),
                &huffman as *const _ as _,
            )?,
        ];
        let mut scan = [
            create(
                VASliceParameterBufferType,
                mem::size_of_val(&slice),
                &slice as *const _ as _,
            )?,
            create(
                VASliceDataBufferType,
                frame.scan_data.len(),
                frame.scan_data.as_ptr() as _,
            )?,
        ];

        unsafe {
            api.check(
                "vaBeginPicture",
                (api.begin_picture)(display.raw, context, surface),
            )?;
            let rendered = api
                .check(
                    "vaRenderPicture",
                    (api.render_picture)(
                        display.raw,
                        context,
                        parameters.as_mut_ptr(),
                        parameters.len() as i32,
                    ),
                )
                .and_then(|_| {
                    api.check(
                        "vaRenderPicture",
                        (api.render_picture)(display.raw, context, scan.as_mut_ptr(), 2),
                    )
                });
            // Always ends the picture, even after a failed render, so the context stays usable
            let ended = api.check("vaEndPicture", (api.end_picture)(display.raw, context));
            rendered.and(ended)
        }
    }
}

fn picture_parameters(frame: &Frame) -> VAPictureParameterBufferJPEGBaseline {
    let mut parameters = VAPictureParameterBufferJPEGBaseline {
        picture_width: frame.width,
        picture_height: frame.height,
        num_components: frame.components.len() as u8,
        ..Default::default()
    };
    for (slot, component) in parameters.components.iter_mut().zip(&frame.components) {
        slot.component_id = component.id;
        slot.h_sampling_factor = component.h_sampling;
        slot.v_sampling_factor = component.v_sampling;
        slot.quantiser_table_selector = component.quant_table;
    }
    parameters
}

// Tables in coded (zig-zag) order, as they come in the picture
fn quant_tables(frame: &Frame) -> VAIQMatrixBufferJPEGBaseline {
    let mut tables = VAIQMatrixBufferJPEGBaseline::default();
    for (id, table) in frame.quant_tables.iter().enumerate() {
        if let Some(table) = table {
            tables.load_quantiser_table[id] = 1;
            tables.quantiser_table[id] = *table;
        }
    }
    tables
}

fn huffman_tables(frame: &Frame) -> Result<VAHuffmanTableBufferJPEGBaseline, String> {
    let mut tables = VAHuffmanTableBufferJPEGBaseline::default();
    for id in 0..2 {
        let (Some(dc), Some(ac)) = (&frame.dc_tables[id], &frame.ac_tables[id]) else {
            continue;
        };
        let target = &mut tables.huffman_table[id];
        target.num_dc_codes = dc.counts;
        copy_values(dc, &mut target.dc_values)?;
        target.num_ac_codes = ac.counts;
        copy_values(ac, &mut target.ac_values)?;
        tables.load_huffman_table[id] = 1;
    }
    Ok(tables)
}

fn copy_values(table: &HuffmanTable, target: &mut [u8]) -> Result<(), String> {
    target
        .get_mut(..table.values.len())
        .ok_or("invalid JPEG Huffman table: too many codes")?
        .copy_from_slice(&table.values);
    Ok(())
}

fn slice_parameters(frame: &Frame) -> VASliceParameterBufferJPEGBaseline {
    let mut parameters = VASliceParameterBufferJPEGBaseline {
        slice_data_size: frame.scan_data.len() as u32,
        slice_data_offset: 0,
        slice_data_flag: VA_SLICE_DATA_FLAG_ALL,
        num_components: frame.scan.len() as u8,
        restart_interval: frame.restart_interval,
        num_mcus: frame.mcu_count(),
        ..Default::default()
    };
    for (slot, scan) in parameters.components.iter_mut().zip(&frame.scan) {
        slot.component_selector = frame.components[scan.index].id;
        slot.dc_table_selector = scan.dc_table;
        slot.ac_table_selector = scan.ac_table;
    }
    parameters
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        process::{Command, Stdio},
        time::Duration,
    };

    use super::*;
    use crate::linux::{gpu, v4l2};

    fn ffmpeg(args: &str, input: &[u8]) -> Vec<u8> {
        let mut child = Command::new("ffmpeg")
            .args("-hide_banner -loglevel error".split_whitespace())
            .args(args.split_whitespace())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("ffmpeg not found");
        child.stdin.take().unwrap().write_all(input).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "ffmpeg {args}");
        output.stdout
    }

    // ffmpeg's own decoding of the picture, made NV12 the way the hardware does it: chroma
    // rows (and columns) are picked, not averaged
    fn reference_nv12(picture: &[u8]) -> Vec<u8> {
        let frame = jpeg::parse(picture).unwrap();
        let (width, height) = (frame.width as usize, frame.height as usize);
        let (row_step, column_step) = match frame.subsampling().unwrap() {
            jpeg::Subsampling::Yuv420 => (1, 1),
            jpeg::Subsampling::Yuv422 => (2, 1),
            other => panic!("no reference for {other:?}"),
        };
        // Planar output in the picture's own layout, nothing resampled
        let planar = ffmpeg("-f mjpeg -i - -f rawvideo -", picture);
        let (chroma_width, chroma_height) = (width.div_ceil(2), height.div_ceil(2) * row_step);
        let chroma = |plane: usize, x: usize, y: usize| {
            planar[width * height
                + plane * chroma_width * chroma_height
                + y * row_step * chroma_width
                + x * column_step]
        };
        let mut nv12 = planar[..width * height].to_vec();
        for y in 0..height / 2 {
            for x in 0..width / 2 {
                nv12.extend([chroma(0, x, y), chroma(1, x, y)]);
            }
        }
        nv12
    }

    // The same picture without its DHT segments, as UVC cameras often send it
    fn without_huffman_tables(picture: &[u8]) -> Vec<u8> {
        let mut output = picture[..2].to_vec();
        let mut position = 2;
        loop {
            let marker = picture[position + 1];
            if marker == 0xda {
                output.extend_from_slice(&picture[position..]);
                return output;
            }
            let length = u16::from_be_bytes([picture[position + 2], picture[position + 3]]);
            let end = position + 2 + length as usize;
            if marker != 0xc4 {
                output.extend_from_slice(&picture[position..end]);
            }
            position = end;
        }
    }

    // Hardware and ffmpeg IDCTs round differently: one level at most
    fn assert_close(name: &str, frame: &DecodedFrame, reference: &[u8]) {
        let decoded = frame.read_nv12().unwrap();
        assert_eq!(decoded.len(), reference.len(), "{name}: size");
        let luma = (frame.visible.2 * frame.visible.3) as usize;
        for (plane, range) in [("Y", 0..luma), ("UV", luma..decoded.len())] {
            let mut errors: Vec<u8> = decoded[range.clone()]
                .iter()
                .zip(&reference[range])
                .map(|(a, b)| a.abs_diff(*b))
                .collect();
            let mean = errors.iter().map(|&e| e as f64).sum::<f64>() / errors.len() as f64;
            errors.sort_unstable();
            let (p99, max) = (errors[errors.len() * 99 / 100], errors[errors.len() - 1]);
            println!("{name} {plane}: mean difference {mean:.3}, 99th percentile {p99}, max {max}");
            assert!(
                mean < 0.05 && max <= 1,
                "{name}: {plane} differs from ffmpeg"
            );
        }
    }

    fn render_node() -> std::path::PathBuf {
        gpu::probe()
            .into_iter()
            .find(|gpu| gpu.vaapi.as_ref().is_ok_and(|caps| caps.jpeg_decode))
            .expect("no GPU decodes JPEG with VA-API")
            .render_node
    }

    #[test]
    #[ignore = "needs ffmpeg and a VA-API GPU, run with: cargo test -- --ignored --nocapture"]
    fn decodes_like_ffmpeg() {
        // ffmpeg writes 4:2:2 with sampling 2x2,1x2,1x2, cameras 2x1,1x1,1x1: both decode the
        // same. Its 4:4:4 (1x2 for every component) is refused by Intel's driver, also when
        // ffmpeg decodes it with VA-API, and is not something cameras send
        let cases = [
            ("4:2:0", "640x480", "-pix_fmt yuvj420p"),
            // Annex K tables, so the picture also decodes once its DHT is removed
            ("4:2:2", "640x480", "-pix_fmt yuvj422p -huffman default"),
            // Not a whole number of MCUs
            ("4:2:2-cropped", "360x202", "-pix_fmt yuvj422p"),
            (
                "4:2:0-optimized-tables",
                "320x240",
                "-pix_fmt yuvj420p -huffman optimal",
            ),
        ];
        // One decoder for every case: the session follows each change of size and layout
        let mut decoder = JpegDecoder::new(&render_node()).unwrap();
        for (name, size, args) in cases {
            let picture = ffmpeg(
                &format!(
                    "-f lavfi -i testsrc2=size={size} -frames:v 1 -c:v mjpeg -q:v 3 {args} -f mjpeg -"
                ),
                &[],
            );
            let reference = reference_nv12(&picture);
            assert_close(name, &decoder.decode(&picture, 0).unwrap(), &reference);
            // Same picture through the default tables of Annex K
            if name == "4:2:2" {
                let stripped = without_huffman_tables(&picture);
                assert!(jpeg::parse(&stripped).unwrap().dc_tables[0].is_some());
                assert_close(
                    "4:2:2-no-dht",
                    &decoder.decode(&stripped, 0).unwrap(),
                    &reference,
                );
            }
        }
    }

    #[test]
    #[ignore = "needs ffmpeg and a VA-API GPU, run with: cargo test -- --ignored --nocapture"]
    fn reuses_surfaces_once_frames_are_dropped() {
        let picture = ffmpeg(
            "-f lavfi -i testsrc2=size=320x240 -frames:v 1 -c:v mjpeg -f mjpeg -",
            &[],
        );
        let mut decoder = JpegDecoder::new(&render_node()).unwrap();
        let held: Vec<DecodedFrame> = (0..SURFACE_COUNT)
            .map(|index| decoder.decode(&picture, index as i64).unwrap())
            .collect();
        assert!(
            decoder.decode(&picture, 99).is_err(),
            "pool should be empty"
        );
        drop(held);
        for index in 0..3 * SURFACE_COUNT {
            decoder.decode(&picture, index as i64).unwrap();
        }
    }

    #[test]
    #[ignore = "opens the real camera, needs ffmpeg and a VA-API GPU, run with: cargo test -- --ignored --nocapture"]
    fn decodes_the_camera_mjpeg() {
        let device = v4l2::list_devices()
            .into_iter()
            .next()
            .expect("no camera found");
        let mode = v4l2::choose_mode(&device, |format| format == v4l2::PixelFormat::Mjpeg)
            .expect("the camera does not send MJPEG");
        let (sender, receiver) = crossbeam_channel::unbounded();
        let stream = v4l2::CaptureStream::start(&mode, move |frame| {
            let _ = sender.send(frame.data.to_vec());
        })
        .unwrap();
        // The first pictures after the camera starts may be dark or incomplete
        let pictures: Vec<Vec<u8>> = (0..15)
            .map_while(|_| receiver.recv_timeout(Duration::from_secs(3)).ok())
            .collect();
        drop(stream);
        assert_eq!(pictures.len(), 15, "the camera stopped sending");

        let mut decoder = JpegDecoder::new(&render_node()).unwrap();
        let started = std::time::Instant::now();
        for (index, picture) in pictures.iter().enumerate() {
            decoder
                .decode(picture, index as i64)
                .unwrap()
                .sync()
                .unwrap();
        }
        println!(
            "{} {}x{}: {:.2} ms per picture",
            device.name,
            mode.width,
            mode.height,
            started.elapsed().as_secs_f64() * 1000.0 / pictures.len() as f64
        );
        let last = pictures.last().unwrap();
        assert_close(
            "camera",
            &decoder.decode(last, 0).unwrap(),
            &reference_nv12(last),
        );
    }
}
