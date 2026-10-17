use std::{
    net::Ipv4Addr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use crate::{
    media::{EncoderConfig, NativeEncoder, VideoEncoder},
    rtsp::server::{RtspServer, StreamPublisher},
    CameraController, CameraError, StreamConfig,
};

// Camera -> H.264 hardware encoder -> RTP -> RTSP clients
pub(crate) struct Streaming {
    // Declared first so it is dropped first: clients stop before the encoder
    server: RtspServer,
    _encoder: NativeEncoder,
    subscription: u64,
}

#[uniffi::export]
impl CameraController {
    /// Starts the RTSP server and returns the stream URL (rtsp://<ip>:<port>/<path>)
    pub async fn start_streaming(&self, config: StreamConfig) -> Result<String, CameraError> {
        // Validated before touching the camera, so a typo doesn't start the capture
        let bind_address: Ipv4Addr = config.bind_address.trim().parse().map_err(|_| {
            CameraError::StreamError {
                msg: format!("Invalid IPv4 address: {}", config.bind_address),
            }
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

        let publisher = StreamPublisher::new();
        let force_keyframe = Arc::new(AtomicBool::new(false));
        let server_force_keyframe = force_keyframe.clone();
        let server = RtspServer::start(
            bind_address,
            config.port,
            path,
            config.transport,
            &publisher,
            Arc::new(move || server_force_keyframe.store(true, Ordering::Relaxed)),
        )?;

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
            Box::new(move |frame| publisher.publish(frame)),
        );

        let encoder = match encoder {
            Ok(encoder) => encoder,
            Err(e) => {
                self.hub.unsubscribe(subscription);
                return Err(e);
            }
        };

        let url = server.url.clone();
        *self.streaming.lock() = Some(Streaming {
            server,
            _encoder: encoder,
            subscription,
        });

        Ok(url)
    }

    /// Disconnects every client and stops the encoder. Camera keeps running for previews
    pub fn stop_streaming(&self) {
        let streaming = self.streaming.lock().take();
        if let Some(streaming) = streaming {
            self.hub.unsubscribe(streaming.subscription);
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
