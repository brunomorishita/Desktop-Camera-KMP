use std::{
    collections::VecDeque,
    mem::ManuallyDrop,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
};

use crossbeam_channel::{bounded, select, Receiver, Sender};
use windows::{
    core::Interface,
    Win32::{
        Graphics::Direct3D11::ID3D11Texture2D,
        Media::MediaFoundation::{
            eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Main, ICodecAPI, IMF2DBuffer,
            IMFActivate, IMFAttributes, IMFDXGIDeviceManager, IMFMediaEventGenerator,
            IMFMediaType, IMFSample, IMFShutdown, IMFTransform, MFCreateAttributes,
            MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType,
            MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFShutdown, MFStartup,
            MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
            CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
            CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame,
            CODECAPI_AVLowLatencyMode, MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS,
            METransformDrainComplete, METransformHaveOutput, METransformNeedInput,
            MFSTARTUP_FULL, MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_ADAPTER_LUID, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER,
            MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
            MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
            MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER,
            MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES,
            MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_EVENT_TYPE,
            MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE,
            MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE,
            MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC,
            MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
        },
        System::{
            Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED},
            Variant::VARIANT,
        },
    },
};

use crate::{
    media::{EncodedFrame, EncodedSink, EncoderConfig, VideoEncoder},
    win32::{device::GpuDevice, frame::SharedFrame, Win32Backend},
    CameraError,
};

// Encoder thread handle: Media Foundation objects live only inside that thread
pub struct H264Encoder {
    shutdown: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl VideoEncoder<Win32Backend> for H264Encoder {
    fn spawn(
        gpu: GpuDevice,
        config: EncoderConfig,
        frames: Receiver<SharedFrame>,
        force_keyframe: Arc<AtomicBool>,
        sink: EncodedSink,
    ) -> Result<Self, CameraError> {
        let (shutdown, shutdown_receiver) = bounded::<()>(1);
        let (ready, ready_receiver) = bounded::<Result<(), CameraError>>(1);

        let thread = std::thread::spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }

            match EncoderSession::new(&gpu, &config) {
                Ok(session) => {
                    let _ = ready.send(Ok(()));
                    if let Err(e) =
                        session.run(&frames, &shutdown_receiver, &force_keyframe, sink)
                    {
                        println!("H.264 encoder stopped: {e}");
                    }
                    session.close();
                }
                Err(e) => {
                    let _ = ready.send(Err(e));
                }
            }

            unsafe {
                let _ = MFShutdown();
                CoUninitialize();
            }
        });

        // Report initialization errors (no hardware encoder, unsupported size...) to the caller
        let init_result = ready_receiver.recv().unwrap_or_else(|_| {
            Err(CameraError::CommonError {
                msg: "Encoder thread exited unexpectedly".to_string(),
            })
        });
        if let Err(e) = init_result {
            let _ = thread.join();
            return Err(e);
        }

        Ok(H264Encoder {
            shutdown: Some(shutdown),
            thread: Some(thread),
        })
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        // Dropping the sender wakes the encoder loop
        drop(self.shutdown.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct EncoderSession {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec_api: Option<ICodecAPI>,
    // Kept alive while the transform uses it
    _device_manager: IMFDXGIDeviceManager,
    provides_samples: bool,
    output_buffer_size: u32,
    frame_duration_hns: i64,
}

impl EncoderSession {
    fn new(gpu: &GpuDevice, config: &EncoderConfig) -> Result<Self, CameraError> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_FULL)?;

            // Encoder works directly on our D3D11 textures
            let mut reset_token = 0u32;
            let mut device_manager: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut reset_token, &mut device_manager)?;
            let device_manager = device_manager.ok_or_else(|| common("No DXGI device manager"))?;
            device_manager.ResetDevice(&gpu.device, reset_token)?;

            let transform = find_hardware_encoder(gpu)?;

            let attributes = transform.GetAttributes()?;
            // Hardware encoders are asynchronous MFTs, driven by events
            if attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 {
                return Err(common("Hardware H.264 encoder is not asynchronous"));
            }
            attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);

            transform.ProcessMessage(
                MFT_MESSAGE_SET_D3D_MANAGER,
                device_manager.as_raw() as usize,
            )?;

            // Rate control and GOP must be set before media types on some encoders
            let codec_api = transform.cast::<ICodecAPI>().ok();
            if let Some(codec_api) = &codec_api {
                let fps = config.frame_rate.numerator.div_ceil(config.frame_rate.denominator);
                let _ = codec_api.SetValue(
                    &CODECAPI_AVEncCommonRateControlMode,
                    &VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
                );
                let _ = codec_api
                    .SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(config.bitrate));
                // One keyframe per second, so clients recover quickly from packet loss
                let _ = codec_api.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(fps.max(1)));
                // No B-frames and no frame buffering inside the encoder
                let _ = codec_api.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
            }

            // Encoders require the output type before the input type
            let output_type = MFCreateMediaType()?;
            output_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output_type.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate)?;
            output_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            set_video_format(&output_type, config)?;
            transform.SetOutputType(0, &output_type, 0)?;

            let input_type = MFCreateMediaType()?;
            input_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            set_video_format(&input_type, config)?;
            transform.SetInputType(0, &input_type, 0)?;

            let stream_info = transform.GetOutputStreamInfo(0)?;
            let provides_samples = stream_info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                    as u32
                != 0;

            let events: IMFMediaEventGenerator = transform.cast()?;

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            let frame_duration_hns = 10_000_000i64 * config.frame_rate.denominator as i64
                / config.frame_rate.numerator as i64;

            Ok(EncoderSession {
                transform,
                events,
                codec_api,
                _device_manager: device_manager,
                provides_samples,
                output_buffer_size: stream_info.cbSize.max(config.width * config.height),
                frame_duration_hns,
            })
        }
    }

    fn run(
        &self,
        frames: &Receiver<SharedFrame>,
        shutdown: &Receiver<()>,
        force_keyframe: &AtomicBool,
        mut sink: EncodedSink,
    ) -> Result<(), CameraError> {
        // Frames given to the encoder, kept out of the pool until their output comes out
        let mut in_flight: VecDeque<SharedFrame> = VecDeque::new();

        loop {
            // Never blocks for long: after each input the encoder always emits output
            let event = unsafe { self.events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)) }?;
            let event_type = MF_EVENT_TYPE(unsafe { event.GetType() }? as i32);

            if event_type == METransformNeedInput {
                let frame = select! {
                    recv(frames) -> msg => match msg {
                        Ok(frame) => frame,
                        Err(_) => break,
                    },
                    recv(shutdown) -> _ => break,
                };

                if force_keyframe.swap(false, Ordering::Relaxed) {
                    if let Some(codec_api) = &self.codec_api {
                        let _ = unsafe {
                            codec_api
                                .SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32))
                        };
                    }
                }

                self.process_input(&frame)?;
                in_flight.push_back(frame);
            } else if event_type == METransformHaveOutput {
                if let Some(encoded) = self.process_output()? {
                    // No reordering in low latency mode: older inputs are fully consumed
                    while in_flight
                        .front()
                        .is_some_and(|f| f.timestamp_hns <= encoded.timestamp_hns)
                    {
                        in_flight.pop_front();
                    }
                    sink(encoded);
                }
            } else if event_type == METransformDrainComplete {
                break;
            }
        }

        Ok(())
    }

    fn process_input(&self, frame: &SharedFrame) -> Result<(), CameraError> {
        unsafe {
            let buffer =
                MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, frame.texture(), 0, false)?;
            let length = buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()?;
            buffer.SetCurrentLength(length)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(frame.timestamp_hns)?;
            sample.SetSampleDuration(self.frame_duration_hns)?;

            self.transform.ProcessInput(0, &sample, 0)?;
        }

        Ok(())
    }

    fn process_output(&self) -> Result<Option<EncodedFrame>, CameraError> {
        unsafe {
            let own_sample = if self.provides_samples {
                None
            } else {
                let sample = MFCreateSample()?;
                sample.AddBuffer(&MFCreateMemoryBuffer(self.output_buffer_size)?)?;
                Some(sample)
            };

            let mut output = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(own_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = self
                .transform
                .ProcessOutput(0, &mut output, &mut status);

            let [output] = output;
            let sample: Option<IMFSample> = ManuallyDrop::into_inner(output.pSample);
            drop(ManuallyDrop::into_inner(output.pEvents));

            if let Err(e) = result {
                if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                    // Encoder changed its output format: accept the one it proposes
                    let new_type = self.transform.GetOutputAvailableType(0, 0)?;
                    self.transform.SetOutputType(0, &new_type, 0)?;
                    return Ok(None);
                }
                return Err(e.into());
            }

            let Some(sample) = sample else {
                return Ok(None);
            };

            let timestamp_hns = sample.GetSampleTime().unwrap_or(0);
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut data_ptr: *mut u8 = std::ptr::null_mut();
            let mut length = 0u32;
            buffer.Lock(&mut data_ptr, None, Some(&mut length))?;
            let data = std::slice::from_raw_parts(data_ptr, length as usize).to_vec();
            buffer.Unlock()?;

            Ok(Some(EncodedFrame {
                data,
                timestamp_hns,
            }))
        }
    }

    fn close(&self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            // Async MFTs hold a reference to themselves until shut down
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
        }
    }
}

