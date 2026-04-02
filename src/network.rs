use std::collections::VecDeque;
use std::fs;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const MAX_CHANNELS: usize = 16;
pub const STATE_SIZE: usize = 15;
pub const LEGACY_STATE_SIZE: usize = 14;

const PAYLOAD_TYPE_L24: u8 = 96;
const RTP_HEADER_SIZE: usize = 12;
const MAX_PACKET_SIZE: usize = 24_576;
const MAX_PACKET_SAMPLES: usize = 441 * MAX_CHANNELS;
const SDP_FILE_NAME: &str = "reastream2110-30.sdp";
const STARTUP_BUFFER_PACKETS: usize = 4;
const MAX_BUFFER_PACKETS: usize = 128;
const TARGET_CALLBACKS: usize = 2;
const TARGET_SAFETY_PACKETS: usize = 3;
const DRIFT_THRESHOLD_PACKETS: usize = 1;
const SEND_STARTUP_PACKETS: usize = 12;
const STALL_SILENCE_TIMEOUT: Duration = Duration::from_millis(20);
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_millis(250);
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(20);
const RTP_PACKET_INTERVAL: Duration = Duration::from_millis(1);
const SEND_QUEUE_PACKETS: usize = 256;
const RECEIVE_QUEUE_PACKETS: usize = 512;
const MAX_SENDER_BACKLOG_PACKETS: usize = 256;
const MAX_PACKETS_PER_CALLBACK: usize = 64;
const MAX_CONCEALMENT_PACKETS_PER_GAP: usize = 8;
const DSCP_EXPEDITED_FORWARDING: i32 = 46 << 2;
#[cfg(unix)]
const SOCKET_BUFFER_BYTES: i32 = 1 << 20;
#[cfg(any(target_os = "linux", target_os = "android"))]
const SOCKET_PRIORITY_AUDIO: i32 = 6;
#[cfg(target_os = "macos")]
const QOS_CLASS_USER_INITIATED: u32 = 0x19;
#[cfg(target_os = "macos")]
const PRIO_DARWIN_THREAD: libc::c_int = 3;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: libc::c_int)
    -> libc::c_int;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamMode {
    Send,
    Receive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamTransport {
    Unicast,
    Multicast,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)]
pub struct SenderStatus {
    pub active: bool,
    pub packets_sent: u64,
    pub packets_dropped: u64,
    pub queued_frames: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)]
pub struct ReceiverStatus {
    pub active: bool,
    pub primed: bool,
    pub queued_samples: usize,
    pub target_buffer_samples: usize,
    pub last_callback_frames: usize,
    pub packets_received: u64,
    pub packets_dropped: u64,
    pub packets_invalid: u64,
    pub packets_invalid_header: u64,
    pub packets_invalid_format: u64,
    pub packets_invalid_frame_mismatch: u64,
    pub last_invalid_samples: usize,
    pub packets_lost: u64,
    pub packets_out_of_order: u64,
    pub underruns: u64,
    pub drift_corrections: u64,
}

impl StreamMode {
    pub fn from_u32(value: u32) -> Self {
        match value {
            1 => Self::Receive,
            _ => Self::Send,
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Self::Send => 0,
            Self::Receive => 1,
        }
    }
}

impl StreamTransport {
    pub fn from_u32(value: u32) -> Self {
        match value {
            1 => Self::Multicast,
            _ => Self::Unicast,
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Self::Unicast => 0,
            Self::Multicast => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamParameters {
    pub enabled: bool,
    pub mode: StreamMode,
    pub transport: StreamTransport,
    pub channels: u8,
    pub port: u16,
    pub ip: [u8; 4],
}

impl StreamParameters {
    pub fn destination(self) -> SocketAddrV4 {
        SocketAddrV4::new(
            Ipv4Addr::new(self.ip[0], self.ip[1], self.ip[2], self.ip[3]),
            self.port,
        )
    }

    pub fn listen_addr(self) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, self.port)
    }

    fn sane_destination(self) -> bool {
        let ip = Ipv4Addr::new(self.ip[0], self.ip[1], self.ip[2], self.ip[3]);
        match self.transport {
            StreamTransport::Unicast => !ip.is_unspecified() && ip != Ipv4Addr::BROADCAST,
            StreamTransport::Multicast => ip.is_multicast(),
        }
    }

    fn sane_listener(self) -> bool {
        if self.port == 0 {
            return false;
        }

        if self.transport == StreamTransport::Multicast {
            Ipv4Addr::new(self.ip[0], self.ip[1], self.ip[2], self.ip[3]).is_multicast()
        } else {
            true
        }
    }

    fn accepts_source(self, source: SocketAddrV4) -> bool {
        if self.transport == StreamTransport::Multicast {
            return true;
        }
        let expected = Ipv4Addr::new(self.ip[0], self.ip[1], self.ip[2], self.ip[3]);
        expected.is_unspecified() || *source.ip() == expected
    }

    pub fn endpoint_label(self) -> &'static str {
        match (self.mode, self.transport) {
            (StreamMode::Send, StreamTransport::Unicast) => "destination",
            (StreamMode::Send, StreamTransport::Multicast) => "group",
            (StreamMode::Receive, StreamTransport::Unicast) => "expected_source",
            (StreamMode::Receive, StreamTransport::Multicast) => "group",
        }
    }

