use std::cell::UnsafeCell;
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

pub const MAX_CHANNELS: usize = 96;
pub const STATE_SIZE: usize = 17;
pub const LEGACY_STATE_SIZE: usize = 14;

const PAYLOAD_TYPE_L24: u8 = 96;
const RTP_HEADER_SIZE: usize = 12;
const MAX_PACKET_FRAMES: usize = 96;
const MAX_PACKET_SAMPLES: usize = MAX_PACKET_FRAMES * MAX_CHANNELS;
const MAX_PACKET_SIZE: usize = RTP_HEADER_SIZE + (MAX_PACKET_SAMPLES * 3);
const SDP_FILE_NAME: &str = "somenet.sdp";
const STARTUP_BUFFER_PACKETS: usize = 2;
const MAX_BUFFER_PACKETS: usize = 128;
const MAX_BUFFER_SAMPLES: usize = MAX_BUFFER_PACKETS * MAX_PACKET_SAMPLES;
const TARGET_CALLBACKS: usize = 1;
const TARGET_SAFETY_PACKETS: usize = 1;
const DRIFT_THRESHOLD_PACKETS: usize = 1;
const SEND_STARTUP_PACKETS: usize = 2;
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
const SOCKET_BUFFER_BYTES: i32 = 4 << 20;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockReference {
    Local,
    Ptp,
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

impl ClockReference {
    pub fn from_u32(value: u32) -> Self {
        match value {
            1 => Self::Ptp,
            _ => Self::Local,
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Self::Local => 0,
            Self::Ptp => 1,
        }
    }

    pub fn status_name(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Ptp => "ptp",
        }
    }

    fn sdp_ts_refclk(self) -> &'static str {
        match self {
            Self::Local => "a=ts-refclk:local\r\n",
            Self::Ptp => "a=ts-refclk:ptp=IEEE1588-2008:traceable\r\n",
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
    pub clock_reference: ClockReference,
    pub ptp_domain: u8,
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
            Self::Legacy44k1 => 44,
            Self::AX96k => 96,
        }
    }

    fn packet_interval(self) -> Duration {
        match self {
            Self::A48k | Self::AX96k => RTP_PACKET_INTERVAL,
            Self::Legacy44k1 => Duration::from_nanos(997_732),
        }
    }

    fn ptime_ms(self) -> u32 {
        match self {
            Self::A48k | Self::Legacy44k1 | Self::AX96k => 1,
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
        (48_000, 1..=96) => Some(StreamLevel::A48k),
        (44_100, 1..=96) => Some(StreamLevel::Legacy44k1),
        (96_000, 1..=96) => Some(StreamLevel::AX96k),
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

fn configure_socket_priority(_socket: &UdpSocket) {
    #[cfg(unix)]
    {
        set_socket_option_int(
            _socket,
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            SOCKET_BUFFER_BYTES,
        );
        set_socket_option_int(
            _socket,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            SOCKET_BUFFER_BYTES,
        );

        set_socket_option_int(
            _socket,
            libc::IPPROTO_IP,
            libc::IP_TOS,
            DSCP_EXPEDITED_FORWARDING,
        );

        #[cfg(any(target_os = "linux", target_os = "android"))]
        set_socket_option_int(
            _socket,
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

pub fn warning_text(sample_rate_hz: u32, channels: u8, clock_reference: ClockReference) -> String {
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
    if channels > 64 {
        warnings.push(
            "More than 64 channels is transport-only; the VST3 wrapper is capped at 64 channels",
        );
    }
    if channels >= 64 && sample_rate_hz >= 48_000 {
        warnings.push("High-channel RTP packets can exceed standard Ethernet MTU; use wired low-jitter networking");
    }
    if matches!(clock_reference, ClockReference::Ptp) {
        warnings.push(
            "PTP mode currently advertises SDP clock reference and domain only; host clock discipline is still TODO",
        );
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
    tx: SyncSender<Box<SentPacket>>,
    shutdown: std::sync::Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl SenderWorker {
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

#[derive(Clone)]
struct SenderWorkerStatus {
    active: std::sync::Arc<AtomicBool>,
    packets_sent: std::sync::Arc<AtomicU64>,
    packets_dropped: std::sync::Arc<AtomicU64>,
}

#[derive(Clone)]
struct ReceiverWorkerStatus {
    active: std::sync::Arc<AtomicBool>,
    packets_dropped: std::sync::Arc<AtomicU64>,
    packets_invalid: std::sync::Arc<AtomicU64>,
    packets_invalid_header: std::sync::Arc<AtomicU64>,
    packets_invalid_format: std::sync::Arc<AtomicU64>,
    packets_invalid_frame_mismatch: std::sync::Arc<AtomicU64>,
    last_invalid_samples: std::sync::Arc<AtomicU64>,
}

fn spawn_sender_worker(
    stream: ActiveStream,
    sdp_path: PathBuf,
    status: SenderWorkerStatus,
) -> (SenderWorker, Receiver<Box<SentPacket>>) {
    let (tx, rx) = sync_channel::<Box<SentPacket>>(SEND_QUEUE_PACKETS);
    let (recycle_tx, recycle_rx) = sync_channel::<Box<SentPacket>>(SEND_QUEUE_PACKETS);
    let shutdown = std::sync::Arc::new(AtomicBool::new(false));
    let thread_shutdown = shutdown.clone();
    let thread_active = status.active.clone();
    let thread_packets_sent = status.packets_sent.clone();
    let thread_packets_dropped = status.packets_dropped.clone();

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
            stream.params,
        );
        thread_active.store(true, Ordering::Relaxed);
        let mut backlog = VecDeque::<Box<SentPacket>>::with_capacity(MAX_SENDER_BACKLOG_PACKETS);
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
                    next_send_at = Some(Instant::now());
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
                    } else {
                        thread_packets_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    if let Some(mut packet) = backlog.pop_front() {
                        packet.clear_payload();
                        let _ = recycle_tx.try_send(packet);
                    }
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
                    while let Some(mut packet) = backlog.pop_front() {
                        if socket.send(&packet.data[..packet.len]).is_ok() {
                            thread_packets_sent.fetch_add(1, Ordering::Relaxed);
                        } else {
                            thread_packets_dropped.fetch_add(1, Ordering::Relaxed);
                        }
                        packet.clear_payload();
                        let _ = recycle_tx.try_send(packet);
                    }
                    break;
                }
            }
        }

        thread_active.store(false, Ordering::Relaxed);
    });

    (
        SenderWorker {
            tx,
            shutdown,
            join: Some(join),
        },
        recycle_rx,
    )
}

fn spawn_receiver_worker(
    stream: ActiveStream,
    status: ReceiverWorkerStatus,
) -> (ReceiverWorker, Receiver<ReceiverPacket>) {
    let (tx, rx) = sync_channel::<ReceiverPacket>(RECEIVE_QUEUE_PACKETS);
    let shutdown = std::sync::Arc::new(AtomicBool::new(false));
    let thread_shutdown = shutdown.clone();
    let thread_active = status.active.clone();
    let thread_packets_dropped = status.packets_dropped.clone();
    let thread_packets_invalid = status.packets_invalid.clone();
    let thread_packets_invalid_header = status.packets_invalid_header.clone();
    let thread_packets_invalid_format = status.packets_invalid_format.clone();
    let thread_packets_invalid_frame_mismatch = status.packets_invalid_frame_mismatch.clone();
    let thread_last_invalid_samples = status.last_invalid_samples.clone();

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

    (
        ReceiverWorker {
            shutdown,
            join: Some(join),
        },
        rx,
    )
}

struct SenderControlState {
    stream: Option<ActiveStream>,
    worker: Option<SenderWorker>,
    sdp_path: PathBuf,
}

impl SenderControlState {
    fn new() -> Self {
        Self {
            stream: None,
            worker: None,
            sdp_path: std::env::temp_dir().join(SDP_FILE_NAME),
        }
    }

    fn reset(&mut self) {
        self.stream = None;
        if let Some(mut worker) = self.worker.take() {
            worker.stop();
        }
    }

    fn reconfigure(
        &mut self,
        candidate: ActiveStream,
        status: SenderWorkerStatus,
    ) -> (SyncSender<Box<SentPacket>>, Receiver<Box<SentPacket>>) {
        self.reset();
        let (worker, recycle_rx) = spawn_sender_worker(candidate, self.sdp_path.clone(), status);
        self.stream = Some(candidate);
        self.worker = Some(worker);
        (
            self.worker
                .as_ref()
                .expect("sender worker must exist after reconfigure")
                .tx
                .clone(),
            recycle_rx,
        )
    }
}

struct SenderAudioState {
    stream: Option<ActiveStream>,
    tx: Option<SyncSender<Box<SentPacket>>>,
    recycle_rx: Option<Receiver<Box<SentPacket>>>,
    pending_packet: Box<SentPacket>,
    pending_frames: usize,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
}

impl SenderAudioState {
    fn new() -> Self {
        Self {
            stream: None,
            tx: None,
            recycle_rx: None,
            pending_packet: Box::new(SentPacket::new()),
            pending_frames: 0,
            sequence: 0,
            timestamp: 0,
            ssrc: seed_ssrc(),
        }
    }

    fn reset_local(&mut self) {
        self.stream = None;
        self.tx = None;
        self.recycle_rx = None;
        self.pending_packet.clear_payload();
        self.pending_frames = 0;
        self.sequence = 0;
        self.timestamp = 0;
        self.ssrc = seed_ssrc();
    }

    fn recycled_or_fresh(&mut self) -> Box<SentPacket> {
        let Some(recycle_rx) = self.recycle_rx.as_ref() else {
            return Box::new(SentPacket::new());
        };
        match recycle_rx.try_recv() {
            Ok(mut packet) => {
                packet.clear_payload();
                packet
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => Box::new(SentPacket::new()),
        }
    }

    fn push_block(
        &mut self,
        params: StreamParameters,
        level: StreamLevel,
        input_channels: &[Option<&[f32]>; MAX_CHANNELS],
        frames: usize,
        dropped_counter: &AtomicU64,
    ) {
        if self.tx.is_none() {
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
                self.flush_packet(level.packet_frames() as u32, dropped_counter);
            }
        }
    }

    fn flush_packet(&mut self, packet_frames: u32, dropped_counter: &AtomicU64) {
        let Some(tx) = self.tx.clone() else {
            return;
        };

        self.pending_packet.data[0] = 0x80;
        self.pending_packet.data[1] = PAYLOAD_TYPE_L24 & 0x7f;
        self.pending_packet.data[2..4].copy_from_slice(&self.sequence.to_be_bytes());
        self.pending_packet.data[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        self.pending_packet.data[8..12].copy_from_slice(&self.ssrc.to_be_bytes());

        let replacement = self.recycled_or_fresh();
        let packet = std::mem::replace(&mut self.pending_packet, replacement);

        match tx.try_send(packet) {
            Ok(()) => {}
            Err(TrySendError::Full(mut packet)) | Err(TrySendError::Disconnected(mut packet)) => {
                packet.clear_payload();
                self.pending_packet = packet;
                dropped_counter.fetch_add(1, Ordering::Relaxed);
            }
        }

        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(packet_frames);
        self.pending_frames = 0;
        self.pending_packet.clear_payload();
    }
}

pub struct NetworkSender {
    control: Mutex<SenderControlState>,
    audio: UnsafeCell<SenderAudioState>,
    reset_requested: AtomicBool,
    active: std::sync::Arc<AtomicBool>,
    packets_sent: std::sync::Arc<AtomicU64>,
    packets_dropped: std::sync::Arc<AtomicU64>,
    queued_frames: AtomicU64,
}

// Safety: only the real-time audio callback mutates `audio`, while reconfiguration and reset
// happen via atomics plus the separate control mutex and never access the audio state directly.
unsafe impl Sync for NetworkSender {}

impl Default for NetworkSender {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkSender {
    pub fn new() -> Self {
        Self {
            control: Mutex::new(SenderControlState::new()),
            audio: UnsafeCell::new(SenderAudioState::new()),
            reset_requested: AtomicBool::new(false),
            active: std::sync::Arc::new(AtomicBool::new(false)),
            packets_sent: std::sync::Arc::new(AtomicU64::new(0)),
            packets_dropped: std::sync::Arc::new(AtomicU64::new(0)),
            queued_frames: AtomicU64::new(0),
        }
    }

    fn worker_status(&self) -> SenderWorkerStatus {
        SenderWorkerStatus {
            active: self.active.clone(),
            packets_sent: self.packets_sent.clone(),
            packets_dropped: self.packets_dropped.clone(),
        }
    }

    fn clear_status(&self) {
        self.active.store(false, Ordering::Relaxed);
        self.packets_sent.store(0, Ordering::Relaxed);
        self.packets_dropped.store(0, Ordering::Relaxed);
        self.queued_frames.store(0, Ordering::Relaxed);
    }

    #[allow(clippy::mut_from_ref)]
    fn audio_state_mut(&self) -> &mut SenderAudioState {
        // Safety: `process` is the sole caller on the audio thread, and non-audio threads never
        // dereference `audio`; they only signal reset via atomics and reconfigure worker state.
        unsafe { &mut *self.audio.get() }
    }

    fn ensure_stream(&self, audio: &mut SenderAudioState, candidate: ActiveStream) {
        if audio.stream == Some(candidate) && audio.tx.is_some() {
            return;
        }

        audio.reset_local();
        let Ok(mut control) = self.control.lock() else {
            return;
        };
        let (tx, recycle_rx) = control.reconfigure(candidate, self.worker_status());
        audio.stream = Some(candidate);
        audio.tx = Some(tx);
        audio.recycle_rx = Some(recycle_rx);
    }

    pub fn reset(&self) {
        self.reset_requested.store(true, Ordering::Relaxed);
        self.clear_status();
        if let Ok(mut control) = self.control.lock() {
            control.reset();
        }
    }

    pub fn push_audio(
        &self,
        params: StreamParameters,
        sample_rate_hz: u32,
        input_channels: &[Option<&[f32]>; MAX_CHANNELS],
        frames: usize,
    ) {
        let audio = self.audio_state_mut();
        if self.reset_requested.swap(false, Ordering::Relaxed) {
            audio.reset_local();
        }

        if !params.enabled || !params.sane_destination() {
            self.queued_frames.store(0, Ordering::Relaxed);
            return;
        }

        let Some(level) = stream_level(sample_rate_hz, params.channels) else {
            self.queued_frames.store(0, Ordering::Relaxed);
            return;
        };

        self.ensure_stream(audio, ActiveStream { params, level });
        audio.push_block(
            params,
            level,
            input_channels,
            frames,
            self.packets_dropped.as_ref(),
        );
        self.queued_frames
            .store(audio.pending_frames as u64, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn status_snapshot(&self) -> SenderStatus {
        SenderStatus {
            active: self.active.load(Ordering::Relaxed),
            packets_sent: self.packets_sent.load(Ordering::Relaxed),
            packets_dropped: self.packets_dropped.load(Ordering::Relaxed),
            queued_frames: self.queued_frames.load(Ordering::Relaxed) as usize,
        }
    }
}

struct ReceiverControlState {
    stream: Option<ActiveStream>,
    worker: Option<ReceiverWorker>,
}

impl ReceiverControlState {
    fn new() -> Self {
        Self {
            stream: None,
            worker: None,
        }
    }

    fn reset(&mut self) {
        self.stream = None;
        if let Some(mut worker) = self.worker.take() {
            worker.stop();
        }
    }

    fn reconfigure(
        &mut self,
        candidate: ActiveStream,
        status: ReceiverWorkerStatus,
    ) -> Receiver<ReceiverPacket> {
        self.reset();
        let (worker, rx) = spawn_receiver_worker(candidate, status);
        self.stream = Some(candidate);
        self.worker = Some(worker);
        rx
    }
}

struct ReceiverAudioState {
    stream: Option<ActiveStream>,
    worker_rx: Option<Receiver<ReceiverPacket>>,
    sample_buffer: Vec<f32>,
    read_index: usize,
    write_index: usize,
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

impl ReceiverAudioState {
    fn new() -> Self {
        Self {
            stream: None,
            worker_rx: None,
            sample_buffer: vec![0.0; MAX_BUFFER_SAMPLES],
            read_index: 0,
            write_index: 0,
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

    fn reset_local(&mut self) {
        self.stream = None;
        self.worker_rx = None;
        self.read_index = 0;
        self.write_index = 0;
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

    fn receive_into_queue(&mut self, params: StreamParameters, level: StreamLevel) {
        for _ in 0..MAX_PACKETS_PER_CALLBACK {
            let Some(worker_rx) = self.worker_rx.as_ref() else {
                return;
            };

            match worker_rx.try_recv() {
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
                    self.reset_local();
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
        let overflow = self
            .queued_samples
            .saturating_add(packet.sample_count)
            .saturating_sub(self.sample_buffer.len());
        if overflow > 0 {
            self.drop_oldest_samples(overflow);
        }

        for sample in packet.samples.iter().take(packet.sample_count) {
            self.sample_buffer[self.write_index] = *sample;
            self.write_index = (self.write_index + 1) % self.sample_buffer.len();
        }
        self.queued_samples = self.queued_samples.saturating_add(packet.sample_count);
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

        self.read_index = 0;
        self.write_index = 0;
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
        for output_channel in output_channels.iter_mut().take(channels) {
            if let Some(channel) = output_channel.as_deref_mut() {
                channel[start_frame..end_frame].fill(value);
            }
        }
    }

    fn clear_stalled_stream(
        &mut self,
        output_channels: &mut [Option<&mut [f32]>; MAX_CHANNELS],
        channels: usize,
        frames: usize,
    ) {
        self.read_index = 0;
        self.write_index = 0;
        self.queued_samples = 0;
        self.last_frame.fill(0.0);
        self.primed = false;
        self.output_constant(output_channels, channels, 0, frames, 0.0);
    }

    fn trim_queue(&mut self, level: StreamLevel, channels: u8) {
        let max_samples = level.packet_frames() * channels as usize * MAX_BUFFER_PACKETS;
        let overflow = self.queued_samples.saturating_sub(max_samples);
        if overflow > 0 {
            self.drop_oldest_samples(overflow);
        }
    }

    fn pull_block(
        &mut self,
        params: StreamParameters,
        level: StreamLevel,
        output_channels: &mut [Option<&mut [f32]>; MAX_CHANNELS],
        frames: usize,
    ) {
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
                self.output_constant(output_channels, channels, 0, frames, 0.0);
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

    fn pop_sample(&mut self) -> Option<f32> {
        if self.queued_samples == 0 {
            return None;
        }

        let sample = self.sample_buffer[self.read_index];
        self.read_index = (self.read_index + 1) % self.sample_buffer.len();
        self.queued_samples = self.queued_samples.saturating_sub(1);
        Some(sample)
    }

    fn drop_oldest_samples(&mut self, count: usize) {
        let to_drop = count.min(self.queued_samples);
        self.read_index = (self.read_index + to_drop) % self.sample_buffer.len();
        self.queued_samples -= to_drop;
    }
}

pub struct NetworkReceiver {
    control: Mutex<ReceiverControlState>,
    audio: UnsafeCell<ReceiverAudioState>,
    reset_requested: AtomicBool,
    active: std::sync::Arc<AtomicBool>,
    primed: AtomicBool,
    queued_samples: AtomicU64,
    target_buffer_samples: AtomicU64,
    last_callback_frames: AtomicU64,
    packets_received: AtomicU64,
    packets_dropped: std::sync::Arc<AtomicU64>,
    packets_invalid: std::sync::Arc<AtomicU64>,
    packets_invalid_header: std::sync::Arc<AtomicU64>,
    packets_invalid_format: std::sync::Arc<AtomicU64>,
    packets_invalid_frame_mismatch: std::sync::Arc<AtomicU64>,
    last_invalid_samples: std::sync::Arc<AtomicU64>,
    packets_lost: AtomicU64,
    packets_out_of_order: AtomicU64,
    underruns: AtomicU64,
    drift_corrections: AtomicU64,
}

// Safety: only the audio callback mutates `audio`. Other threads coordinate resets and worker
// lifetime through atomics plus the control mutex and never touch the callback-owned state.
unsafe impl Sync for NetworkReceiver {}

impl Default for NetworkReceiver {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkReceiver {
    pub fn new() -> Self {
        Self {
            control: Mutex::new(ReceiverControlState::new()),
            audio: UnsafeCell::new(ReceiverAudioState::new()),
            reset_requested: AtomicBool::new(false),
            active: std::sync::Arc::new(AtomicBool::new(false)),
            primed: AtomicBool::new(false),
            queued_samples: AtomicU64::new(0),
            target_buffer_samples: AtomicU64::new(0),
            last_callback_frames: AtomicU64::new(0),
            packets_received: AtomicU64::new(0),
            packets_dropped: std::sync::Arc::new(AtomicU64::new(0)),
            packets_invalid: std::sync::Arc::new(AtomicU64::new(0)),
            packets_invalid_header: std::sync::Arc::new(AtomicU64::new(0)),
            packets_invalid_format: std::sync::Arc::new(AtomicU64::new(0)),
            packets_invalid_frame_mismatch: std::sync::Arc::new(AtomicU64::new(0)),
            last_invalid_samples: std::sync::Arc::new(AtomicU64::new(0)),
            packets_lost: AtomicU64::new(0),
            packets_out_of_order: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            drift_corrections: AtomicU64::new(0),
        }
    }

    fn worker_status(&self) -> ReceiverWorkerStatus {
        ReceiverWorkerStatus {
            active: self.active.clone(),
            packets_dropped: self.packets_dropped.clone(),
            packets_invalid: self.packets_invalid.clone(),
            packets_invalid_header: self.packets_invalid_header.clone(),
            packets_invalid_format: self.packets_invalid_format.clone(),
            packets_invalid_frame_mismatch: self.packets_invalid_frame_mismatch.clone(),
            last_invalid_samples: self.last_invalid_samples.clone(),
        }
    }

    fn clear_status(&self) {
        self.active.store(false, Ordering::Relaxed);
        self.primed.store(false, Ordering::Relaxed);
        self.queued_samples.store(0, Ordering::Relaxed);
        self.target_buffer_samples.store(0, Ordering::Relaxed);
        self.last_callback_frames.store(0, Ordering::Relaxed);
        self.packets_received.store(0, Ordering::Relaxed);
        self.packets_dropped.store(0, Ordering::Relaxed);
        self.packets_invalid.store(0, Ordering::Relaxed);
        self.packets_invalid_header.store(0, Ordering::Relaxed);
        self.packets_invalid_format.store(0, Ordering::Relaxed);
        self.packets_invalid_frame_mismatch
            .store(0, Ordering::Relaxed);
        self.last_invalid_samples.store(0, Ordering::Relaxed);
        self.packets_lost.store(0, Ordering::Relaxed);
        self.packets_out_of_order.store(0, Ordering::Relaxed);
        self.underruns.store(0, Ordering::Relaxed);
        self.drift_corrections.store(0, Ordering::Relaxed);
    }

    fn publish_audio_status(&self, audio: &ReceiverAudioState) {
        self.primed.store(audio.primed, Ordering::Relaxed);
        self.queued_samples
            .store(audio.queued_samples as u64, Ordering::Relaxed);
        self.target_buffer_samples
            .store(audio.target_buffer_samples as u64, Ordering::Relaxed);
        self.last_callback_frames
            .store(audio.last_callback_frames as u64, Ordering::Relaxed);
        self.packets_received
            .store(audio.packets_received, Ordering::Relaxed);
        self.packets_lost
            .store(audio.packets_lost, Ordering::Relaxed);
        self.packets_out_of_order
            .store(audio.packets_out_of_order, Ordering::Relaxed);
        self.underruns.store(audio.underruns, Ordering::Relaxed);
        self.drift_corrections
            .store(audio.drift_corrections, Ordering::Relaxed);
    }

    #[allow(clippy::mut_from_ref)]
    fn audio_state_mut(&self) -> &mut ReceiverAudioState {
        // Safety: `process` is the only caller on the audio thread, and non-audio threads only
        // signal resets or replace worker lifetime through the separate control state.
        unsafe { &mut *self.audio.get() }
    }

    fn ensure_stream(&self, audio: &mut ReceiverAudioState, candidate: ActiveStream) {
        if audio.stream == Some(candidate) && audio.worker_rx.is_some() {
            return;
        }

        audio.reset_local();
        let Ok(mut control) = self.control.lock() else {
            return;
        };
        let rx = control.reconfigure(candidate, self.worker_status());
        audio.stream = Some(candidate);
        audio.worker_rx = Some(rx);
    }

    pub fn reset(&self) {
        self.reset_requested.store(true, Ordering::Relaxed);
        self.clear_status();
        if let Ok(mut control) = self.control.lock() {
            control.reset();
        }
    }

    pub fn pull_audio(
        &self,
        params: StreamParameters,
        sample_rate_hz: u32,
        output_channels: &mut [Option<&mut [f32]>; MAX_CHANNELS],
        frames: usize,
    ) {
        let audio = self.audio_state_mut();
        if self.reset_requested.swap(false, Ordering::Relaxed) {
            audio.reset_local();
        }

        if !params.enabled || !params.sane_listener() {
            zero_all_outputs(output_channels);
            self.publish_audio_status(audio);
            return;
        }

        let Some(level) = stream_level(sample_rate_hz, params.channels) else {
            zero_all_outputs(output_channels);
            self.publish_audio_status(audio);
            return;
        };

        self.ensure_stream(audio, ActiveStream { params, level });
        if audio.worker_rx.is_some() {
            audio.pull_block(params, level, output_channels, frames);
        } else {
            zero_all_outputs(output_channels);
        }
        self.publish_audio_status(audio);
    }

    #[allow(dead_code)]
    pub fn status_snapshot(&self) -> ReceiverStatus {
        ReceiverStatus {
            active: self.active.load(Ordering::Relaxed),
            primed: self.primed.load(Ordering::Relaxed),
            queued_samples: self.queued_samples.load(Ordering::Relaxed) as usize,
            target_buffer_samples: self.target_buffer_samples.load(Ordering::Relaxed) as usize,
            last_callback_frames: self.last_callback_frames.load(Ordering::Relaxed) as usize,
            packets_received: self.packets_received.load(Ordering::Relaxed),
            packets_dropped: self.packets_dropped.load(Ordering::Relaxed),
            packets_invalid: self.packets_invalid.load(Ordering::Relaxed),
            packets_invalid_header: self.packets_invalid_header.load(Ordering::Relaxed),
            packets_invalid_format: self.packets_invalid_format.load(Ordering::Relaxed),
            packets_invalid_frame_mismatch: self
                .packets_invalid_frame_mismatch
                .load(Ordering::Relaxed),
            last_invalid_samples: self.last_invalid_samples.load(Ordering::Relaxed) as usize,
            packets_lost: self.packets_lost.load(Ordering::Relaxed),
            packets_out_of_order: self.packets_out_of_order.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
            drift_corrections: self.drift_corrections.load(Ordering::Relaxed),
        }
    }
}

fn zero_all_outputs(output_channels: &mut [Option<&mut [f32]>; MAX_CHANNELS]) {
    for channel in output_channels.iter_mut() {
        if let Some(buffer) = channel.as_deref_mut() {
            buffer.fill(0.0);
        }
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
    params: StreamParameters,
) {
    let mut extra_clock_lines = String::new();
    if matches!(params.clock_reference, ClockReference::Ptp) {
        extra_clock_lines.push_str(&format!("a=x-somenet-ptp-domain:{}\r\n", params.ptp_domain));
    }
    let content = format!(
        "v=0\r\n\
o=- 0 0 IN IP4 {local_ip}\r\n\
s=SomeNET Sender\r\n\
c=IN IP4 {destination_ip}\r\n\
t=0 0\r\n\
m=audio {destination_port} RTP/AVP {payload_type}\r\n\
a=rtpmap:{payload_type} L24/{sample_rate}/{channels}\r\n\
a=fmtp:{payload_type} channel-order=SMPTE2110.({channel_order})\r\n\
a=ptime:{ptime}\r\n\
a=maxptime:{ptime}\r\n\
a=sendonly\r\n\
{a_clock_ref}\
a=mediaclk:direct=0\r\n\
{a_clock_extra}\
a=x-conformance:{conformance}\r\n",
        destination_ip = destination.ip(),
        destination_port = destination.port(),
        payload_type = PAYLOAD_TYPE_L24,
        sample_rate = level.sample_rate_hz(),
        channels = params.channels,
        channel_order = channel_order(params.channels),
        ptime = level.ptime_ms(),
        a_clock_ref = params.clock_reference.sdp_ts_refclk(),
        a_clock_extra = extra_clock_lines,
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
    let has_extension = packet[0] & 0x10 != 0;
    let csrc_count = (packet[0] & 0x0f) as usize;
    let payload_type = packet[1] & 0x7f;
    if has_extension || payload_type != PAYLOAD_TYPE_L24 {
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
    let has_extension = packet[0] & 0x10 != 0;
    let csrc_count = (packet[0] & 0x0f) as usize;
    let payload_type = packet[1] & 0x7f;
    if has_extension || payload_type != PAYLOAD_TYPE_L24 {
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
    let has_extension = packet[0] & 0x10 != 0;
    let csrc_count = (packet[0] & 0x0f) as usize;
    let payload_type = packet[1] & 0x7f;
    if has_extension {
        return InvalidPacketKind::Format;
    }
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
    out[4] = 4;
    out[5] = state.enabled as u8;
    out[6] = state.mode.as_u8();
    out[7] = state.transport.as_u8();
    out[8] = state.channels;
    out[9..11].copy_from_slice(&state.port.to_le_bytes());
    out[11..15].copy_from_slice(&state.ip);
    out[15] = state.clock_reference.as_u8();
    out[16] = state.ptp_domain.min(127);
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
        3 if bytes.len() >= 15 => (
            StreamMode::from_u32(bytes[6] as u32),
            StreamTransport::from_u32(bytes[7] as u32),
            8,
            9,
            11,
        ),
        4 if bytes.len() >= STATE_SIZE => (
            StreamMode::from_u32(bytes[6] as u32),
            StreamTransport::from_u32(bytes[7] as u32),
            8,
            9,
            11,
        ),
        _ => return None,
    };

    let channels = bytes[channels_index].clamp(1, MAX_CHANNELS as u8);
    let (clock_reference, ptp_domain) = match bytes[4] {
        4 if bytes.len() >= STATE_SIZE => (
            ClockReference::from_u32(bytes[15] as u32),
            bytes[16].min(127),
        ),
        _ => (ClockReference::Local, 0),
    };
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
        clock_reference,
        ptp_domain,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_mode_limits_follow_level_a_and_ax() {
        assert_eq!(stream_level(48_000, 96), Some(StreamLevel::A48k));
        assert_eq!(stream_level(44_100, 96), Some(StreamLevel::Legacy44k1));
        assert_eq!(stream_level(96_000, 96), Some(StreamLevel::AX96k));
        assert_eq!(stream_level(48_000, 97), None);
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
            clock_reference: ClockReference::Ptp,
            ptp_domain: 0,
        };

        let encoded = encode_state(original);
        let decoded = decode_state(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn state_round_trip_preserves_96_channels() {
        let original = StreamParameters {
            enabled: true,
            mode: StreamMode::Receive,
            transport: StreamTransport::Unicast,
            channels: 96,
            port: 5004,
            ip: [10, 1, 2, 3],
            clock_reference: ClockReference::Local,
            ptp_domain: 0,
        };

        let encoded = encode_state(original);
        let decoded = decode_state(&encoded).unwrap();
        assert_eq!(decoded.channels, 96);
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
            clock_reference: ClockReference::Local,
            ptp_domain: 0,
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
            clock_reference: ClockReference::Local,
            ptp_domain: 0,
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
    fn decoder_rejects_unsupported_rtp_extensions() {
        let mut packet = vec![0x90, PAYLOAD_TYPE_L24, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1];
        for _ in 0..(48 * 2) {
            write_l24_sample_vec(&mut packet, 0.25);
        }

        assert!(decode_rtp_l24_packet_strict(&packet, 2, 48).is_none());
        assert!(matches!(
            classify_invalid_packet(&packet, 2, 48),
            InvalidPacketKind::Format
        ));
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
            clock_reference: ClockReference::Local,
            ptp_domain: 0,
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
        let mut state = ReceiverAudioState::new();
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
        assert_eq!(target_buffer_samples(StreamLevel::A48k, 4, 512), 2_304);
        assert_eq!(target_buffer_samples(StreamLevel::A48k, 2, 128), 384);
        assert_eq!(
            target_buffer_samples(StreamLevel::Legacy44k1, 2, 512),
            1_144
        );
        assert_eq!(target_buffer_samples(StreamLevel::A48k, 16, 512), 9_216);
        assert_eq!(target_buffer_samples(StreamLevel::A48k, 96, 64), 13_824);
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
        assert!(
            warning_text(44_100, 2, ClockReference::Local).contains("not ST 2110-30 compliant")
        );
        assert!(
            warning_text(48_000, 16, ClockReference::Local).contains("outside ST 2110-30 Level A")
        );
        assert_eq!(warning_text(48_000, 8, ClockReference::Local), "");
        assert!(warning_text(48_000, 96, ClockReference::Local).contains("transport-only"));
        assert!(warning_text(48_000, 2, ClockReference::Ptp).contains("PTP mode"));
    }

    #[test]
    fn stalled_receiver_detects_missing_source() {
        let mut state = ReceiverAudioState::new();
        state.last_frame[0] = 0.75;
        state.primed = true;
        state.last_packet_at =
            Some(Instant::now() - STALL_SILENCE_TIMEOUT - Duration::from_millis(1));

        assert!(state.is_stalled());
    }

    #[test]
    fn clear_stalled_stream_flushes_buffer_and_mutes() {
        let mut state = ReceiverAudioState::new();
        state.primed = true;
        state.last_frame[0] = 0.5;
        state.enqueue_packet(ReceiverPacket {
            sequence: 1,
            sample_count: 2,
            samples: [0.25; MAX_PACKET_SAMPLES],
        });
        let mut out = vec![1.0_f32; 8];
        {
            let mut output_channels: [Option<&mut [f32]>; MAX_CHANNELS] =
                std::array::from_fn(|_| None);
            output_channels[0] = Some(out.as_mut_slice());
            state.clear_stalled_stream(&mut output_channels, 1, 8);
        }

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
            clock_reference: ClockReference::Local,
            ptp_domain: 0,
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

        let deadline = Instant::now() + Duration::from_millis(250);
        let mut received_audio = false;
        while Instant::now() < deadline {
            sender.push_audio(params, 48_000, &input_channels, 48);

            {
                let mut output_channels: [Option<&mut [f32]>; MAX_CHANNELS] =
                    std::array::from_fn(|_| None);
                output_channels[0] = Some(out_left.as_mut_slice());
                output_channels[1] = Some(out_right.as_mut_slice());
                receiver.pull_audio(params, 48_000, &mut output_channels, 48);
            }

            received_audio = out_left.iter().any(|sample| (sample - 0.25).abs() < 0.01)
                && out_right.iter().any(|sample| (sample + 0.25).abs() < 0.01);
            if received_audio {
                break;
            }

            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(received_audio);

        let sender_status = sender.status_snapshot();
        let receiver_status = receiver.status_snapshot();
        assert!(sender_status.packets_sent >= 1);
        assert!(receiver_status.packets_received >= 1);
        assert_eq!(receiver_status.underruns, 0);
    }
}
