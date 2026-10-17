use std::{
    net::Ipv4Addr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
};

use crossbeam_channel::{bounded, select, Sender};

use crate::{
    media::{
        CameraH264, CaptureSource, EncoderConfig, FrameHub, NativeEncoder, SharedEncodedFrame,
        VideoEncoder,
    },
    rtsp::server::{RtspServer, StreamPublisher},
    CameraController, CameraError, StreamConfig,
};

// Camera -> H.264 (hardware encoder, or the camera's own) -> RTP -> RTSP clients
pub(crate) struct Streaming {
    // Declared first so it is dropped first: clients stop before the source
    server: RtspServer,
    source: StreamSource,
}

enum StreamSource {
    // Subscribed to the controller's frame hub, unsubscribed by stop_streaming
    Encoder {
        _encoder: NativeEncoder,
        subscription: u64,
    },
    Passthrough {
        _forwarder: Passthrough,
    },
}

#[uniffi::export]
impl CameraController {
    /// Starts the RTSP server and returns the stream URL (rtsp://<ip>:<port>/<path>).
    /// When the camera sends H.264 itself it is streamed as is: the bitrate is then the
    /// camera's and `config.bitrate` is ignored
    pub async fn start_streaming(&self, config: StreamConfig) -> Result<String, CameraError> {
        // Validated before touching the camera, so a typo doesn't start the capture
        let bind_address: Ipv4Addr =
            config
                .bind_address
                .trim()
                .parse()
                .map_err(|_| CameraError::StreamError {
                    msg: format!("Invalid IPv4 address: {}", config.bind_address),
                })?;
        let path = config.path.trim().trim_matches('/');
        if path.is_empty() || path.contains(char::is_whitespace) {
            return Err(CameraError::StreamError {
                msg: format!("Invalid stream path: {}", config.path),
            });
        }

        let mut capture = self.capture.lock().await;
        let (dimensions, frame_rate) = self.ensure_capture(&mut capture).await?;

        if let Some(streaming) = self.streaming.lock().as_ref() {
            return Ok(streaming.server.url.clone());
        }

        let camera_h264 = capture.as_ref().and_then(|active| active.camera_h264());
        let publisher = StreamPublisher::new();
        let force_keyframe = Arc::new(AtomicBool::new(false));
        let request_keyframe = match &camera_h264 {
            Some(camera) => camera.request_keyframe.clone(),
            None => {
                let force_keyframe = force_keyframe.clone();
                Arc::new(move || force_keyframe.store(true, Ordering::Relaxed))
            }
        };
        let server = RtspServer::start(
            bind_address,
            config.port,
            path,
            config.transport,
            &publisher,
            request_keyframe,
        )?;

        let source = match camera_h264 {
            Some(camera) => StreamSource::Passthrough {
                _forwarder: Passthrough::spawn(camera, publisher)?,
            },
            None => {
                // Capacity 2: absorbs jitter between capture and encoder without adding latency
                let (subscription, frames) = self.hub.subscribe(2);
                let mut publisher = publisher;
                let encoder = NativeEncoder::spawn(
                    self.gpu()?,
                    EncoderConfig {
                        width: dimensions.width,
                        height: dimensions.height,
                        frame_rate,
                        bitrate: config.bitrate,
                    },
                    frames,
                    force_keyframe,
                    Box::new(move |frame| publisher.publish(&frame)),
                );

                match encoder {
                    Ok(encoder) => StreamSource::Encoder {
                        _encoder: encoder,
                        subscription,
                    },
                    Err(e) => {
                        self.hub.unsubscribe(subscription);
                        return Err(e);
                    }
                }
            }
        };

        let url = server.url.clone();
        *self.streaming.lock() = Some(Streaming { server, source });

        Ok(url)
    }

    /// Disconnects every client and stops the encoder. Camera keeps running for previews
    pub fn stop_streaming(&self) {
        let streaming = self.streaming.lock().take();
        if let Some(streaming) = streaming {
            if let StreamSource::Encoder { subscription, .. } = &streaming.source {
                self.hub.unsubscribe(*subscription);
            }
            drop(streaming);
        }
    }

    /// Number of clients currently receiving the stream
    pub fn streaming_clients(&self) -> u32 {
        self.streaming
            .lock()
            .as_ref()
            .map_or(0, |streaming| streaming.server.client_count())
    }
}