    fn group_addr(self) -> Option<Ipv4Addr> {
        let ip = Ipv4Addr::new(self.ip[0], self.ip[1], self.ip[2], self.ip[3]);
        ip.is_multicast().then_some(ip)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamLevel {
    A48k,
    Legacy44k1,
    AX96k,
}

impl StreamLevel {
    fn sample_rate_hz(self) -> u32 {
        match self {
            Self::A48k => 48_000,
            Self::Legacy44k1 => 44_100,
            Self::AX96k => 96_000,
        }
    }

    fn packet_frames(self) -> usize {
        match self {
            Self::A48k => 48,
            Self::Legacy44k1 => 441,
            Self::AX96k => 96,
        }
    }

    fn packet_interval(self) -> Duration {
        match self {
            Self::A48k | Self::AX96k => RTP_PACKET_INTERVAL,
            Self::Legacy44k1 => Duration::from_millis(10),
        }
    }

    fn ptime_ms(self) -> u32 {
        match self {
            Self::A48k | Self::AX96k => 1,
            Self::Legacy44k1 => 10,
        }
    }

    fn conformance_label(self) -> &'static str {
        match self {
            Self::A48k => "Level A",
            Self::Legacy44k1 => "Non-standard 44.1 kHz",
            Self::AX96k => "Level AX",
        }
    }
}

fn stream_level(sample_rate_hz: u32, channels: u8) -> Option<StreamLevel> {
    match (sample_rate_hz, channels) {
        (48_000, 1..=16) => Some(StreamLevel::A48k),
        (44_100, 1..=16) => Some(StreamLevel::Legacy44k1),
        (96_000, 1..=8) => Some(StreamLevel::AX96k),
        _ => None,
    }
}

fn target_buffer_samples(level: StreamLevel, channels: usize, callback_frames: usize) -> usize {
    let packet_frames = level.packet_frames();
    let packet_samples = packet_frames * channels;
    let callback_packets = callback_frames.div_ceil(packet_frames);
    let target_packets =
        STARTUP_BUFFER_PACKETS.max(callback_packets * TARGET_CALLBACKS + TARGET_SAFETY_PACKETS);
    target_packets * packet_samples
}

fn playout_adjustment_frames(
    queued_samples: usize,
    target_buffer_samples: usize,
    packet_samples: usize,
    available_frames: usize,
    requested_frames: usize,
) -> i32 {
    let threshold = packet_samples * DRIFT_THRESHOLD_PACKETS;
    if queued_samples + threshold < target_buffer_samples
        && available_frames + 1 >= requested_frames
    {
        -1
    } else if queued_samples > target_buffer_samples + threshold
        && available_frames > requested_frames
    {
        1
    } else {
        0
    }
}

fn bounded_wait_until(deadline: Instant) -> Duration {
    deadline
        .saturating_duration_since(Instant::now())
        .min(WORKER_POLL_INTERVAL)
}

#[cfg(unix)]
fn set_socket_option_int(socket: &UdpSocket, level: libc::c_int, name: libc::c_int, value: i32) {
    let fd = socket.as_raw_fd();
    let value = value as libc::c_int;
    let _ = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            (&value as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
}

fn configure_socket_priority(socket: &UdpSocket) {
    #[cfg(unix)]
    {
        set_socket_option_int(
            socket,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            SOCKET_BUFFER_BYTES,
        );
        set_socket_option_int(
            socket,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            SOCKET_BUFFER_BYTES,
        );

        set_socket_option_int(
            socket,
            libc::IPPROTO_IP,
            libc::IP_TOS,
            DSCP_EXPEDITED_FORWARDING,
        );

        #[cfg(any(target_os = "linux", target_os = "android"))]
        set_socket_option_int(
            socket,
            libc::SOL_SOCKET,
            libc::SO_PRIORITY,
            SOCKET_PRIORITY_AUDIO,
        );
    }
}

fn configure_thread_priority() {
    #[cfg(target_os = "macos")]
    unsafe {
        let _ = pthread_set_qos_class_self_np(QOS_CLASS_USER_INITIATED, 0);
        let _ = libc::setpriority(PRIO_DARWIN_THREAD, 0, -10);
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        let priority = libc::sched_get_priority_min(libc::SCHED_RR);
        if priority >= 0 {
            let param = libc::sched_param {
                sched_priority: priority,
            };
            let _ = libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_RR, &param);
        }
    }
}

pub(crate) fn warning_text(sample_rate_hz: u32, channels: u8) -> String {
    let mut warnings = Vec::new();
    if sample_rate_hz == 44_100 {
        warnings.push("44.1 kHz is supported but not ST 2110-30 compliant");
    }
    if sample_rate_hz == 48_000 && channels > 8 {
        warnings.push("More than 8 channels at 48 kHz is outside ST 2110-30 Level A");
    }
    if sample_rate_hz == 96_000 && channels > 4 {
        warnings.push("More than 4 channels at 96 kHz is outside ST 2110-30 Level AX");
    }
    warnings.join(" | ")
}

fn configure_multicast_sender(socket: &UdpSocket, destination: Ipv4Addr) {
    if !destination.is_multicast() {
        return;
    }

    let _ = socket.set_multicast_loop_v4(true);
    let _ = socket.set_multicast_ttl_v4(8);
}

fn configure_multicast_receiver(socket: &UdpSocket, group: Ipv4Addr) {
    let _ = socket.join_multicast_v4(&group, &Ipv4Addr::UNSPECIFIED);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveStream {
    params: StreamParameters,
    level: StreamLevel,
}

struct SenderWorker {
    tx: SyncSender<SentPacket>,
    active: std::sync::Arc<AtomicBool>,
    packets_sent: std::sync::Arc<AtomicU64>,
    packets_dropped: std::sync::Arc<AtomicU64>,
    shutdown: std::sync::Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl SenderWorker {
    fn try_send(&self, packet: SentPacket) {
        match self.tx.try_send(packet) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.packets_dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.active.store(false, Ordering::Relaxed);
                self.packets_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for SenderWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

struct SentPacket {
    len: usize,
    data: [u8; MAX_PACKET_SIZE],
}

impl SentPacket {
    fn new() -> Self {
        Self {
            len: RTP_HEADER_SIZE,
            data: [0; MAX_PACKET_SIZE],
        }
    }

    fn clear_payload(&mut self) {
        self.len = RTP_HEADER_SIZE;
    }
}

struct ReceiverPacket {
    sequence: u16,
    sample_count: usize,
    samples: [f32; MAX_PACKET_SAMPLES],
}

struct DecodedSamples {
    sample_count: usize,
    samples: [f32; MAX_PACKET_SAMPLES],
}

struct ReceiverWorker {
    rx: Receiver<ReceiverPacket>,
    active: std::sync::Arc<AtomicBool>,
    packets_dropped: std::sync::Arc<AtomicU64>,
    packets_invalid: std::sync::Arc<AtomicU64>,
    packets_invalid_header: std::sync::Arc<AtomicU64>,
    packets_invalid_format: std::sync::Arc<AtomicU64>,
    packets_invalid_frame_mismatch: std::sync::Arc<AtomicU64>,
    last_invalid_samples: std::sync::Arc<AtomicU64>,
    shutdown: std::sync::Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl ReceiverWorker {
    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for ReceiverWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn_sender_worker(stream: ActiveStream, sdp_path: PathBuf) -> SenderWorker {
    let (tx, rx) = sync_channel::<SentPacket>(SEND_QUEUE_PACKETS);
    let active = std::sync::Arc::new(AtomicBool::new(false));
    let packets_sent = std::sync::Arc::new(AtomicU64::new(0));
    let packets_dropped = std::sync::Arc::new(AtomicU64::new(0));
    let shutdown = std::sync::Arc::new(AtomicBool::new(false));

    let thread_active = active.clone();
    let thread_packets_sent = packets_sent.clone();
    let thread_packets_dropped = packets_dropped.clone();
    let thread_shutdown = shutdown.clone();

    let join = thread::spawn(move || {
        configure_thread_priority();
        let destination = stream.params.destination();
        let Ok(socket) = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)) else {
            return;
        };
        configure_socket_priority(&socket);
        if stream.params.transport == StreamTransport::Multicast {
            configure_multicast_sender(&socket, *destination.ip());
        }
        if socket.connect(destination).is_err() {
            return;
        }

        let local_ip = match socket.local_addr() {
            Ok(SocketAddr::V4(addr)) => *addr.ip(),
            _ => Ipv4Addr::UNSPECIFIED,
        };
        write_sdp_file(
            &sdp_path,
            local_ip,
            destination,
            stream.level,
            stream.params.channels,
        );
        thread_active.store(true, Ordering::Relaxed);
        let mut backlog = VecDeque::<SentPacket>::with_capacity(MAX_SENDER_BACKLOG_PACKETS);
        let mut primed = false;
        let mut next_send_at: Option<Instant> = None;

        loop {
            if thread_shutdown.load(Ordering::Relaxed) {
                break;
            }

            while let Ok(packet) = rx.try_recv() {
                if backlog.len() >= MAX_SENDER_BACKLOG_PACKETS {
                    backlog.pop_front();
                    thread_packets_dropped.fetch_add(1, Ordering::Relaxed);
                }
                backlog.push_back(packet);
            }

            if !primed {
                if backlog.len() >= SEND_STARTUP_PACKETS {
                    primed = true;
                    next_send_at = Some(
                        Instant::now()
                            + stream.level.packet_interval() * SEND_STARTUP_PACKETS as u32,
                    );
                } else {
                    match rx.recv_timeout(WORKER_POLL_INTERVAL) {
                        Ok(packet) => {
                            backlog.push_back(packet);
                            continue;
                        }
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            }

            if backlog.is_empty() {
                primed = false;
                next_send_at = None;
                continue;
            }

            if let Some(packet) = backlog.front() {
                let send_now = next_send_at.is_none_or(|deadline| Instant::now() >= deadline);
                if send_now {
                    if socket.send(&packet.data[..packet.len]).is_ok() {
                        thread_packets_sent.fetch_add(1, Ordering::Relaxed);
                    }
                    backlog.pop_front();
                    let scheduled =
                        next_send_at.unwrap_or_else(Instant::now) + stream.level.packet_interval();
                    next_send_at = Some(scheduled);
                    continue;
                }
            }

            let wait = next_send_at
                .map(bounded_wait_until)
                .unwrap_or(WORKER_POLL_INTERVAL);
            match rx.recv_timeout(wait) {
                Ok(packet) => {
                    backlog.push_back(packet);
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    while let Some(packet) = backlog.pop_front() {
                        if socket.send(&packet.data[..packet.len]).is_ok() {
                            thread_packets_sent.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    break;
                }
            }
        }

        thread_active.store(false, Ordering::Relaxed);
    });

    SenderWorker {
        tx,
        active,
        packets_sent,
        packets_dropped,
        shutdown,
        join: Some(join),
    }
}

fn spawn_receiver_worker(stream: ActiveStream) -> ReceiverWorker {
    let (tx, rx) = sync_channel::<ReceiverPacket>(RECEIVE_QUEUE_PACKETS);
    let active = std::sync::Arc::new(AtomicBool::new(false));
    let packets_dropped = std::sync::Arc::new(AtomicU64::new(0));
    let packets_invalid = std::sync::Arc::new(AtomicU64::new(0));
    let packets_invalid_header = std::sync::Arc::new(AtomicU64::new(0));
    let packets_invalid_format = std::sync::Arc::new(AtomicU64::new(0));
    let packets_invalid_frame_mismatch = std::sync::Arc::new(AtomicU64::new(0));
    let last_invalid_samples = std::sync::Arc::new(AtomicU64::new(0));
    let shutdown = std::sync::Arc::new(AtomicBool::new(false));

    let thread_active = active.clone();
    let thread_packets_dropped = packets_dropped.clone();
    let thread_packets_invalid = packets_invalid.clone();
    let thread_packets_invalid_header = packets_invalid_header.clone();
    let thread_packets_invalid_format = packets_invalid_format.clone();
    let thread_packets_invalid_frame_mismatch = packets_invalid_frame_mismatch.clone();
    let thread_last_invalid_samples = last_invalid_samples.clone();
    let thread_shutdown = shutdown.clone();

    let join = thread::spawn(move || {
        configure_thread_priority();
        let Ok(socket) = UdpSocket::bind(stream.params.listen_addr()) else {
            return;
        };
        configure_socket_priority(&socket);
        if let Some(group) = stream.params.group_addr() {
            configure_multicast_receiver(&socket, group);
        }
        let _ = socket.set_read_timeout(Some(WORKER_POLL_INTERVAL));
        let mut packet_buffer = [0_u8; MAX_PACKET_SIZE];
        thread_active.store(true, Ordering::Relaxed);

        loop {
            if thread_shutdown.load(Ordering::Relaxed) {
                break;
            }

            match socket.recv_from(&mut packet_buffer) {
                Ok((size, SocketAddr::V4(source))) => {
                    if !stream.params.accepts_source(source) {
                        continue;
                    }

                    let packet = &packet_buffer[..size];
                    let Some(sequence) = parse_rtp_sequence(packet) else {
                        thread_packets_invalid.fetch_add(1, Ordering::Relaxed);
                        thread_packets_invalid_header.fetch_add(1, Ordering::Relaxed);
                        continue;
                    };
                    let Some(decoded) = decode_rtp_l24_packet_fixed(
                        packet,
                        stream.params.channels,
                        stream.level.packet_frames(),
                    ) else {
                        thread_packets_invalid.fetch_add(1, Ordering::Relaxed);
                        match classify_invalid_packet(
                            packet,
                            stream.params.channels,
                            stream.level.packet_frames(),
                        ) {
                            InvalidPacketKind::Header => {
                                thread_packets_invalid_header.fetch_add(1, Ordering::Relaxed);
                            }
                            InvalidPacketKind::Format => {
                                thread_packets_invalid_format.fetch_add(1, Ordering::Relaxed);
                            }
                            InvalidPacketKind::FrameMismatch { actual_samples } => {
                                thread_packets_invalid_frame_mismatch
                                    .fetch_add(1, Ordering::Relaxed);
                                thread_last_invalid_samples
                                    .store(actual_samples as u64, Ordering::Relaxed);
                            }
                        }
                        continue;
                    };

                    match tx.try_send(ReceiverPacket {
                        sequence,
                        sample_count: decoded.sample_count,
                        samples: decoded.samples,
                    }) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => {
                            thread_packets_dropped.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(TrySendError::Disconnected(_)) => break,
                    }
                }
                Ok((_size, _source)) => continue,
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(_) => break,
            }
        }

        thread_active.store(false, Ordering::Relaxed);
    });

    ReceiverWorker {
        rx,
        active,
        packets_dropped,
        packets_invalid,
        packets_invalid_header,
        packets_invalid_format,
        packets_invalid_frame_mismatch,
        last_invalid_samples,
        shutdown,
        join: Some(join),
    }
}

struct SenderState {
    stream: Option<ActiveStream>,
    worker: Option<SenderWorker>,
    pending_packet: SentPacket,
    pending_frames: usize,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    sdp_path: PathBuf,
}

impl SenderState {
    fn new() -> Self {
        Self {
            stream: None,
            worker: None,
            pending_packet: SentPacket::new(),
            pending_frames: 0,
            sequence: 0,
            timestamp: 0,
            ssrc: seed_ssrc(),
            sdp_path: std::env::temp_dir().join(SDP_FILE_NAME),
        }
    }

    fn reset(&mut self) {
        self.stream = None;
        if let Some(mut worker) = self.worker.take() {
            worker.stop();
        }
        self.pending_packet.clear_payload();
        self.pending_frames = 0;
        self.sequence = 0;
        self.timestamp = 0;
        self.ssrc = seed_ssrc();
    }

    fn ensure_stream(&mut self, params: StreamParameters, level: StreamLevel) {
        let candidate = ActiveStream { params, level };
        if self.stream == Some(candidate) && self.worker.is_some() {
            return;
        }

        self.reset();

        self.worker = Some(spawn_sender_worker(candidate, self.sdp_path.clone()));
        self.stream = Some(candidate);
    }

    fn push_block(
        &mut self,
        params: StreamParameters,
        level: StreamLevel,
        input_channels: &[Option<&[f32]>; MAX_CHANNELS],
        frames: usize,
    ) {
        self.ensure_stream(params, level);
        if self.worker.is_none() {
            return;
        }

        for frame_index in 0..frames {
            for input_channel in input_channels.iter().take(params.channels as usize) {
                let sample = input_channel
                    .map(|channel| channel[frame_index])
                    .unwrap_or(0.0);
                write_l24_sample(
                    &mut self.pending_packet.data,
                    &mut self.pending_packet.len,
                    sample,
                );
            }

            self.pending_frames += 1;
            if self.pending_frames == level.packet_frames() {
                self.flush_packet(level.packet_frames() as u32);
            }
        }
    }

    fn flush_packet(&mut self, packet_frames: u32) {
        let Some(worker) = &self.worker else {
            return;
        };

        let mut packet = SentPacket::new();
        packet.len = self.pending_packet.len;
        packet.data[0] = 0x80;
        packet.data[1] = PAYLOAD_TYPE_L24 & 0x7f;
        packet.data[2..4].copy_from_slice(&self.sequence.to_be_bytes());
        packet.data[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        packet.data[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
        packet.data[RTP_HEADER_SIZE..packet.len]
            .copy_from_slice(&self.pending_packet.data[RTP_HEADER_SIZE..self.pending_packet.len]);

        worker.try_send(packet);

        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(packet_frames);
        self.pending_frames = 0;
        self.pending_packet.clear_payload();
    }

    #[allow(dead_code)]
    fn status(&self) -> SenderStatus {
        let (active, packets_sent, packets_dropped) = self
            .worker
            .as_ref()
            .map(|worker| {
                (
                    worker.active.load(Ordering::Relaxed),
                    worker.packets_sent.load(Ordering::Relaxed),
                    worker.packets_dropped.load(Ordering::Relaxed),
                )
            })
            .unwrap_or((false, 0, 0));

        SenderStatus {
            active,
            packets_sent,
            packets_dropped,
            queued_frames: self.pending_frames,
        }
    }
}

pub struct NetworkSender {
    state: Mutex<SenderState>,
}

impl NetworkSender {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(SenderState::new()),
        }
    }

    pub fn reset(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.reset();
        }
    }

    pub fn push_audio(
        &self,
        params: StreamParameters,
        sample_rate_hz: u32,
        input_channels: &[Option<&[f32]>; MAX_CHANNELS],
        frames: usize,
    ) {
        if !params.enabled || !params.sane_destination() {
            return;
        }

        let Some(level) = stream_level(sample_rate_hz, params.channels) else {
            return;
        };

        if let Ok(mut state) = self.state.lock() {
            state.push_block(params, level, input_channels, frames);
        }
    }

    #[allow(dead_code)]
    pub fn status_snapshot(&self) -> SenderStatus {
        self.state
            .lock()
            .map(|state| state.status())
            .unwrap_or_default()
    }
}

struct ReceiverState {
    stream: Option<ActiveStream>,
    worker: Option<ReceiverWorker>,
    packet_queue: VecDeque<ReceiverPacket>,
    front_packet_offset: usize,
    queued_samples: usize,
    last_frame: [f32; MAX_CHANNELS],
    primed: bool,
    target_buffer_samples: usize,
    last_callback_frames: usize,
    last_sequence: Option<u16>,
    last_packet_at: Option<Instant>,
    packets_received: u64,
    packets_lost: u64,
    packets_out_of_order: u64,
    underruns: u64,
    drift_corrections: u64,
}

impl ReceiverState {
    fn new() -> Self {
        Self {
            stream: None,
            worker: None,
            packet_queue: VecDeque::with_capacity(MAX_BUFFER_PACKETS),
            front_packet_offset: 0,
            queued_samples: 0,
            last_frame: [0.0; MAX_CHANNELS],
            primed: false,
            target_buffer_samples: 0,
            last_callback_frames: 0,
            last_sequence: None,
            last_packet_at: None,
            packets_received: 0,
            packets_lost: 0,
            packets_out_of_order: 0,
            underruns: 0,
            drift_corrections: 0,
        }
    }

    fn reset(&mut self) {
        self.stream = None;
        if let Some(mut worker) = self.worker.take() {
            worker.stop();
        }
        self.packet_queue.clear();
        self.front_packet_offset = 0;
        self.queued_samples = 0;
        self.last_frame.fill(0.0);
        self.primed = false;
        self.target_buffer_samples = 0;
        self.last_callback_frames = 0;
        self.last_sequence = None;
        self.last_packet_at = None;
        self.packets_received = 0;
        self.packets_lost = 0;
        self.packets_out_of_order = 0;
        self.underruns = 0;
        self.drift_corrections = 0;
    }

    fn ensure_stream(&mut self, params: StreamParameters, level: StreamLevel) {
        let candidate = ActiveStream { params, level };
        if self.stream == Some(candidate) && self.worker.is_some() {
            return;
        }

        self.reset();
        self.worker = Some(spawn_receiver_worker(candidate));
        self.stream = Some(candidate);
    }

    fn receive_into_queue(&mut self, params: StreamParameters, level: StreamLevel) {
        for _ in 0..MAX_PACKETS_PER_CALLBACK {
            let Some(worker) = &self.worker else {
                return;
            };

            match worker.rx.try_recv() {
                Ok(packet) => {
                    let Some(missing_packets) = self.accept_sequence(Some(packet.sequence)) else {
                        continue;
                    };
                    self.last_packet_at = Some(Instant::now());
                    self.packets_received = self.packets_received.saturating_add(1);
                    if missing_packets > 0 {
                        self.enqueue_concealment_packets(missing_packets, level, params.channels);
                    }
                    self.enqueue_packet(packet);
                    self.trim_queue(level, params.channels);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if let Some(mut worker) = self.worker.take() {
                        worker.stop();
                    }
                    self.stream = None;
                    self.packet_queue.clear();
                    self.front_packet_offset = 0;
                    self.queued_samples = 0;
                    self.last_frame.fill(0.0);
                    self.primed = false;
                    self.last_sequence = None;
                    self.last_packet_at = None;
                    break;
                }
            }
        }
    }

    fn accept_sequence(&mut self, sequence: Option<u16>) -> Option<usize> {
        let sequence = sequence?;

        let mut missing_packets = 0;
        if let Some(previous) = self.last_sequence {
            let expected = previous.wrapping_add(1);
            if sequence != expected {
                let forward_distance = sequence.wrapping_sub(expected);
                if forward_distance < 0x8000 {
                    self.packets_lost = self.packets_lost.saturating_add(forward_distance as u64);
                    missing_packets = forward_distance as usize;
                } else {
                    self.packets_out_of_order = self.packets_out_of_order.saturating_add(1);
                    return None;
                }
            }
        }

        self.last_sequence = Some(sequence);
        Some(missing_packets.min(MAX_CONCEALMENT_PACKETS_PER_GAP))
    }

    fn enqueue_packet(&mut self, packet: ReceiverPacket) {
        self.queued_samples = self.queued_samples.saturating_add(packet.sample_count);
        self.packet_queue.push_back(packet);
    }

    fn enqueue_concealment_packets(
        &mut self,
        missing_packets: usize,
        level: StreamLevel,
        channels: u8,
    ) {
        let packet_samples = level.packet_frames() * channels as usize;
        for _ in 0..missing_packets {
            let mut packet = ReceiverPacket {
                sequence: 0,
                sample_count: packet_samples,
                samples: [0.0; MAX_PACKET_SAMPLES],
            };
            for frame_index in 0..level.packet_frames() {
                for (channel_index, last_frame) in
                    self.last_frame.iter().take(channels as usize).enumerate()
                {
                    packet.samples[frame_index * channels as usize + channel_index] = *last_frame;
                }
            }
            self.enqueue_packet(packet);
        }
    }

    fn handle_idle_timeout(&mut self) {
        let Some(last_packet_at) = self.last_packet_at else {
            return;
        };

        if last_packet_at.elapsed() < STREAM_IDLE_TIMEOUT {
            return;
        }

        self.packet_queue.clear();
        self.front_packet_offset = 0;
        self.queued_samples = 0;
        self.last_frame.fill(0.0);
        self.primed = false;
        self.last_sequence = None;
        self.last_packet_at = None;
    }

    fn is_stalled(&self) -> bool {
        self.last_packet_at
            .is_some_and(|last_packet_at| last_packet_at.elapsed() >= STALL_SILENCE_TIMEOUT)
    }

    fn output_constant(
        &mut self,
        output_channels: &mut [Option<&mut [f32]>; MAX_CHANNELS],
        channels: usize,
        start_frame: usize,
        end_frame: usize,
        value: f32,
    ) {
        for frame_index in start_frame..end_frame {
            for output_channel in output_channels.iter_mut().take(channels) {
                if let Some(channel) = output_channel.as_deref_mut() {
                    channel[frame_index] = value;
                }
            }
        }
    }

    fn clear_stalled_stream(
        &mut self,
        output_channels: &mut [Option<&mut [f32]>; MAX_CHANNELS],
        channels: usize,
        frames: usize,
    ) {
        self.packet_queue.clear();
        self.front_packet_offset = 0;
        self.queued_samples = 0;
        self.last_frame.fill(0.0);
        self.primed = false;
        self.output_constant(output_channels, channels, 0, frames, 0.0);
    }

    fn trim_queue(&mut self, level: StreamLevel, channels: u8) {
        let max_samples = level.packet_frames() * channels as usize * MAX_BUFFER_PACKETS;
        let mut overflow = self.queued_samples.saturating_sub(max_samples);
        while overflow > 0 {
            let Some(front) = self.packet_queue.front_mut() else {
                self.front_packet_offset = 0;
                self.queued_samples = 0;
                break;
            };
            let available = front.sample_count.saturating_sub(self.front_packet_offset);
            let to_drop = overflow.min(available);
            self.front_packet_offset += to_drop;
            self.queued_samples = self.queued_samples.saturating_sub(to_drop);
            overflow -= to_drop;

            if self.front_packet_offset >= front.sample_count {
                self.packet_queue.pop_front();
                self.front_packet_offset = 0;
            }
        }
    }

    fn pull_block(
        &mut self,
        params: StreamParameters,
        level: StreamLevel,
        output_channels: &mut [Option<&mut [f32]>; MAX_CHANNELS],
        frames: usize,
    ) {
        self.ensure_stream(params, level);
        self.handle_idle_timeout();
        self.receive_into_queue(params, level);

        let channels = params.channels as usize;
        if self.is_stalled() {
            self.clear_stalled_stream(output_channels, channels, frames);
            return;
        }

        self.last_callback_frames = frames;
        self.target_buffer_samples = target_buffer_samples(level, channels, frames);
        if !self.primed {
            if self.queued_samples < self.target_buffer_samples {
                return;
            }
            self.primed = true;
        }

        let available_frames = self.queued_samples / channels;
        if available_frames == 0 {
            self.underruns = self.underruns.saturating_add(1);
            self.output_constant(output_channels, channels, 0, frames, 0.0);
            return;
        }

        let packet_samples = level.packet_frames() * channels;
        let adjust = playout_adjustment_frames(
            self.queued_samples,
            self.target_buffer_samples,
            packet_samples,
            available_frames,
            frames,
        );
        if adjust != 0 {
            self.drift_corrections = self.drift_corrections.saturating_add(1);
        }

        let duplicate_frame_at = (adjust < 0 && frames > 1).then_some(frames / 2);
        let skip_source_frame_at = (adjust > 0).then_some(frames / 2);
        let mut produced_frames = 0;

        for frame_index in 0..frames {
            if duplicate_frame_at == Some(frame_index) {
                for (channel_index, output_channel) in
                    output_channels.iter_mut().enumerate().take(channels)
                {
                    if let Some(channel) = output_channel.as_deref_mut() {
                        channel[frame_index] = self.last_frame[channel_index];
                    }
                }
                produced_frames += 1;
                continue;
            }

            if skip_source_frame_at == Some(frame_index) {
                for channel_index in 0..channels {
                    let _ = self.pop_sample().unwrap_or(self.last_frame[channel_index]);
                }
            }

            let mut had_source = true;
            for (channel_index, output_channel) in
                output_channels.iter_mut().enumerate().take(channels)
            {
                let Some(sample) = self.pop_sample() else {
                    had_source = false;
                    break;
                };
                self.last_frame[channel_index] = sample;
                if let Some(channel) = output_channel.as_deref_mut() {
                    channel[frame_index] = sample;
                }
            }

            if !had_source {
                break;
            }

            produced_frames += 1;
        }

        if produced_frames < frames {
            self.underruns = self.underruns.saturating_add(1);
            self.output_constant(output_channels, channels, produced_frames, frames, 0.0);
        }
    }

    #[allow(dead_code)]
    fn status(&self) -> ReceiverStatus {
        let (
            active,
            packets_dropped,
            packets_invalid,
            packets_invalid_header,
            packets_invalid_format,
            packets_invalid_frame_mismatch,
            last_invalid_samples,
        ) = self
            .worker
            .as_ref()
            .map(|worker| {
                (
                    worker.active.load(Ordering::Relaxed),
                    worker.packets_dropped.load(Ordering::Relaxed),
                    worker.packets_invalid.load(Ordering::Relaxed),
                    worker.packets_invalid_header.load(Ordering::Relaxed),
                    worker.packets_invalid_format.load(Ordering::Relaxed),
                    worker
                        .packets_invalid_frame_mismatch
                        .load(Ordering::Relaxed),
                    worker.last_invalid_samples.load(Ordering::Relaxed) as usize,
                )
            })
            .unwrap_or((false, 0, 0, 0, 0, 0, 0));

        ReceiverStatus {
            active,
            primed: self.primed,
            queued_samples: self.queued_samples,
            target_buffer_samples: self.target_buffer_samples,
            last_callback_frames: self.last_callback_frames,
            packets_received: self.packets_received,
            packets_dropped,
            packets_invalid,
            packets_invalid_header,
            packets_invalid_format,
            packets_invalid_frame_mismatch,
            last_invalid_samples,
            packets_lost: self.packets_lost,
            packets_out_of_order: self.packets_out_of_order,
            underruns: self.underruns,
            drift_corrections: self.drift_corrections,
        }
    }

    fn pop_sample(&mut self) -> Option<f32> {
        loop {
            let front = self.packet_queue.front_mut()?;
            if self.front_packet_offset < front.sample_count {
                let sample = front.samples[self.front_packet_offset];
                self.front_packet_offset += 1;
                self.queued_samples = self.queued_samples.saturating_sub(1);

                if self.front_packet_offset >= front.sample_count {
                    self.packet_queue.pop_front();
                    self.front_packet_offset = 0;
                }

                return Some(sample);
            }

            self.packet_queue.pop_front();
            self.front_packet_offset = 0;
        }
    }
}

pub struct NetworkReceiver {
    state: Mutex<ReceiverState>,
}

impl NetworkReceiver {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(ReceiverState::new()),
        }
    }

    pub fn reset(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.reset();
        }
    }

    pub fn pull_audio(
        &self,
        params: StreamParameters,
        sample_rate_hz: u32,
        output_channels: &mut [Option<&mut [f32]>; MAX_CHANNELS],
        frames: usize,
    ) {
        for channel in output_channels.iter_mut() {
            if let Some(buffer) = channel.as_deref_mut() {
                buffer.fill(0.0);
            }
        }

        if !params.enabled || !params.sane_listener() {
            return;
        }

        let Some(level) = stream_level(sample_rate_hz, params.channels) else {
            return;
        };

        if let Ok(mut state) = self.state.lock() {
            state.pull_block(params, level, output_channels, frames);
        }
    }

    #[allow(dead_code)]
    pub fn status_snapshot(&self) -> ReceiverStatus {
        self.state
            .lock()
            .map(|state| state.status())
            .unwrap_or_default()
    }
}

fn seed_ssrc() -> u32 {
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    seed.subsec_nanos() ^ (seed.as_secs() as u32).rotate_left(13)
}

fn write_sdp_file(
    path: &PathBuf,
    local_ip: Ipv4Addr,
    destination: SocketAddrV4,
    level: StreamLevel,
    channels: u8,
) {
    let content = format!(
        "v=0\r\n\
o=- 0 0 IN IP4 {local_ip}\r\n\
s=ReaStream 2110-30 Sender\r\n\
c=IN IP4 {destination_ip}\r\n\
t=0 0\r\n\
m=audio {destination_port} RTP/AVP {payload_type}\r\n\
a=rtpmap:{payload_type} L24/{sample_rate}/{channels}\r\n\
a=fmtp:{payload_type} channel-order=SMPTE2110.({channel_order})\r\n\
a=ptime:{ptime}\r\n\
a=maxptime:{ptime}\r\n\
a=sendonly\r\n\
a=ts-refclk:local\r\n\
a=mediaclk:direct=0\r\n\
a=x-conformance:{conformance}\r\n",
        destination_ip = destination.ip(),
        destination_port = destination.port(),
        payload_type = PAYLOAD_TYPE_L24,
        sample_rate = level.sample_rate_hz(),
        channels = channels,
        channel_order = channel_order(channels),
        ptime = level.ptime_ms(),
        conformance = level.conformance_label(),
    );

    let _ = fs::write(path, content);
}

fn channel_order(channels: u8) -> String {
    format!("U{channels:02}")
}

fn write_l24_sample(dst: &mut [u8; MAX_PACKET_SIZE], len: &mut usize, sample: f32) {
    let quantized = if sample >= 1.0 {
        8_388_607
    } else if sample <= -1.0 {
        -8_388_608
    } else {
        (sample as f64 * 8_388_608.0).round() as i32
    };
    let bytes = quantized.to_be_bytes();
    dst[*len..*len + 3].copy_from_slice(&bytes[1..4]);
    *len += 3;
}

#[cfg(test)]
fn decode_rtp_l24_packet(packet: &[u8], channels: u8) -> Option<Vec<f32>> {
    if packet.len() < RTP_HEADER_SIZE || channels == 0 {
        return None;
    }

    let version = packet[0] >> 6;
    if version != 2 {
        return None;
    }

    let has_padding = packet[0] & 0x20 != 0;
    let csrc_count = (packet[0] & 0x0f) as usize;
    let payload_type = packet[1] & 0x7f;
    if payload_type != PAYLOAD_TYPE_L24 {
        return None;
    }

    let header_len = RTP_HEADER_SIZE + (csrc_count * 4);
    if packet.len() < header_len {
        return None;
    }

    let payload_end = if has_padding {
        let pad_bytes = *packet.last()? as usize;
        packet.len().checked_sub(pad_bytes)?
    } else {
        packet.len()
    };

    if payload_end < header_len {
        return None;
    }

    let payload = &packet[header_len..payload_end];
    let bytes_per_frame = channels as usize * 3;
    if bytes_per_frame == 0 || !payload.len().is_multiple_of(bytes_per_frame) {
        return None;
    }

    let mut samples = Vec::with_capacity(payload.len() / 3);
    for chunk in payload.chunks_exact(3) {
        samples.push(read_l24_sample(chunk));
    }
    Some(samples)
}

#[cfg(test)]
fn decode_rtp_l24_packet_strict(
    packet: &[u8],
    channels: u8,
    expected_frames: usize,
) -> Option<Vec<f32>> {
    let samples = decode_rtp_l24_packet(packet, channels)?;
    if samples.len() != expected_frames * channels as usize {
        return None;
    }
    Some(samples)
}

fn decode_rtp_l24_packet_fixed(
    packet: &[u8],
    channels: u8,
    expected_frames: usize,
) -> Option<DecodedSamples> {
    let expected_samples = expected_frames * channels as usize;
    if packet.len() < RTP_HEADER_SIZE || channels == 0 || expected_samples > MAX_PACKET_SAMPLES {
        return None;
    }

    let version = packet[0] >> 6;
    if version != 2 {
        return None;
    }

    let has_padding = packet[0] & 0x20 != 0;
    let csrc_count = (packet[0] & 0x0f) as usize;
    let payload_type = packet[1] & 0x7f;
    if payload_type != PAYLOAD_TYPE_L24 {
        return None;
    }

    let header_len = RTP_HEADER_SIZE + (csrc_count * 4);
    if packet.len() < header_len {
        return None;
    }

    let payload_end = if has_padding {
        let pad_bytes = *packet.last()? as usize;
        packet.len().checked_sub(pad_bytes)?
    } else {
        packet.len()
    };

    if payload_end < header_len {
        return None;
    }

    let payload = &packet[header_len..payload_end];
    let bytes_per_frame = channels as usize * 3;
    if bytes_per_frame == 0 || payload.len() != expected_frames * bytes_per_frame {
        return None;
    }

    let mut fixed = DecodedSamples {
        sample_count: expected_samples,
        samples: [0.0; MAX_PACKET_SAMPLES],
    };
    for (index, chunk) in payload.chunks_exact(3).enumerate() {
        fixed.samples[index] = read_l24_sample(chunk);
    }
    Some(fixed)
}

enum InvalidPacketKind {
    Header,
    Format,
    FrameMismatch { actual_samples: usize },
}

fn classify_invalid_packet(
    packet: &[u8],
    channels: u8,
    expected_frames: usize,
) -> InvalidPacketKind {
    if packet.len() < RTP_HEADER_SIZE || channels == 0 {
        return InvalidPacketKind::Header;
    }

    let version = packet[0] >> 6;
    if version != 2 {
        return InvalidPacketKind::Header;
    }

    let has_padding = packet[0] & 0x20 != 0;
    let csrc_count = (packet[0] & 0x0f) as usize;
    let payload_type = packet[1] & 0x7f;
    if payload_type != PAYLOAD_TYPE_L24 {
        return InvalidPacketKind::Format;
    }

    let header_len = RTP_HEADER_SIZE + (csrc_count * 4);
    if packet.len() < header_len {
        return InvalidPacketKind::Header;
    }

    let payload_end = if has_padding {
        let Some(last) = packet.last() else {
            return InvalidPacketKind::Header;
        };
        let pad_bytes = *last as usize;
        let Some(end) = packet.len().checked_sub(pad_bytes) else {
            return InvalidPacketKind::Header;
        };
        end
    } else {
        packet.len()
    };

    if payload_end < header_len {
        return InvalidPacketKind::Header;
    }

    let payload = &packet[header_len..payload_end];
    let bytes_per_frame = channels as usize * 3;
    if bytes_per_frame == 0
        || !payload.len().is_multiple_of(3)
        || !payload.len().is_multiple_of(bytes_per_frame)
    {
        return InvalidPacketKind::Format;
    }

    let actual_samples = payload.len() / 3;
    let expected_samples = expected_frames * channels as usize;
    if actual_samples != expected_samples {
        return InvalidPacketKind::FrameMismatch { actual_samples };
    }

    InvalidPacketKind::Format
}

fn parse_rtp_sequence(packet: &[u8]) -> Option<u16> {
    if packet.len() < RTP_HEADER_SIZE {
        return None;
    }

    Some(u16::from_be_bytes([packet[2], packet[3]]))
}

fn read_l24_sample(chunk: &[u8]) -> f32 {
    let sign = if chunk[0] & 0x80 != 0 { 0xff } else { 0x00 };
    let value = i32::from_be_bytes([sign, chunk[0], chunk[1], chunk[2]]);
    (value as f64 / 8_388_608.0) as f32
}

#[cfg(test)]
fn write_l24_sample_vec(dst: &mut Vec<u8>, sample: f32) {
    let mut packet = [0_u8; MAX_PACKET_SIZE];
    let mut len = 0;
    write_l24_sample(&mut packet, &mut len, sample);
    dst.extend_from_slice(&packet[..len]);
}

pub fn encode_state(state: StreamParameters) -> [u8; STATE_SIZE] {
    let mut out = [0_u8; STATE_SIZE];
    out[0..4].copy_from_slice(b"RST3");
    out[4] = 3;
    out[5] = state.enabled as u8;
    out[6] = state.mode.as_u8();
    out[7] = state.transport.as_u8();
    out[8] = state.channels;
    out[9..11].copy_from_slice(&state.port.to_le_bytes());
    out[11..15].copy_from_slice(&state.ip);
    out
}

pub fn decode_state(bytes: &[u8]) -> Option<StreamParameters> {
    if bytes.len() < LEGACY_STATE_SIZE || &bytes[0..4] != b"RST3" {
        return None;
    }

    let (mode, transport, channels_index, port_index, ip_index) = match bytes[4] {
        1 => (StreamMode::Send, StreamTransport::Unicast, 6, 7, 9),
        2 if bytes.len() >= LEGACY_STATE_SIZE => (
            StreamMode::from_u32(bytes[6] as u32),
            StreamTransport::Unicast,
            7,
            8,
            10,
        ),
        3 if bytes.len() >= STATE_SIZE => (
            StreamMode::from_u32(bytes[6] as u32),
            StreamTransport::from_u32(bytes[7] as u32),
            8,
            9,
            11,
        ),
        _ => return None,
    };

    let channels = bytes[channels_index].clamp(1, MAX_CHANNELS as u8);
    Some(StreamParameters {
        enabled: bytes[5] != 0,
        mode,
        transport,
        channels,
        port: u16::from_le_bytes([bytes[port_index], bytes[port_index + 1]]).max(1),
        ip: [
            bytes[ip_index],
            bytes[ip_index + 1],
            bytes[ip_index + 2],
            bytes[ip_index + 3],
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_mode_limits_follow_level_a_and_ax() {
        assert_eq!(stream_level(48_000, 16), Some(StreamLevel::A48k));
        assert_eq!(stream_level(44_100, 16), Some(StreamLevel::Legacy44k1));
        assert_eq!(stream_level(96_000, 8), Some(StreamLevel::AX96k));
        assert_eq!(stream_level(96_000, 16), None);
        assert_eq!(stream_level(192_000, 2), None);
    }

    #[test]
    fn state_round_trip_is_stable() {
        let original = StreamParameters {
            enabled: true,
            mode: StreamMode::Receive,
            transport: StreamTransport::Multicast,
            channels: 8,
            port: 5004,
            ip: [192, 168, 1, 42],
        };

        let encoded = encode_state(original);
        let decoded = decode_state(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn decode_v1_state_defaults_to_sender_mode() {
        let bytes = [b'R', b'S', b'T', b'3', 1, 1, 2, 0x8c, 0x13, 127, 0, 0, 1, 0];
        let decoded = decode_state(&bytes).unwrap();
        assert_eq!(decoded.mode, StreamMode::Send);
        assert_eq!(decoded.transport, StreamTransport::Unicast);
        assert_eq!(decoded.port, 5004);
        assert_eq!(decoded.channels, 2);
    }

    #[test]
    fn decode_v2_state_defaults_transport_to_unicast() {
        let bytes = [
            b'R', b'S', b'T', b'3', 2, 1, 1, 6, 0x8c, 0x13, 10, 20, 30, 40,
        ];
        let decoded = decode_state(&bytes).unwrap();
        assert_eq!(decoded.mode, StreamMode::Receive);
        assert_eq!(decoded.transport, StreamTransport::Unicast);
        assert_eq!(decoded.port, 5004);
        assert_eq!(decoded.channels, 6);
        assert_eq!(decoded.ip, [10, 20, 30, 40]);
    }

    #[test]
    fn multicast_requires_group_address_and_accepts_any_source() {
        let params = StreamParameters {
            enabled: true,
            mode: StreamMode::Receive,
            transport: StreamTransport::Multicast,
            channels: 2,
            port: 5004,
            ip: [239, 69, 1, 30],
        };

        assert!(params.sane_listener());
        assert!(params.sane_destination());
        assert!(params.accepts_source(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 9), 5004)));
    }

    #[test]
    fn unicast_rejects_unspecified_destination() {
        let params = StreamParameters {
            enabled: true,
            mode: StreamMode::Send,
            transport: StreamTransport::Unicast,
            channels: 2,
            port: 5004,
            ip: [0, 0, 0, 0],
        };

        assert!(!params.sane_destination());
    }

    #[test]
    fn l24_codec_round_trips_signed_samples() {
        let mut bytes = Vec::new();
        write_l24_sample_vec(&mut bytes, 1.0);
        write_l24_sample_vec(&mut bytes, -1.0);
        assert_eq!(&bytes[0..3], &[0x7f, 0xff, 0xff]);
        assert_eq!(&bytes[3..6], &[0x80, 0x00, 0x00]);
        assert!((read_l24_sample(&bytes[0..3]) - 0.999_999_9).abs() < 0.0001);
        assert!((read_l24_sample(&bytes[3..6]) + 1.0).abs() < 0.0001);
    }

    #[test]
    fn decode_rtp_l24_packet_extracts_interleaved_samples() {
        let mut packet = vec![0x80, PAYLOAD_TYPE_L24, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1];
        write_l24_sample_vec(&mut packet, 0.5);
        write_l24_sample_vec(&mut packet, -0.5);

        let decoded = decode_rtp_l24_packet(&packet, 2).unwrap();
        assert_eq!(decoded.len(), 2);
        assert!((decoded[0] - 0.5).abs() < 0.0001);
        assert!((decoded[1] + 0.5).abs() < 0.0001);
    }

    #[test]
    fn strict_decoder_rejects_channel_count_mismatch() {
        let mut packet = vec![0x80, PAYLOAD_TYPE_L24, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1];
        for _ in 0..(48 * 4) {
            write_l24_sample_vec(&mut packet, 0.25);
        }

        assert!(decode_rtp_l24_packet_strict(&packet, 2, 48).is_none());
        assert!(decode_rtp_l24_packet_strict(&packet, 4, 48).is_some());
    }

    #[test]
    fn sender_reset_clears_partial_packet_state() {
        let sender = NetworkSender::new();
        let params = StreamParameters {
            enabled: true,
            mode: StreamMode::Send,
            transport: StreamTransport::Unicast,
            channels: 2,
            port: 5004,
            ip: [127, 0, 0, 1],
        };
        let left = vec![0.1_f32; 24];
        let right = vec![-0.1_f32; 24];
        let input_channels = std::array::from_fn(|index| match index {
            0 => Some(left.as_slice()),
            1 => Some(right.as_slice()),
            _ => None,
        });

        sender.push_audio(params, 48_000, &input_channels, 24);
        assert_eq!(sender.status_snapshot().queued_frames, 24);
        sender.reset();
        let status = sender.status_snapshot();
        assert_eq!(status.queued_frames, 0);
        assert_eq!(status.packets_sent, 0);
    }

    #[test]
    fn concealment_packets_repeat_last_frame() {
        let mut state = ReceiverState::new();
        state.last_frame[0] = 0.75;
        state.last_frame[1] = -0.5;
        state.enqueue_concealment_packets(1, StreamLevel::A48k, 2);

        assert_eq!(state.queued_samples, 96);
        assert_eq!(state.pop_sample(), Some(0.75));
        assert_eq!(state.pop_sample(), Some(-0.5));
        assert_eq!(state.pop_sample(), Some(0.75));
        assert_eq!(state.pop_sample(), Some(-0.5));
    }

    #[test]
    fn target_buffer_scales_above_single_callback() {
        assert_eq!(target_buffer_samples(StreamLevel::A48k, 4, 512), 4_800);
        assert_eq!(target_buffer_samples(StreamLevel::A48k, 2, 128), 864);
        assert_eq!(
            target_buffer_samples(StreamLevel::Legacy44k1, 2, 512),
            6_174
        );
        assert_eq!(target_buffer_samples(StreamLevel::A48k, 16, 512), 19_200);
    }

    #[test]
    fn playout_adjustment_stretches_when_queue_runs_low() {
        assert_eq!(playout_adjustment_frames(400, 768, 48, 511, 512), -1);
    }

    #[test]
    fn playout_adjustment_compresses_when_queue_runs_high() {
        assert_eq!(playout_adjustment_frames(900, 768, 48, 513, 512), 1);
    }

    #[test]
    fn playout_adjustment_stays_neutral_near_target() {
        assert_eq!(playout_adjustment_frames(760, 768, 48, 512, 512), 0);
    }

    #[test]
    fn warning_text_marks_44100_as_non_standard() {
        assert!(warning_text(44_100, 2).contains("not ST 2110-30 compliant"));
        assert!(warning_text(48_000, 16).contains("outside ST 2110-30 Level A"));
        assert_eq!(warning_text(48_000, 8), "");
    }

    #[test]
    fn stalled_receiver_detects_missing_source() {
        let mut state = ReceiverState::new();
        state.last_frame[0] = 0.75;
        state.primed = true;
        state.last_packet_at =
            Some(Instant::now() - STALL_SILENCE_TIMEOUT - Duration::from_millis(1));

        assert!(state.is_stalled());
    }

    #[test]
    fn clear_stalled_stream_flushes_buffer_and_mutes() {
        let mut state = ReceiverState::new();
        state.queued_samples = 128;
        state.primed = true;
        state.last_frame[0] = 0.5;
        state.packet_queue.push_back(ReceiverPacket {
            sequence: 1,
            sample_count: 2,
            samples: [0.25; MAX_PACKET_SAMPLES],
        });
        let mut out = vec![1.0_f32; 8];
        let mut output_channels = [
            Some(out.as_mut_slice()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        ];

        state.clear_stalled_stream(&mut output_channels, 1, 8);

        assert_eq!(state.queued_samples, 0);
        assert!(!state.primed);
        assert!(out.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn sender_and_receiver_round_trip_over_loopback() {
        let probe = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let sender = NetworkSender::new();
        let receiver = NetworkReceiver::new();
        let params = StreamParameters {
            enabled: true,
            mode: StreamMode::Send,
            transport: StreamTransport::Unicast,
            channels: 2,
            port,
            ip: [127, 0, 0, 1],
        };

        let left = vec![0.25_f32; 48];
        let right = vec![-0.25_f32; 48];
        let input_channels = std::array::from_fn(|index| match index {
            0 => Some(left.as_slice()),
            1 => Some(right.as_slice()),
            _ => None,
        });

        let mut out_left = vec![0.0_f32; 48];
        let mut out_right = vec![0.0_f32; 48];

        {
            let mut output_channels: [Option<&mut [f32]>; MAX_CHANNELS] =
                std::array::from_fn(|_| None);
            output_channels[0] = Some(out_left.as_mut_slice());
            output_channels[1] = Some(out_right.as_mut_slice());
            receiver.pull_audio(params, 48_000, &mut output_channels, 48);
        }

        for _ in 0..SEND_STARTUP_PACKETS.max(STARTUP_BUFFER_PACKETS) {
            sender.push_audio(params, 48_000, &input_channels, 48);
        }
        std::thread::sleep(std::time::Duration::from_millis(30));

        {
            let mut output_channels: [Option<&mut [f32]>; MAX_CHANNELS] =
                std::array::from_fn(|_| None);
            output_channels[0] = Some(out_left.as_mut_slice());
            output_channels[1] = Some(out_right.as_mut_slice());
            receiver.pull_audio(params, 48_000, &mut output_channels, 48);
        }

        assert!(out_left.iter().any(|sample| (sample - 0.25).abs() < 0.01));
        assert!(out_right.iter().any(|sample| (sample + 0.25).abs() < 0.01));

        let sender_status = sender.status_snapshot();
        let receiver_status = receiver.status_snapshot();
        assert!(sender_status.packets_sent >= SEND_STARTUP_PACKETS as u64);
        assert!(receiver_status.packets_received >= 1);
        assert_eq!(receiver_status.underruns, 0);
    }
}
