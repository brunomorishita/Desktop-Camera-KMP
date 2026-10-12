use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

use parking_lot::Mutex;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream, UdpSocket},
    runtime::Runtime,
    sync::{broadcast, mpsc, watch},
    task::JoinHandle,
};

use super::{
    h264::{AccessUnit, AccessUnitBuilder, ParameterSets},
    rtp::{random_u32, RtpPacketizer, PAYLOAD_TYPE},
};
use crate::{win32::encoder::EncodedFrame, CameraError};

// Access units buffered per client before it is considered behind
const BROADCAST_CAPACITY: usize = 64;
// Outgoing bytes (responses + interleaved RTP) per TCP connection
const TCP_OUT_QUEUE: usize = 512;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const FIRST_RTP_PORT: u16 = 6970;

pub type KeyframeRequest = Arc<dyn Fn() + Send + Sync>;

// Producer side: encoder output goes in, every playing client gets it
pub struct StreamPublisher {
    frames: broadcast::Sender<Arc<AccessUnit>>,
    params: watch::Sender<Option<ParameterSets>>,
    builder: AccessUnitBuilder,
}

impl StreamPublisher {
    pub fn new() -> Self {
        let (frames, _) = broadcast::channel(BROADCAST_CAPACITY);
        let (params, _) = watch::channel(None);
        StreamPublisher {
            frames,
            params,
            builder: AccessUnitBuilder::default(),
        }
    }

    pub fn publish(&mut self, frame: EncodedFrame) {
        let access_unit = self.builder.build(&frame);

        let params = self.builder.parameter_sets();
        if params.is_some() && *self.params.borrow() != params {
            self.params.send_replace(params);
        }

        // Fails only when nobody is playing
        let _ = self.frames.send(Arc::new(access_unit));
    }

    fn source(&self) -> (broadcast::Sender<Arc<AccessUnit>>, watch::Receiver<Option<ParameterSets>>) {
        (self.frames.clone(), self.params.subscribe())
    }
}

pub struct RtspServer {
    runtime: Option<Runtime>,
    clients: Arc<AtomicU32>,
    pub url: String,
}

impl RtspServer {
    pub fn start(
        port: u16,
        path: &str,
        publisher: &StreamPublisher,
        request_keyframe: KeyframeRequest,
    ) -> Result<Self, CameraError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("rtsp-server")
            .enable_all()
            .build()
            .map_err(stream_error)?;

        // Bound synchronously so "port in use" reaches the caller
        let listener = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).map_err(stream_error)?;
        listener.set_nonblocking(true).map_err(stream_error)?;
        let (rtp_socket, rtcp_socket, server_ports) = bind_udp_pair()?;

        let clients = Arc::new(AtomicU32::new(0));
        let (frames, params) = publisher.source();
        let path = path.trim_matches('/').to_string();

        {
            let _guard = runtime.enter();
            let listener = TcpListener::from_std(listener).map_err(stream_error)?;
            let rtp_socket = Arc::new(UdpSocket::from_std(rtp_socket).map_err(stream_error)?);
            let rtcp_socket = UdpSocket::from_std(rtcp_socket).map_err(stream_error)?;

            let context = Arc::new(ServerContext {
                frames,
                params,
                request_keyframe,
                rtp_socket: rtp_socket.clone(),
                server_ports,
                clients: clients.clone(),
                path: path.clone(),
            });

            // Receiver reports are not used, just keep the sockets drained
            runtime.spawn(drain_udp(rtp_socket));
            runtime.spawn(drain_udp(Arc::new(rtcp_socket)));
            runtime.spawn(accept_loop(listener, context));
        }

        Ok(RtspServer {
            runtime: Some(runtime),
            clients,
            url: format!("rtsp://{}:{}/{}", local_ip(), port, path),
        })
    }

    pub fn client_count(&self) -> u32 {
        self.clients.load(Ordering::Relaxed)
    }
}

impl Drop for RtspServer {
    fn drop(&mut self) {
        // Cancels every connection task and closes all sockets without blocking
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

struct ServerContext {
    frames: broadcast::Sender<Arc<AccessUnit>>,
    params: watch::Receiver<Option<ParameterSets>>,
    request_keyframe: KeyframeRequest,
    rtp_socket: Arc<UdpSocket>,
    server_ports: (u16, u16),
    clients: Arc<AtomicU32>,
    path: String,
}

async fn accept_loop(listener: TcpListener, context: Arc<ServerContext>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let _ = stream.set_nodelay(true);
                tokio::spawn(handle_connection(stream, context.clone()));
            }
            Err(e) => println!("RTSP accept failed: {e}"),
        }
    }
}