// Hardware H.264 encoder on the same adapter as our device, so it can read our textures
fn find_hardware_encoder(gpu: &GpuDevice) -> Result<IMFTransform, CameraError> {
    unsafe {
        let adapter_desc = gpu.adapter()?.GetDesc()?;
        let luid = adapter_desc.AdapterLuid;

        let mut attributes: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attributes, 1)?;
        let attributes = attributes.ok_or_else(|| common("Failed to create attributes"))?;
        attributes.SetBlob(
            &MFT_ENUM_ADAPTER_LUID,
            std::slice::from_raw_parts(
                &luid as *const _ as *const u8,
                std::mem::size_of_val(&luid),
            ),
        )?;

        let input_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };

        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        windows::Win32::Media::MediaFoundation::MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input_info),
            Some(&output_info),
            &attributes,
            &mut activates,
            &mut count,
        )?;

        if activates.is_null() {
            return Err(common("No hardware H.264 encoder found"));
        }

        // Take ownership of every IMFActivate so all of them are released
        let list: Vec<Option<IMFActivate>> = std::slice::from_raw_parts_mut(activates, count as usize)
            .iter_mut()
            .map(Option::take)
            .collect();
        CoTaskMemFree(Some(activates as *const _));

        list.into_iter()
            .flatten()
            .find_map(|activate| activate.ActivateObject::<IMFTransform>().ok())
            .ok_or_else(|| common("No hardware H.264 encoder found"))
    }
}

fn set_video_format(media_type: &IMFMediaType, config: &EncoderConfig) -> Result<(), CameraError> {
    unsafe {
        media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(config.width, config.height))?;
        media_type.SetUINT64(
            &MF_MT_FRAME_RATE,
            pack(config.frame_rate.numerator, config.frame_rate.denominator),
        )?;
        media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    }
    Ok(())
}

// Same as MFSetAttributeSize / MFSetAttributeRatio from mfapi.h
fn pack(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

fn common(msg: &str) -> CameraError {
    CameraError::CommonError {
        msg: msg.to_string(),
    }
}