// Forwards the camera's H.264 to the RTSP publisher on its own thread until dropped
struct Passthrough {
    frames: Arc<FrameHub<SharedEncodedFrame>>,
    subscription: u64,
    shutdown: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Passthrough {
    fn spawn(camera: CameraH264, mut publisher: StreamPublisher) -> Result<Self, CameraError> {
        // A few frames of slack: forwarding is cheap, but a stalled publisher must not
        // make the capture thread drop frames mid GOP more than necessary
        let (subscription, frames) = camera.frames.subscribe(8);
        let (shutdown, shutdown_receiver) = bounded::<()>(1);

        let thread = std::thread::Builder::new()
            .name("h264-passthrough".to_string())
            .spawn(move || loop {
                select! {
                    recv(frames) -> msg => match msg {
                        Ok(frame) => publisher.publish(&frame),
                        Err(_) => break,
                    },
                    recv(shutdown_receiver) -> _ => break,
                }
            });

        match thread {
            Ok(thread) => Ok(Passthrough {
                frames: camera.frames,
                subscription,
                shutdown: Some(shutdown),
                thread: Some(thread),
            }),
            Err(e) => {
                camera.frames.unsubscribe(subscription);
                Err(CameraError::StreamError {
                    msg: format!("Failed to start the passthrough thread: {e}"),
                })
            }
        }
    }
}

impl Drop for Passthrough {
    fn drop(&mut self) {
        self.frames.unsubscribe(self.subscription);
        // Dropping the sender wakes the loop
        drop(self.shutdown.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        process::Command,
        time::{Duration, Instant},
    };

    use super::*;
    use crate::{
        media::EncodedFrame,
        rtsp::h264::{nal_type, split_annex_b, NAL_AUD, NAL_PPS, NAL_SPS},
        StreamTransport,
    };

    // Camera-like H.264 from ffmpeg: Main profile, no B-frames, 1 s GOP and an access unit
    // delimiter per frame
    const FFMPEG_ARGS: &str = "-hide_banner -loglevel error -y \
        -f lavfi -i testsrc=size=640x480:rate=30 -t 4 -pix_fmt yuv420p \
        -c:v libx264 -profile:v main -bf 0 -g 30 -x264-params aud=1 -f h264";

    fn camera_like_h264(dir: &std::path::Path) -> Vec<u8> {
        let output = dir.join("camera.h264");
        let status = Command::new("ffmpeg")
            .args(FFMPEG_ARGS.split_whitespace())
            .arg(&output)
            .status()
            .expect("ffmpeg not found");
        assert!(status.success(), "ffmpeg failed to encode the test stream");
        std::fs::read(output).unwrap()
    }

    // One buffer per frame, split at the delimiters, like V4L2 hands them out. SPS/PPS are
    // kept only in the first frame, as several UVC cameras send them once at stream start
    fn split_frames(stream: &[u8]) -> Vec<Vec<u8>> {
        let mut frames: Vec<Vec<u8>> = Vec::new();
        for nal in split_annex_b(stream) {
            let kind = nal_type(nal);
            if kind == NAL_AUD || frames.is_empty() {
                frames.push(Vec::new());
            }
            if frames.len() > 1 && (kind == NAL_SPS || kind == NAL_PPS) {
                continue;
            }
            let frame = frames.last_mut().unwrap();
            frame.extend_from_slice(&[0, 0, 0, 1]);
            frame.extend_from_slice(nal);
        }
        frames
    }

    // ffprobe connected over RTSP for 2 s: (width, height, frames decoded)
    fn probe(url: &str, transport: &str) -> (u32, u32, u32) {
        let output = Command::new("ffprobe")
            .args(["-v", "error", "-rtsp_transport", transport])
            // Socket timeout in microseconds: a broken server fails the test instead of hanging
            .args(["-timeout", "5000000", "-read_intervals", "%+2"])
            .args(["-select_streams", "v:0", "-count_frames"])
            .args(["-show_entries", "stream=width,height,nb_read_frames"])
            .args(["-of", "csv=p=0", url])
            .output()
            .expect("ffprobe not found");
        let text = String::from_utf8_lossy(&output.stdout);
        let fields: Vec<u32> = text
            .trim()
            .split(',')
            .filter_map(|field| field.trim().parse().ok())
            .collect();
        assert_eq!(
            fields.len(),
            3,
            "unexpected ffprobe output: {text} {}",
            String::from_utf8_lossy(&output.stderr)
        );
        (fields[0], fields[1], fields[2])
    }

    #[test]
    #[ignore = "needs ffmpeg and ffprobe, run with: cargo test -- --ignored --nocapture"]
    fn streams_camera_h264_without_encoding() {
        let dir = std::env::temp_dir().join(format!("camera-engine-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let frames = split_frames(&camera_like_h264(&dir));
        assert!(
            frames.len() >= 100,
            "test stream has only {} frames",
            frames.len()
        );
        std::fs::remove_dir_all(&dir).ok();

        let keyframe_requests = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let requests = keyframe_requests.clone();
        let camera = CameraH264 {
            frames: Arc::new(FrameHub::default()),
            request_keyframe: Arc::new(move || {
                requests.fetch_add(1, Ordering::Relaxed);
            }),
        };

        let publisher = StreamPublisher::new();
        let server = RtspServer::start(
            Ipv4Addr::LOCALHOST,
            18554,
            "camera",
            StreamTransport::Any,
            &publisher,
            camera.request_keyframe.clone(),
        )
        .unwrap();
        let passthrough = Passthrough::spawn(camera.clone(), publisher).unwrap();

        // Plays the file in a loop at 30 fps, as the capture thread would publish it
        let hub = camera.frames.clone();
        let running = Arc::new(AtomicBool::new(true));
        let feeder_running = running.clone();
        let feeder = std::thread::spawn(move || {
            let started = Instant::now();
            for (index, data) in frames.iter().cycle().enumerate() {
                if !feeder_running.load(Ordering::Relaxed) {
                    break;
                }
                let timestamp = Duration::from_nanos(index as u64 * 1_000_000_000 / 30);
                if let Some(wait) = timestamp.checked_sub(started.elapsed()) {
                    std::thread::sleep(wait);
                }
                hub.publish(Arc::new(EncodedFrame {
                    data: data.clone(),
                    timestamp_hns: (timestamp.as_nanos() / 100) as i64,
                }));
            }
        });

        // The SDP needs SPS/PPS, which only exist once the first frames went through
        std::thread::sleep(Duration::from_millis(200));
        let url = "rtsp://127.0.0.1:18554/camera";
        for transport in ["tcp", "udp"] {
            let (width, height, decoded) = probe(url, transport);
            println!("{transport}: {width}x{height}, {decoded} frames decoded in 2 s");
            assert_eq!((width, height), (640, 480));
            // Joins at the next IDR (up to 1 s GOP), then about 30 fps
            assert!(
                decoded >= 20,
                "only {decoded} frames decoded over {transport}"
            );
        }

        running.store(false, Ordering::Relaxed);
        feeder.join().unwrap();
        drop(passthrough);
        drop(server);
        assert!(keyframe_requests.load(Ordering::Relaxed) >= 2);
    }
}