async fn drain_udp(socket: Arc<UdpSocket>) {
    let mut buffer = [0u8; 2048];
    loop {
        // Windows reports ICMP "port unreachable" as recv errors, ignore them
        let _ = socket.recv_from(&mut buffer).await;
    }
}

async fn handle_connection(stream: TcpStream, context: Arc<ServerContext>) {
    let Ok(peer) = stream.peer_addr() else {
        return;
    };
    let local = stream
        .local_addr()
        .map(|a| a.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let (read_half, mut write_half) = stream.into_split();

    // Single writer, so RTSP responses and interleaved RTP never get mixed
    let (out, mut out_receiver) = mpsc::channel::<Vec<u8>>(TCP_OUT_QUEUE);
    let writer = tokio::spawn(async move {
        while let Some(bytes) = out_receiver.recv().await {
            if write_half.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });

    let mut session = Session::new(context, peer.ip(), local, out.clone());
    let mut reader = BufReader::new(read_half);

    while let Ok(Some(request)) = read_request(&mut reader).await {
        let reply = session.handle(&request).await;
        if out.send(reply.response).await.is_err() {
            break;
        }
        // Only after the PLAY response, so RTP never arrives before it
        if reply.start_playing {
            session.start_playing();
        }
        if reply.close {
            break;
        }
    }

    session.stop_playing();
    drop(session);
    drop(out);
    let _ = writer.await;
}

struct Request {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

async fn read_request<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> io::Result<Option<Request>> {
    // Clients may send RTCP interleaved on the same connection ($ + channel + length)
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return Ok(None);
        }
        if buffer[0] != b'$' {
            break;
        }
        let mut header = [0u8; 4];
        reader.read_exact(&mut header).await?;
        let length = u16::from_be_bytes([header[2], header[3]]) as usize;
        let mut payload = vec![0u8; length];
        reader.read_exact(&mut payload).await?;
    }

    let mut total = 0;
    let mut line = String::new();

    // Request line, skipping stray empty lines between requests
    let request_line = loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            return Ok(None);
        }
        total += read;
        if !line.trim().is_empty() {
            break line.trim().to_string();
        }
    };

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let url = parts.next().unwrap_or_default().to_string();

    let mut headers = Vec::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        total += read;
        if read == 0 || total > MAX_HEADER_BYTES {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }

    let request = Request {
        method,
        url,
        headers,
    };

    // Body (e.g. SET_PARAMETER) is not used
    if let Some(length) = request.header("Content-Length").and_then(|v| v.parse::<usize>().ok()) {
        let mut body = vec![0u8; length.min(MAX_HEADER_BYTES)];
        reader.read_exact(&mut body).await?;
    }

    Ok(Some(request))
}

enum Transport {
    Udp { rtp: SocketAddr },
    Tcp { channel: u8 },
}

struct Reply {
    response: Vec<u8>,
    start_playing: bool,
    close: bool,
}

struct Session {
    id: String,
    context: Arc<ServerContext>,
    peer: IpAddr,
    local: IpAddr,
    out: mpsc::Sender<Vec<u8>>,
    transport: Option<Arc<Transport>>,
    packetizer: Arc<Mutex<RtpPacketizer>>,
    playing: Option<JoinHandle<()>>,
}

impl Session {
    fn new(context: Arc<ServerContext>, peer: IpAddr, local: IpAddr, out: mpsc::Sender<Vec<u8>>) -> Self {
        Session {
            id: format!("{:08X}{:08X}", random_u32(), random_u32()),
            context,
            peer,
            local,
            out,
            transport: None,
            packetizer: Arc::new(Mutex::new(RtpPacketizer::new())),
            playing: None,
        }
    }

    async fn handle(&mut self, request: &Request) -> Reply {
        let cseq = request.header("CSeq").unwrap_or("0").to_string();
        let ok = |headers: Vec<String>, body: Option<(&str, String)>| Reply {
            response: response(200, "OK", &cseq, headers, body),
            start_playing: false,
            close: false,
        };
        let session_header = format!("Session: {};timeout=60", self.id);

        match request.method.as_str() {
            "OPTIONS" => ok(
                vec!["Public: OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN, GET_PARAMETER".into()],
                None,
            ),
            "DESCRIBE" => {
                if !self.matches_path(&request.url) {
                    return error_reply(404, "Not Found", &cseq);
                }
                let sdp = self.sdp().await;
                let base = if request.url.ends_with('/') {
                    request.url.clone()
                } else {
                    format!("{}/", request.url)
                };
                ok(
                    vec![format!("Content-Base: {base}")],
                    Some(("application/sdp", sdp)),
                )
            }
            "SETUP" => {
                let Some(transport_header) = request.header("Transport") else {
                    return error_reply(461, "Unsupported Transport", &cseq);
                };
                let ssrc = self.packetizer.lock().ssrc;

                let (transport, reply_transport) = if transport_header.contains("RTP/AVP/TCP") {
                    let (channel, _) =
                        parse_pair(transport_header, "interleaved=").unwrap_or((0, 1));
                    (
                        Transport::Tcp {
                            channel: channel as u8,
                        },
                        format!(
                            "RTP/AVP/TCP;unicast;interleaved={}-{};ssrc={ssrc:08X}",
                            channel,
                            channel + 1
                        ),
                    )
                } else if transport_header.contains("multicast") {
                    return error_reply(461, "Unsupported Transport", &cseq);
                } else {
                    let Some((rtp_port, rtcp_port)) = parse_pair(transport_header, "client_port=")
                    else {
                        return error_reply(461, "Unsupported Transport", &cseq);
                    };
                    let (server_rtp, server_rtcp) = self.context.server_ports;
                    (
                        Transport::Udp {
                            rtp: SocketAddr::new(self.peer, rtp_port),
                        },
                        format!(
                            "RTP/AVP;unicast;client_port={rtp_port}-{rtcp_port};server_port={server_rtp}-{server_rtcp};ssrc={ssrc:08X}"
                        ),
                    )
                };

                self.stop_playing();
                self.transport = Some(Arc::new(transport));
                ok(
                    vec![format!("Transport: {reply_transport}"), session_header],
                    None,
                )
            }
            "PLAY" => {
                if self.transport.is_none() {
                    return error_reply(455, "Method Not Valid in This State", &cseq);
                }
                let sequence = self.packetizer.lock().next_sequence();
                let track_url = format!("{}/trackID=0", request.url.trim_end_matches('/'));
                Reply {
                    response: response(
                        200,
                        "OK",
                        &cseq,
                        vec![
                            session_header,
                            "Range: npt=0.000-".into(),
                            format!("RTP-Info: url={track_url};seq={sequence}"),
                        ],
                        None,
                    ),
                    start_playing: true,
                    close: false,
                }
            }
            "PAUSE" => {
                self.stop_playing();
                ok(vec![session_header], None)
            }
            "TEARDOWN" => {
                self.stop_playing();
                Reply {
                    response: response(200, "OK", &cseq, vec![session_header], None),
                    start_playing: false,
                    close: true,
                }
            }
            // Keep-alive used by VLC and FFmpeg
            "GET_PARAMETER" | "SET_PARAMETER" => ok(vec![session_header], None),
            _ => error_reply(501, "Not Implemented", &cseq),
        }
    }

    fn matches_path(&self, url: &str) -> bool {
        let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
        let path = without_scheme.split_once('/').map_or("", |(_, p)| p);
        let path = path.split('?').next().unwrap_or_default().trim_matches('/');
        path == self.context.path
    }

    async fn sdp(&self) -> String {
        // Parameter sets come with the first IDR: ask for one and wait a bit
        (self.context.request_keyframe)();
        let mut params = self.context.params.clone();
        let params = tokio::time::timeout(Duration::from_secs(3), params.wait_for(Option::is_some))
            .await
            .ok()
            .and_then(|r| r.ok().and_then(|p| p.clone()));

        // Without sprop-parameter-sets clients still decode from the in-band SPS/PPS
        let fmtp = match params {
            Some(p) if p.sps.len() >= 4 => format!(
                "a=fmtp:{PAYLOAD_TYPE} packetization-mode=1;profile-level-id={:02X}{:02X}{:02X};sprop-parameter-sets={},{}\r\n",
                p.sps[1],
                p.sps[2],
                p.sps[3],
                base64(&p.sps),
                base64(&p.pps)
            ),
            _ => format!("a=fmtp:{PAYLOAD_TYPE} packetization-mode=1\r\n"),
        };
        let address_type = if self.local.is_ipv4() { "IP4" } else { "IP6" };

        format!(
            "v=0\r\n\
             o=- {id} 1 IN {address_type} {local}\r\n\
             s=Desktop Camera\r\n\
             c=IN {address_type} {local}\r\n\
             t=0 0\r\n\
             a=control:*\r\n\
             m=video 0 RTP/AVP {PAYLOAD_TYPE}\r\n\
             a=rtpmap:{PAYLOAD_TYPE} H264/90000\r\n\
             {fmtp}\
             a=control:trackID=0\r\n",
            id = random_u32(),
            local = self.local,
        )
    }

    fn start_playing(&mut self) {
        if self.playing.is_some() {
            return;
        }
        let Some(transport) = self.transport.clone() else {
            return;
        };

        let context = self.context.clone();
        let packetizer = self.packetizer.clone();
        let out = self.out.clone();
        let mut frames = context.frames.subscribe();

        self.playing = Some(tokio::spawn(async move {
            let _client = ClientGuard::new(context.clients.clone());
            // A new client can only start decoding from an IDR frame
            let mut waiting_keyframe = true;
            (context.request_keyframe)();

            loop {
                match frames.recv().await {
                    Ok(access_unit) => {
                        if waiting_keyframe {
                            if !access_unit.keyframe {
                                continue;
                            }
                            waiting_keyframe = false;
                        }

                        let packets = packetizer.lock().packetize(&access_unit);
                        for packet in packets {
                            if !send_packet(&transport, &context, &out, packet).await {
                                // Picture is now incomplete: resync on the next keyframe
                                waiting_keyframe = true;
                                (context.request_keyframe)();
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        waiting_keyframe = true;
                        (context.request_keyframe)();
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }));
    }

    fn stop_playing(&mut self) {
        if let Some(task) = self.playing.take() {
            task.abort();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop_playing();
    }
}

async fn send_packet(
    transport: &Transport,
    context: &ServerContext,
    out: &mpsc::Sender<Vec<u8>>,
    packet: Vec<u8>,
) -> bool {
    match transport {
        Transport::Udp { rtp } => {
            // UDP is fire-and-forget, losses are handled by the client
            let _ = context.rtp_socket.send_to(&packet, rtp).await;
            true
        }
        Transport::Tcp { channel } => {
            let mut framed = Vec::with_capacity(4 + packet.len());
            framed.push(b'$');
            framed.push(*channel);
            framed.extend_from_slice(&(packet.len() as u16).to_be_bytes());
            framed.extend_from_slice(&packet);
            // Slow client: drop instead of buffering latency
            out.try_send(framed).is_ok()
        }
    }
}

// Counts playing clients, decremented even when the task is aborted
struct ClientGuard(Arc<AtomicU32>);

impl ClientGuard {
    fn new(counter: Arc<AtomicU32>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        ClientGuard(counter)
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn response(
    code: u16,
    reason: &str,
    cseq: &str,
    headers: Vec<String>,
    body: Option<(&str, String)>,
) -> Vec<u8> {
    let mut text = format!("RTSP/1.0 {code} {reason}\r\nCSeq: {cseq}\r\nServer: DesktopCamera\r\n");
    for header in headers {
        text.push_str(&header);
        text.push_str("\r\n");
    }
    match body {
        Some((content_type, body)) => {
            text.push_str(&format!(
                "Content-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            ));
        }
        None => text.push_str("\r\n"),
    }
    text.into_bytes()
}

fn error_reply(code: u16, reason: &str, cseq: &str) -> Reply {
    Reply {
        response: response(code, reason, cseq, Vec::new(), None),
        start_playing: false,
        close: false,
    }
}

// "client_port=5000-5001" / "interleaved=0-1"
fn parse_pair(header: &str, key: &str) -> Option<(u16, u16)> {
    let value = header
        .split(';')
        .find_map(|part| part.trim().strip_prefix(key))?;
    match value.split_once('-') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => {
            let a: u16 = value.parse().ok()?;
            Some((a, a + 1))
        }
    }
}

fn bind_udp_pair() -> Result<(std::net::UdpSocket, std::net::UdpSocket, (u16, u16)), CameraError> {
    // RTP on an even port and RTCP on the next one
    for rtp_port in (FIRST_RTP_PORT..FIRST_RTP_PORT + 200).step_by(2) {
        let Ok(rtp) = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, rtp_port)) else {
            continue;
        };
        let Ok(rtcp) = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, rtp_port + 1)) else {
            continue;
        };
        rtp.set_nonblocking(true).map_err(stream_error)?;
        rtcp.set_nonblocking(true).map_err(stream_error)?;
        return Ok((rtp, rtcp, (rtp_port, rtp_port + 1)));
    }

    Err(CameraError::StreamError {
        msg: "No free UDP port pair for RTP".to_string(),
    })
}

// Address of the interface used to reach the network (no packet is actually sent)
fn local_ip() -> IpAddr {
    std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|socket| {
            socket.connect((Ipv4Addr::new(8, 8, 8, 8), 80))?;
            socket.local_addr()
        })
        .map(|addr| addr.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

fn stream_error(e: io::Error) -> CameraError {
    CameraError::StreamError { msg: e.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_base64() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
    }

    #[test]
    fn parses_transport_pairs() {
        let header = "RTP/AVP;unicast;client_port=5000-5001";
        assert_eq!(parse_pair(header, "client_port="), Some((5000, 5001)));
        assert_eq!(parse_pair("RTP/AVP/TCP;interleaved=2", "interleaved="), Some((2, 3)));
    }
}
