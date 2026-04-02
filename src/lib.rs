#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(unsafe_op_in_unsafe_fn)]

mod editor_api;
mod network;
mod params;

use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_void};
use std::fs;
use std::ptr;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{slice, str};

use network::{
    LEGACY_STATE_SIZE, MAX_CHANNELS, NetworkReceiver, NetworkSender, ReceiverStatus, SenderStatus,
    StreamMode, StreamParameters, StreamTransport, decode_state, encode_state,
};
use params::{
    PARAM_APPLY_SEQ, PARAM_CHANNELS, PARAM_COUNT, PARAM_ENABLED, PARAM_IP_1, PARAM_IP_2,
    PARAM_IP_3, PARAM_IP_4, PARAM_MODE, PARAM_PORT, PARAM_TRANSPORT, copy_cstring,
    default_stream_parameters, parameter_spec,
};
use vst3::{Class, ComPtr, ComRef, ComWrapper, Steinberg::Vst::*, Steinberg::*, uid};

use crate::editor_api::EditorControllerApi;

#[cfg(target_os = "macos")]
mod macos_gui;

const PLUGIN_NAME: &str = "SomethingNet";
const VENDOR_NAME: &str = "Something Audio";
const VENDOR_URL: &str = "";
const VENDOR_EMAIL: &str = "";
const PLUGIN_VERSION: &str = "0.1.0";
const PLUGIN_SUBCATEGORIES: &str = "Fx";
const SDK_VERSION: &str = "VST 3";

fn copy_wstring(src: &str, dst: &mut [TChar]) {
    let mut len = 0;
    for (src, dst) in src.encode_utf16().zip(dst.iter_mut()) {
        *dst = src as TChar;
        len += 1;
    }

    if len < dst.len() {
        dst[len] = 0;
    } else if let Some(last) = dst.last_mut() {
        *last = 0;
    }
}

unsafe fn len_wstring(string: *const TChar) -> usize {
    let mut len = 0;
    while *string.offset(len) != 0 {
        len += 1;
    }
    len as usize
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn runtime_status_path(params: StreamParameters) -> std::path::PathBuf {
    let mode = match params.mode {
        StreamMode::Send => "send",
        StreamMode::Receive => "recv",
    };
    std::env::temp_dir().join(format!(
        "somethingnet-status-{}-{}-{}-{}-{}-{}-{}-{}.txt",
        std::process::id(),
        mode,
        params.port,
        params.channels,
        params.transport.as_u8(),
        params.ip[0],
        params.ip[1],
        u16::from_be_bytes([params.ip[2], params.ip[3]])
    ))
}

#[derive(Clone, Copy)]
struct RuntimeStatusSnapshot {
    params: StreamParameters,
    sample_rate_hz: u32,
    sender: SenderStatus,
    receiver: ReceiverStatus,
}

struct StatusWriter {
    tx: SyncSender<RuntimeStatusSnapshot>,
    shutdown: std::sync::Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl StatusWriter {
    fn new() -> Self {
        let (tx, rx) = sync_channel::<RuntimeStatusSnapshot>(32);
        let shutdown = std::sync::Arc::new(AtomicBool::new(false));
        let thread_shutdown = shutdown.clone();

        let join = thread::spawn(move || {
            loop {
                if thread_shutdown.load(Ordering::Relaxed) {
                    break;
                }

                match rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(snapshot) => {
                        let text = match snapshot.params.mode {
                            StreamMode::Send => sender_status_text(
                                snapshot.params,
                                snapshot.sample_rate_hz,
                                snapshot.sender,
                            ),
                            StreamMode::Receive => receiver_status_text(
                                snapshot.params,
                                snapshot.sample_rate_hz,
                                snapshot.receiver,
                            ),
                        };
                        let _ = fs::write(runtime_status_path(snapshot.params), text);
                    }
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        });

        Self {
            tx,
            shutdown,
            join: Some(join),
        }
    }

    fn try_send(&self, snapshot: RuntimeStatusSnapshot) {
        match self.tx.try_send(snapshot) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for StatusWriter {
    fn drop(&mut self) {
        self.stop();
    }
}

fn sender_status_text(
    params: StreamParameters,
    sample_rate_hz: u32,
    status: SenderStatus,
) -> String {
    let endpoint_label = params.endpoint_label();
    let transport = match params.transport {
        StreamTransport::Unicast => "unicast",
        StreamTransport::Multicast => "multicast",
    };
    format!(
        "mode=send\ntransport={}\nactive={}\nsample_rate={}\nchannels={}\n{}={}.{}.{}.{}:{}\npackets_sent={}\npackets_dropped={}\nqueued_frames={}\nwarning={}\n",
        transport,
        status.active as u8,
        sample_rate_hz,
        params.channels,
        endpoint_label,
        params.ip[0],
        params.ip[1],
        params.ip[2],
        params.ip[3],
        params.port,
        status.packets_sent,
        status.packets_dropped,
        status.queued_frames,
        network::warning_text(sample_rate_hz, params.channels),
    )
}

fn receiver_status_text(
    params: StreamParameters,
    sample_rate_hz: u32,
    status: ReceiverStatus,
) -> String {
    let endpoint_label = params.endpoint_label();
    let transport = match params.transport {
        StreamTransport::Unicast => "unicast",
        StreamTransport::Multicast => "multicast",
    };
    format!(
        "mode=receive\ntransport={}\nactive={}\nprimed={}\nsample_rate={}\nchannels={}\nlisten=0.0.0.0:{}\n{}={}.{}.{}.{}\npackets_received={}\npackets_dropped={}\npackets_invalid={}\npackets_invalid_header={}\npackets_invalid_format={}\npackets_invalid_frame_mismatch={}\nlast_invalid_samples={}\npackets_lost={}\npackets_out_of_order={}\nqueued_samples={}\ntarget_buffer_samples={}\nlast_callback_frames={}\nunderruns={}\ndrift_corrections={}\nwarning={}\n",
        transport,
        status.active as u8,
        status.primed as u8,
        sample_rate_hz,
        params.channels,
        params.port,
        endpoint_label,
        params.ip[0],
        params.ip[1],
        params.ip[2],
        params.ip[3],
        status.packets_received,
        status.packets_dropped,
        status.packets_invalid,
        status.packets_invalid_header,
        status.packets_invalid_format,
        status.packets_invalid_frame_mismatch,
        status.last_invalid_samples,
        status.packets_lost,
        status.packets_out_of_order,
        status.queued_samples,
        status.target_buffer_samples,
        status.last_callback_frames,
        status.underruns,
        status.drift_corrections,
        network::warning_text(sample_rate_hz, params.channels),
    )
}

unsafe fn read_state(stream: *mut IBStream) -> Option<StreamParameters> {
    // The host guarantees the IBStream pointer remains valid for the duration of this call.
    let stream = ComRef::from_raw(stream)?;
    let mut bytes = [0_u8; network::STATE_SIZE];
    let mut bytes_read = 0;
    let result = stream.read(
        bytes.as_mut_ptr().cast(),
        bytes.len() as i32,
        &mut bytes_read,
    );

    if result == kResultOk
        && (bytes_read == bytes.len() as i32 || bytes_read == LEGACY_STATE_SIZE as i32)
    {
        decode_state(&bytes[..bytes_read as usize])
    } else {
        None
    }
}

unsafe fn write_state(stream: *mut IBStream, state: StreamParameters) -> tresult {
    // The host guarantees the IBStream pointer remains valid for the duration of this call.
    let Some(stream) = ComRef::from_raw(stream) else {
        return kInvalidArgument;
    };

    let mut bytes = encode_state(state);
    let mut bytes_written = 0;
    let result = stream.write(
        bytes.as_mut_ptr().cast(),
        bytes.len() as i32,
        &mut bytes_written,
    );

    if result == kResultOk && bytes_written == bytes.len() as i32 {
        kResultOk
    } else {
        kResultFalse
    }
}

fn arrangement_channel_count(arrangement: SpeakerArrangement) -> i32 {
    arrangement.count_ones() as i32
}

fn speaker_arrangement_for_channels(channels: u8) -> SpeakerArrangement {
    match channels.clamp(1, MAX_CHANNELS as u8) {
        1 => SpeakerArr::kMono,
        2 => SpeakerArr::kStereo,
        3 => SpeakerArr::k30Music,
        4 => SpeakerArr::k40Music,
        5 => SpeakerArr::k50,
        6 => SpeakerArr::k51,
        7 => SpeakerArr::k70Cine,
        8 => SpeakerArr::k71Cine,
        n => ((1_u64 << n) - 1) as SpeakerArrangement,
    }
}

unsafe fn copy_input_to_output(
    input_buses: &[AudioBusBuffers],
    output_buses: &[AudioBusBuffers],
    num_samples: usize,
) {
    // VST3 supplies `channelBuffers32` as valid `numChannels` x `num_samples` buffers for this callback.
    let Some(output_bus) = output_buses.first() else {
        return;
    };

    let output_channels = slice::from_raw_parts_mut(
        output_bus.__field0.channelBuffers32,
        output_bus.numChannels as usize,
    );

    let input_channels = input_buses.first().map(|input_bus| {
        slice::from_raw_parts(
            input_bus.__field0.channelBuffers32,
            input_bus.numChannels as usize,
        )
    });

    let copy_channels = input_channels
        .map(|channels| channels.len().min(output_channels.len()))
        .unwrap_or(0);

    if let Some(input_channels) = input_channels {
        for channel_index in 0..copy_channels {
            let src = slice::from_raw_parts(input_channels[channel_index], num_samples);
            let dst = slice::from_raw_parts_mut(output_channels[channel_index], num_samples);
            dst.copy_from_slice(src);
        }
    }

    for output_channel in output_channels.iter().skip(copy_channels) {
        let dst = slice::from_raw_parts_mut(*output_channel, num_samples);
        dst.fill(0.0);
    }
}

unsafe fn fill_outputs_from_receiver(
    receiver: &NetworkReceiver,
    params: StreamParameters,
    sample_rate_hz: u32,
    output_buses: &[AudioBusBuffers],
    num_samples: usize,
) {
    // VST3 supplies `channelBuffers32` as valid `numChannels` x `num_samples` buffers for this callback.
    let Some(output_bus) = output_buses.first() else {
        return;
    };

    let output_channel_buffers = slice::from_raw_parts_mut(
        output_bus.__field0.channelBuffers32,
        output_bus.numChannels as usize,
    );
    let mut output_channels = std::array::from_fn(|_| None);

    for channel_index in 0..output_channel_buffers.len().min(MAX_CHANNELS) {
        output_channels[channel_index] = Some(slice::from_raw_parts_mut(
            output_channel_buffers[channel_index],
            num_samples,
        ));
    }

    receiver.pull_audio(params, sample_rate_hz, &mut output_channels, num_samples);
}

unsafe fn collect_source_channels<'a>(
    input_buses: &[AudioBusBuffers],
    output_buses: &[AudioBusBuffers],
    num_samples: usize,
) -> [Option<&'a [f32]>; MAX_CHANNELS] {
    // VST3 supplies `channelBuffers32` as valid `numChannels` x `num_samples` buffers for this callback.
    let mut source_channels: [Option<&[f32]>; MAX_CHANNELS] = [None; MAX_CHANNELS];

    let source_bus = input_buses
        .first()
        .filter(|bus| bus.numChannels > 0)
        .or_else(|| output_buses.first().filter(|bus| bus.numChannels > 0));

    let Some(source_bus) = source_bus else {
        return source_channels;
    };

    let channel_buffers = slice::from_raw_parts(
        source_bus.__field0.channelBuffers32,
        source_bus.numChannels as usize,
    );

    for channel_index in 0..channel_buffers.len().min(MAX_CHANNELS) {
        source_channels[channel_index] = Some(slice::from_raw_parts(
            channel_buffers[channel_index],
            num_samples,
        ));
    }

    source_channels
}

struct StreamProcessor {
    enabled: AtomicBool,
    mode: AtomicU32,
    transport: AtomicU32,
    channels: AtomicU32,
    port: AtomicU32,
    ip: [AtomicU32; 4],
    apply_seq: AtomicU32,
    input_arrangement: AtomicU64,
    output_arrangement: AtomicU64,
    sample_rate_hz: AtomicU32,
    last_status_write_ms: AtomicU64,
    status_writer: StatusWriter,
    sender: NetworkSender,
    receiver: NetworkReceiver,
}

impl Class for StreamProcessor {
    type Interfaces = (IComponent, IAudioProcessor, IProcessContextRequirements);
}

impl StreamProcessor {
    const CID: TUID = uid(0x6ACD6E1A, 0x4D0A4E20, 0xB3B29E42, 0x6F2F1791);

    fn new() -> Self {
        let defaults = default_stream_parameters();
        let default_arrangement = speaker_arrangement_for_channels(MAX_CHANNELS as u8);
        Self {
            enabled: AtomicBool::new(defaults.enabled),
            mode: AtomicU32::new(defaults.mode.as_u8() as u32),
            transport: AtomicU32::new(defaults.transport.as_u8() as u32),
            channels: AtomicU32::new(defaults.channels as u32),
            port: AtomicU32::new(defaults.port as u32),
            ip: defaults.ip.map(|octet| AtomicU32::new(octet as u32)),
            apply_seq: AtomicU32::new(0),
            input_arrangement: AtomicU64::new(default_arrangement),
            output_arrangement: AtomicU64::new(default_arrangement),
            sample_rate_hz: AtomicU32::new(48_000),
            last_status_write_ms: AtomicU64::new(0),
            status_writer: StatusWriter::new(),
            sender: NetworkSender::new(),
            receiver: NetworkReceiver::new(),
        }
    }

    fn parameters(&self) -> StreamParameters {
        StreamParameters {
            enabled: self.enabled.load(Ordering::Relaxed),
            mode: StreamMode::from_u32(self.mode.load(Ordering::Relaxed)),
            transport: StreamTransport::from_u32(self.transport.load(Ordering::Relaxed)),
            channels: self
                .channels
                .load(Ordering::Relaxed)
                .clamp(1, MAX_CHANNELS as u32) as u8,
            port: self.port.load(Ordering::Relaxed).clamp(1, u16::MAX as u32) as u16,
            ip: [
                self.ip[0].load(Ordering::Relaxed).clamp(0, 255) as u8,
                self.ip[1].load(Ordering::Relaxed).clamp(0, 255) as u8,
                self.ip[2].load(Ordering::Relaxed).clamp(0, 255) as u8,
                self.ip[3].load(Ordering::Relaxed).clamp(0, 255) as u8,
            ],
        }
    }

    fn apply_state(&self, state: StreamParameters) {
        self.enabled.store(state.enabled, Ordering::Relaxed);
        self.mode
            .store(state.mode.as_u8() as u32, Ordering::Relaxed);
        self.transport
            .store(state.transport.as_u8() as u32, Ordering::Relaxed);
        self.channels
            .store(state.channels as u32, Ordering::Relaxed);
        let arrangement = speaker_arrangement_for_channels(state.channels);
        self.input_arrangement.store(arrangement, Ordering::Relaxed);
        self.output_arrangement
            .store(arrangement, Ordering::Relaxed);
        self.port.store(state.port as u32, Ordering::Relaxed);
        for (atomic, value) in self.ip.iter().zip(state.ip) {
            atomic.store(value as u32, Ordering::Relaxed);
        }
    }

    fn apply_parameter_change(&self, id: u32, value: f64) {
        let Some(spec) = parameter_spec(id) else {
            return;
        };
        let plain = spec.normalized_to_plain(value);

        match id {
            PARAM_ENABLED => {
                self.enabled.store(plain >= 1, Ordering::Relaxed);
                self.sender.reset();
                self.receiver.reset();
            }
            PARAM_MODE => {
                self.mode.store(plain, Ordering::Relaxed);
                self.sender.reset();
                self.receiver.reset();
            }
            PARAM_TRANSPORT => {
                self.transport.store(plain, Ordering::Relaxed);
                self.sender.reset();
                self.receiver.reset();
            }
            PARAM_CHANNELS => {
                self.channels.store(plain, Ordering::Relaxed);
                let arrangement = speaker_arrangement_for_channels(plain as u8);
                self.input_arrangement.store(arrangement, Ordering::Relaxed);
                self.output_arrangement
                    .store(arrangement, Ordering::Relaxed);
                self.sender.reset();
                self.receiver.reset();
            }
            PARAM_PORT => self.port.store(plain, Ordering::Relaxed),
            PARAM_IP_1 => self.ip[0].store(plain, Ordering::Relaxed),
            PARAM_IP_2 => self.ip[1].store(plain, Ordering::Relaxed),
            PARAM_IP_3 => self.ip[2].store(plain, Ordering::Relaxed),
            PARAM_IP_4 => self.ip[3].store(plain, Ordering::Relaxed),
            PARAM_APPLY_SEQ => {
                self.apply_seq.store(plain, Ordering::Relaxed);
                self.sender.reset();
                self.receiver.reset();
            }
            _ => {}
        }
    }

    fn write_runtime_status(&self, params: StreamParameters, sample_rate_hz: u32) {
        let now = now_millis();
        let last = self.last_status_write_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) < 250 {
            return;
        }
        self.last_status_write_ms.store(now, Ordering::Relaxed);
        self.status_writer.try_send(RuntimeStatusSnapshot {
            params,
            sample_rate_hz,
            sender: self.sender.status_snapshot(),
            receiver: self.receiver.status_snapshot(),
        });
    }
}

impl IPluginBaseTrait for StreamProcessor {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }

    unsafe fn terminate(&self) -> tresult {
        self.sender.reset();
        self.receiver.reset();
        kResultOk
    }
}

impl IComponentTrait for StreamProcessor {
    unsafe fn getControllerClassId(&self, class_id: *mut TUID) -> tresult {
        *class_id = StreamController::CID;
        kResultOk
    }

    unsafe fn setIoMode(&self, _mode: IoMode) -> tresult {
        kResultOk
    }

    unsafe fn getBusCount(&self, mediaType: MediaType, dir: BusDirection) -> i32 {
        match mediaType as MediaTypes {
            MediaTypes_::kAudio => match dir as BusDirections {
                BusDirections_::kInput | BusDirections_::kOutput => 1,
                _ => 0,
            },
            _ => 0,
        }
    }

    unsafe fn getBusInfo(
        &self,
        mediaType: MediaType,
        dir: BusDirection,
        index: i32,
        bus: *mut BusInfo,
    ) -> tresult {
        if mediaType as MediaTypes != MediaTypes_::kAudio || index != 0 {
            return kInvalidArgument;
        }

        let bus = &mut *bus;
        let arrangement = match dir as BusDirections {
            BusDirections_::kInput => self.input_arrangement.load(Ordering::Relaxed),
            BusDirections_::kOutput => self.output_arrangement.load(Ordering::Relaxed),
            _ => return kInvalidArgument,
        };

        bus.mediaType = MediaTypes_::kAudio as MediaType;
        bus.direction = dir;
        bus.channelCount = arrangement_channel_count(arrangement);
        copy_wstring(
            if dir as BusDirections == BusDirections_::kInput {
                "Input"
            } else {
                "Output"
            },
            &mut bus.name,
        );
        bus.busType = BusTypes_::kMain as BusType;
        bus.flags = BusInfo_::BusFlags_::kDefaultActive;

        kResultOk
    }

    unsafe fn getRoutingInfo(
        &self,
        _in_info: *mut RoutingInfo,
        _out_info: *mut RoutingInfo,
    ) -> tresult {
        kNotImplemented
    }

    unsafe fn activateBus(
        &self,
        _media_type: MediaType,
        _dir: BusDirection,
        _index: i32,
        _state: TBool,
    ) -> tresult {
        kResultOk
    }

    unsafe fn setActive(&self, state: TBool) -> tresult {
        if state == 0 {
            self.sender.reset();
            self.receiver.reset();
        }
        kResultOk
    }

    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        let Some(decoded) = read_state(state) else {
            return kResultFalse;
        };
        self.apply_state(decoded);
        kResultOk
    }

    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        write_state(state, self.parameters())
    }
}

impl IAudioProcessorTrait for StreamProcessor {
    unsafe fn setBusArrangements(
        &self,
        inputs: *mut SpeakerArrangement,
        num_ins: i32,
        outputs: *mut SpeakerArrangement,
        num_outs: i32,
    ) -> tresult {
        if num_ins != 1 || num_outs != 1 {
            return kResultFalse;
        }

        let input_arrangement = *inputs;
        let output_arrangement = *outputs;
        let input_channels = arrangement_channel_count(input_arrangement);
        let output_channels = arrangement_channel_count(output_arrangement);

        if input_channels < 1
            || output_channels < 1
            || input_channels > MAX_CHANNELS as i32
            || output_channels > MAX_CHANNELS as i32
            || input_channels != output_channels
        {
            return kResultFalse;
        }

        self.input_arrangement
            .store(input_arrangement, Ordering::Relaxed);
        self.output_arrangement
            .store(output_arrangement, Ordering::Relaxed);

        kResultTrue
    }

    unsafe fn getBusArrangement(
        &self,
        dir: BusDirection,
        index: i32,
        arr: *mut SpeakerArrangement,
    ) -> tresult {
        if index != 0 {
            return kInvalidArgument;
        }

        match dir as BusDirections {
            BusDirections_::kInput => {
                *arr = self.input_arrangement.load(Ordering::Relaxed);
                kResultOk
            }
            BusDirections_::kOutput => {
                *arr = self.output_arrangement.load(Ordering::Relaxed);
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn canProcessSampleSize(&self, symbolic_sample_size: i32) -> tresult {
        match symbolic_sample_size as SymbolicSampleSizes {
            SymbolicSampleSizes_::kSample32 => kResultOk,
            SymbolicSampleSizes_::kSample64 => kNotImplemented,
            _ => kInvalidArgument,
        }
    }

    unsafe fn getLatencySamples(&self) -> u32 {
        0
    }

    unsafe fn setupProcessing(&self, setup: *mut ProcessSetup) -> tresult {
        let setup = &*setup;
        self.sample_rate_hz
            .store(setup.sampleRate.round() as u32, Ordering::Relaxed);
        kResultOk
    }

    unsafe fn setProcessing(&self, state: TBool) -> tresult {
        if state == 0 {
            self.sender.reset();
            self.receiver.reset();
        }
        kResultOk
    }

    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        let process_data = &*data;

        if let Some(param_changes) = ComRef::from_raw(process_data.inputParameterChanges) {
            let param_count = param_changes.getParameterCount();
            for param_index in 0..param_count {
                let Some(param_queue) =
                    ComRef::from_raw(param_changes.getParameterData(param_index))
                else {
                    continue;
                };

                let point_count = param_queue.getPointCount();
                if point_count == 0 {
                    continue;
                }

                let mut sample_offset = 0;
                let mut value = 0.0;
                let result = param_queue.getPoint(point_count - 1, &mut sample_offset, &mut value);
                if result == kResultTrue {
                    self.apply_parameter_change(param_queue.getParameterId(), value);
                }
            }
        }

        let num_samples = process_data.numSamples as usize;
        let input_buses =
            slice::from_raw_parts(process_data.inputs, process_data.numInputs as usize);
        let output_buses =
            slice::from_raw_parts(process_data.outputs, process_data.numOutputs as usize);
        let params = self.parameters();
        let sample_rate_hz = self.sample_rate_hz.load(Ordering::Relaxed);

        match params.mode {
            StreamMode::Send => {
                self.receiver.reset();
                copy_input_to_output(input_buses, output_buses, num_samples);
                let source_channels =
                    collect_source_channels(input_buses, output_buses, num_samples);

                self.sender
                    .push_audio(params, sample_rate_hz, &source_channels, num_samples);
            }
            StreamMode::Receive => {
                self.sender.reset();
                fill_outputs_from_receiver(
                    &self.receiver,
                    params,
                    sample_rate_hz,
                    output_buses,
                    num_samples,
                );
            }
        }

        self.write_runtime_status(params, sample_rate_hz);

        kResultOk
    }

    unsafe fn getTailSamples(&self) -> u32 {
        0
    }
}

impl IProcessContextRequirementsTrait for StreamProcessor {
    unsafe fn getProcessContextRequirements(&self) -> u32 {
        0
    }
}

struct StreamController {
    enabled: Cell<f64>,
    mode: Cell<f64>,
    transport: Cell<f64>,
    channels: Cell<f64>,
    port: Cell<f64>,
    ip: [Cell<f64>; 4],
    apply_seq: Cell<f64>,
    handler: RefCell<Option<ComPtr<IComponentHandler>>>,
}

impl Class for StreamController {
    type Interfaces = (IEditController,);
}

impl StreamController {
    const CID: TUID = uid(0x1B2D9E75, 0xABEF4BFC, 0x9B9D1A84, 0xB59F4092);

    fn new() -> Self {
        let defaults = default_stream_parameters();
        Self {
            enabled: Cell::new(
                parameter_spec(PARAM_ENABLED)
                    .unwrap()
                    .plain_to_normalized(defaults.enabled as u32),
            ),
            mode: Cell::new(
                parameter_spec(PARAM_MODE)
                    .unwrap()
                    .plain_to_normalized(defaults.mode.as_u8() as u32),
            ),
            transport: Cell::new(
                parameter_spec(PARAM_TRANSPORT)
                    .unwrap()
                    .plain_to_normalized(defaults.transport.as_u8() as u32),
            ),
            channels: Cell::new(
                parameter_spec(PARAM_CHANNELS)
                    .unwrap()
                    .plain_to_normalized(defaults.channels as u32),
            ),
            port: Cell::new(
                parameter_spec(PARAM_PORT)
                    .unwrap()
                    .plain_to_normalized(defaults.port as u32),
            ),
            ip: [
                Cell::new(
                    parameter_spec(PARAM_IP_1)
                        .unwrap()
                        .plain_to_normalized(defaults.ip[0] as u32),
                ),
                Cell::new(
                    parameter_spec(PARAM_IP_2)
                        .unwrap()
                        .plain_to_normalized(defaults.ip[1] as u32),
                ),
                Cell::new(
                    parameter_spec(PARAM_IP_3)
                        .unwrap()
                        .plain_to_normalized(defaults.ip[2] as u32),
                ),
                Cell::new(
                    parameter_spec(PARAM_IP_4)
                        .unwrap()
                        .plain_to_normalized(defaults.ip[3] as u32),
                ),
            ],
            apply_seq: Cell::new(0.0),
            handler: RefCell::new(None),
        }
    }

    fn parameters(&self) -> StreamParameters {
        StreamParameters {
            enabled: self.enabled.get() >= 0.5,
            mode: StreamMode::from_u32(
                parameter_spec(PARAM_MODE)
                    .unwrap()
                    .normalized_to_plain(self.mode.get()),
            ),
            transport: StreamTransport::from_u32(
                parameter_spec(PARAM_TRANSPORT)
                    .unwrap()
                    .normalized_to_plain(self.transport.get()),
            ),
            channels: parameter_spec(PARAM_CHANNELS)
                .unwrap()
                .normalized_to_plain(self.channels.get()) as u8,
            port: parameter_spec(PARAM_PORT)
                .unwrap()
                .normalized_to_plain(self.port.get()) as u16,
            ip: [
                parameter_spec(PARAM_IP_1)
                    .unwrap()
                    .normalized_to_plain(self.ip[0].get()) as u8,
                parameter_spec(PARAM_IP_2)
                    .unwrap()
                    .normalized_to_plain(self.ip[1].get()) as u8,
                parameter_spec(PARAM_IP_3)
                    .unwrap()
                    .normalized_to_plain(self.ip[2].get()) as u8,
                parameter_spec(PARAM_IP_4)
                    .unwrap()
                    .normalized_to_plain(self.ip[3].get()) as u8,
            ],
        }
    }

    pub(crate) fn runtime_status_lines(&self) -> [String; 4] {
        let params = self.parameters();
        let path = runtime_status_path(params);
        let Ok(content) = fs::read_to_string(path) else {
            return [
                "Runtime: waiting for audio callback".to_string(),
                format!(
                    "Mode={} Transport={} Ch={} Port={}",
                    match params.mode {
                        StreamMode::Send => "Send",
                        StreamMode::Receive => "Receive",
                    },
                    match params.transport {
                        StreamTransport::Unicast => "Unicast",
                        StreamTransport::Multicast => "Multicast",
                    },
                    params.channels,
                    params.port
                ),
                format!(
                    "{}={}.{}.{}.{}",
                    params.endpoint_label(),
                    params.ip[0],
                    params.ip[1],
                    params.ip[2],
                    params.ip[3]
                ),
                "Counters will appear once the host is actively processing audio.".to_string(),
            ];
        };

        let mut mode = String::new();
        let mut transport = String::new();
        let mut active = String::new();
        let mut primed = String::new();
        let mut sample_rate = String::new();
        let mut channels = String::new();
        let mut endpoint = String::new();
        let mut packets_received = String::new();
        let mut packets_sent = String::new();
        let mut packets_dropped = String::new();
        let mut packets_invalid = String::new();
        let mut packets_invalid_header = String::new();
        let mut packets_invalid_format = String::new();
        let mut packets_invalid_frame_mismatch = String::new();
        let mut last_invalid_samples = String::new();
        let mut packets_lost = String::new();
        let mut packets_out_of_order = String::new();
        let mut queued = String::new();
        let mut target_buffer = String::new();
        let mut last_callback_frames = String::new();
        let mut underruns = String::new();
        let mut drift_corrections = String::new();
        let mut warning = String::new();

        for line in content.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key {
                "mode" => mode = value.to_string(),
                "transport" => transport = value.to_string(),
                "active" => active = value.to_string(),
                "primed" => primed = value.to_string(),
                "sample_rate" => sample_rate = value.to_string(),
                "channels" => channels = value.to_string(),
                "destination" | "group" | "expected_source" | "listen" => {
                    endpoint = value.to_string()
                }
                "packets_received" => packets_received = value.to_string(),
                "packets_sent" => packets_sent = value.to_string(),
                "packets_dropped" => packets_dropped = value.to_string(),
                "packets_invalid" => packets_invalid = value.to_string(),
                "packets_invalid_header" => packets_invalid_header = value.to_string(),
                "packets_invalid_format" => packets_invalid_format = value.to_string(),
                "packets_invalid_frame_mismatch" => {
                    packets_invalid_frame_mismatch = value.to_string()
                }
                "last_invalid_samples" => last_invalid_samples = value.to_string(),
                "packets_lost" => packets_lost = value.to_string(),
                "packets_out_of_order" => packets_out_of_order = value.to_string(),
                "queued_samples" | "queued_frames" => queued = value.to_string(),
                "target_buffer_samples" => target_buffer = value.to_string(),
                "last_callback_frames" => last_callback_frames = value.to_string(),
                "underruns" => underruns = value.to_string(),
                "drift_corrections" => drift_corrections = value.to_string(),
                "warning" => warning = value.to_string(),
                _ => {}
            }
        }

        if mode == "send" {
            [
                format!(
                    "Runtime: SEND active={} sr={} ch={} endpoint={}",
                    active, sample_rate, channels, endpoint
                ),
                format!("Transport={}", transport),
                format!(
                    "Packets sent={} dropped={} packet_fill_frames={}",
                    packets_sent, packets_dropped, queued
                ),
                if warning.is_empty() {
                    "Receiver-side counters are not shown here. Match channels and sample rate exactly.".to_string()
                } else {
                    warning
                },
            ]
        } else {
            [
                format!(
                    "Recv a={} p={} t={} sr={} ch={} ep={}",
                    active, primed, transport, sample_rate, channels, endpoint
                ),
                format!(
                    "rx={} drop={} bad={} lost={} ooo={}",
                    packets_received,
                    packets_dropped,
                    packets_invalid,
                    packets_lost,
                    packets_out_of_order
                ),
                if warning.is_empty() {
                    format!(
                        "q={} tgt={} cb={} underrun={} adj={}",
                        queued, target_buffer, last_callback_frames, underruns, drift_corrections
                    )
                } else {
                    warning
                },
                format!(
                    "bad_hdr={} bad_fmt={} bad_frames={} last_bad_samples={}",
                    packets_invalid_header,
                    packets_invalid_format,
                    packets_invalid_frame_mismatch,
                    last_invalid_samples
                ),
            ]
        }
    }

    fn apply_state(&self, state: StreamParameters) {
        self.enabled.set(
            parameter_spec(PARAM_ENABLED)
                .unwrap()
                .plain_to_normalized(state.enabled as u32),
        );
        self.mode.set(
            parameter_spec(PARAM_MODE)
                .unwrap()
                .plain_to_normalized(state.mode.as_u8() as u32),
        );
        self.transport.set(
            parameter_spec(PARAM_TRANSPORT)
                .unwrap()
                .plain_to_normalized(state.transport.as_u8() as u32),
        );
        self.channels.set(
            parameter_spec(PARAM_CHANNELS)
                .unwrap()
                .plain_to_normalized(state.channels as u32),
        );
        self.port.set(
            parameter_spec(PARAM_PORT)
                .unwrap()
                .plain_to_normalized(state.port as u32),
        );
        self.ip[0].set(
            parameter_spec(PARAM_IP_1)
                .unwrap()
                .plain_to_normalized(state.ip[0] as u32),
        );
        self.ip[1].set(
            parameter_spec(PARAM_IP_2)
                .unwrap()
                .plain_to_normalized(state.ip[1] as u32),
        );
        self.ip[2].set(
            parameter_spec(PARAM_IP_3)
                .unwrap()
                .plain_to_normalized(state.ip[2] as u32),
        );
        self.ip[3].set(
            parameter_spec(PARAM_IP_4)
                .unwrap()
                .plain_to_normalized(state.ip[3] as u32),
        );
    }

    fn cell_for_param(&self, id: u32) -> Option<&Cell<f64>> {
        match id {
            PARAM_ENABLED => Some(&self.enabled),
            PARAM_MODE => Some(&self.mode),
            PARAM_TRANSPORT => Some(&self.transport),
            PARAM_CHANNELS => Some(&self.channels),
            PARAM_PORT => Some(&self.port),
            PARAM_IP_1 => Some(&self.ip[0]),
            PARAM_IP_2 => Some(&self.ip[1]),
            PARAM_IP_3 => Some(&self.ip[2]),
            PARAM_IP_4 => Some(&self.ip[3]),
            PARAM_APPLY_SEQ => Some(&self.apply_seq),
            _ => None,
        }
    }

    fn apply_ui_parameter(&self, id: u32, normalized: f64) {
        let Some(cell) = self.cell_for_param(id) else {
            return;
        };

        let normalized = normalized.clamp(0.0, 1.0);
        cell.set(normalized);

        if let Some(handler) = self.handler.borrow().as_ref() {
            unsafe {
                handler.beginEdit(id);
                handler.performEdit(id, normalized);
                handler.endEdit(id);
                let restart_flags = if id == PARAM_CHANNELS {
                    RestartFlags_::kParamValuesChanged | RestartFlags_::kIoChanged
                } else {
                    RestartFlags_::kParamValuesChanged
                };
                handler.restartComponent(restart_flags);
            }
        }
    }

    fn trigger_apply_reset(&self) {
        let spec = parameter_spec(PARAM_APPLY_SEQ).unwrap();
        let current = spec.normalized_to_plain(self.apply_seq.get());
        let next = if current >= spec.max {
            spec.min
        } else {
            current + 1
        };
        self.apply_ui_parameter(PARAM_APPLY_SEQ, spec.plain_to_normalized(next));
    }
}

#[cfg(target_os = "macos")]
unsafe fn editor_controller_parameters(controller: *const c_void) -> StreamParameters {
    (&*(controller as *const StreamController)).parameters()
}

#[cfg(target_os = "macos")]
unsafe fn editor_controller_apply_ui_parameter(
    controller: *const c_void,
    id: u32,
    normalized: f64,
) {
    (&*(controller as *const StreamController)).apply_ui_parameter(id, normalized);
}

#[cfg(target_os = "macos")]
unsafe fn editor_controller_trigger_apply_reset(controller: *const c_void) {
    (&*(controller as *const StreamController)).trigger_apply_reset();
}

#[cfg(target_os = "macos")]
unsafe fn editor_controller_runtime_status_lines(controller: *const c_void) -> [String; 4] {
    (&*(controller as *const StreamController)).runtime_status_lines()
}

#[cfg(target_os = "macos")]
fn editor_controller_api(controller: *const StreamController) -> EditorControllerApi {
    EditorControllerApi {
        controller: controller.cast(),
        parameters: editor_controller_parameters,
        apply_ui_parameter: editor_controller_apply_ui_parameter,
        trigger_apply_reset: editor_controller_trigger_apply_reset,
        runtime_status_lines: editor_controller_runtime_status_lines,
    }
}

impl IPluginBaseTrait for StreamController {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }

    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl IEditControllerTrait for StreamController {
    unsafe fn setComponentState(&self, state: *mut IBStream) -> tresult {
        let Some(decoded) = read_state(state) else {
            return kResultFalse;
        };
        self.apply_state(decoded);
        kResultOk
    }

    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        let Some(decoded) = read_state(state) else {
            return kResultFalse;
        };
        self.apply_state(decoded);
        kResultOk
    }

    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        write_state(state, self.parameters())
    }

    unsafe fn getParameterCount(&self) -> i32 {
        PARAM_COUNT
    }

    unsafe fn getParameterInfo(&self, param_index: i32, info: *mut ParameterInfo) -> tresult {
        let Some(spec) = parameter_spec(param_index as u32) else {
            return kInvalidArgument;
        };

        let info = &mut *info;
        info.id = spec.id;
        copy_wstring(spec.title, &mut info.title);
        copy_wstring(spec.short_title, &mut info.shortTitle);
        copy_wstring(spec.units, &mut info.units);
        info.stepCount = spec.step_count();
        info.defaultNormalizedValue = spec.plain_to_normalized(spec.default);
        info.unitId = 0;
        info.flags = if spec.id == PARAM_APPLY_SEQ {
            ParameterInfo_::ParameterFlags_::kCanAutomate
                | ParameterInfo_::ParameterFlags_::kIsHidden
        } else {
            ParameterInfo_::ParameterFlags_::kCanAutomate
        };

        kResultOk
    }

    unsafe fn getParamStringByValue(
        &self,
        id: u32,
        value_normalized: f64,
        string: *mut String128,
    ) -> tresult {
        let Some(spec) = parameter_spec(id) else {
            return kInvalidArgument;
        };

        let value = spec.normalized_to_plain(value_normalized);
        let display = match id {
            PARAM_ENABLED => {
                if value >= 1 {
                    "On".to_string()
                } else {
                    "Off".to_string()
                }
            }
            PARAM_MODE => {
                if value >= 1 {
                    "Receive".to_string()
                } else {
                    "Send".to_string()
                }
            }
            PARAM_TRANSPORT => {
                if value >= 1 {
                    "Multicast".to_string()
                } else {
                    "Unicast".to_string()
                }
            }
            _ => value.to_string(),
        };

        copy_wstring(&display, &mut *string);
        kResultOk
    }

    unsafe fn getParamValueByString(
        &self,
        id: u32,
        string: *mut TChar,
        value_normalized: *mut f64,
    ) -> tresult {
        let Some(spec) = parameter_spec(id) else {
            return kInvalidArgument;
        };

        let len = len_wstring(string as *const TChar);
        let Ok(text) = String::from_utf16(slice::from_raw_parts(string as *const u16, len)) else {
            return kInvalidArgument;
        };
        let text = text.trim();

        let plain_value = match id {
            PARAM_ENABLED => match text.to_ascii_lowercase().as_str() {
                "on" | "1" | "true" => 1,
                "off" | "0" | "false" => 0,
                _ => return kInvalidArgument,
            },
            PARAM_MODE => match text.to_ascii_lowercase().as_str() {
                "send" | "sender" | "0" => 0,
                "receive" | "receiver" | "recv" | "1" => 1,
                _ => return kInvalidArgument,
            },
            PARAM_TRANSPORT => match text.to_ascii_lowercase().as_str() {
                "unicast" | "uni" | "0" => 0,
                "multicast" | "multi" | "1" => 1,
                _ => return kInvalidArgument,
            },
            _ => match u32::from_str(text) {
                Ok(value) => spec.clamp(value),
                Err(_) => return kInvalidArgument,
            },
        };

        *value_normalized = spec.plain_to_normalized(plain_value);
        kResultOk
    }

    unsafe fn normalizedParamToPlain(&self, id: u32, value_normalized: f64) -> f64 {
        parameter_spec(id)
            .map(|spec| spec.normalized_to_plain(value_normalized) as f64)
            .unwrap_or(0.0)
    }

    unsafe fn plainParamToNormalized(&self, id: u32, plain_value: f64) -> f64 {
        parameter_spec(id)
            .map(|spec| spec.plain_to_normalized(plain_value.round().max(0.0) as u32))
            .unwrap_or(0.0)
    }

    unsafe fn getParamNormalized(&self, id: u32) -> f64 {
        self.cell_for_param(id)
            .map(|cell| cell.get())
            .unwrap_or(0.0)
    }

    unsafe fn setParamNormalized(&self, id: u32, value: f64) -> tresult {
        let Some(cell) = self.cell_for_param(id) else {
            return kInvalidArgument;
        };

        cell.set(value.clamp(0.0, 1.0));
        kResultOk
    }

    unsafe fn setComponentHandler(&self, _handler: *mut IComponentHandler) -> tresult {
        *self.handler.borrow_mut() = ComRef::from_raw(_handler).map(|handler| handler.to_com_ptr());
        kResultOk
    }

    unsafe fn createView(&self, _name: *const c_char) -> *mut IPlugView {
        #[cfg(target_os = "macos")]
        {
            return macos_gui::create_editor_view(editor_controller_api(self as *const Self));
        }

        #[allow(unreachable_code)]
        ptr::null_mut()
    }
}

struct Factory;

impl Class for Factory {
    type Interfaces = (IPluginFactory3,);
}

impl IPluginFactoryTrait for Factory {
    unsafe fn getFactoryInfo(&self, info: *mut PFactoryInfo) -> tresult {
        let info = &mut *info;
        copy_cstring(VENDOR_NAME, &mut info.vendor);
        copy_cstring(VENDOR_URL, &mut info.url);
        copy_cstring(VENDOR_EMAIL, &mut info.email);
        info.flags = PFactoryInfo_::FactoryFlags_::kUnicode as int32;
        kResultOk
    }

    unsafe fn countClasses(&self) -> i32 {
        2
    }

    unsafe fn getClassInfo(&self, index: i32, info: *mut PClassInfo) -> tresult {
        let info = &mut *info;
        match index {
            0 => {
                info.cid = StreamProcessor::CID;
                info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
                copy_cstring("Audio Module Class", &mut info.category);
                copy_cstring(PLUGIN_NAME, &mut info.name);
                kResultOk
            }
            1 => {
                info.cid = StreamController::CID;
                info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
                copy_cstring("Component Controller Class", &mut info.category);
                copy_cstring(PLUGIN_NAME, &mut info.name);
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn createInstance(
        &self,
        cid: FIDString,
        iid: FIDString,
        obj: *mut *mut c_void,
    ) -> tresult {
        let instance = match *(cid as *const TUID) {
            StreamProcessor::CID => Some(
                ComWrapper::new(StreamProcessor::new())
                    .to_com_ptr::<FUnknown>()
                    .unwrap(),
            ),
            StreamController::CID => Some(
                ComWrapper::new(StreamController::new())
                    .to_com_ptr::<FUnknown>()
                    .unwrap(),
            ),
            _ => None,
        };

        if let Some(instance) = instance {
            let ptr = instance.as_ptr();
            ((*(*ptr).vtbl).queryInterface)(ptr, iid as *mut TUID, obj)
        } else {
            kInvalidArgument
        }
    }
}

impl IPluginFactory2Trait for Factory {
    unsafe fn getClassInfo2(&self, index: i32, info: *mut PClassInfo2) -> tresult {
        let info = &mut *info;
        match index {
            0 => {
                info.cid = StreamProcessor::CID;
                info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
                copy_cstring("Audio Module Class", &mut info.category);
                copy_cstring(PLUGIN_NAME, &mut info.name);
                info.classFlags = 0;
                copy_cstring(PLUGIN_SUBCATEGORIES, &mut info.subCategories);
                copy_cstring(VENDOR_NAME, &mut info.vendor);
                copy_cstring(PLUGIN_VERSION, &mut info.version);
                copy_cstring(SDK_VERSION, &mut info.sdkVersion);
                kResultOk
            }
            1 => {
                info.cid = StreamController::CID;
                info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
                copy_cstring("Component Controller Class", &mut info.category);
                copy_cstring(PLUGIN_NAME, &mut info.name);
                info.classFlags = 0;
                copy_cstring("", &mut info.subCategories);
                copy_cstring(VENDOR_NAME, &mut info.vendor);
                copy_cstring(PLUGIN_VERSION, &mut info.version);
                copy_cstring(SDK_VERSION, &mut info.sdkVersion);
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }
}

impl IPluginFactory3Trait for Factory {
    unsafe fn getClassInfoUnicode(&self, index: i32, info: *mut PClassInfoW) -> tresult {
        let info = &mut *info;
        match index {
            0 => {
                info.cid = StreamProcessor::CID;
                info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
                copy_cstring("Audio Module Class", &mut info.category);
                copy_wstring(PLUGIN_NAME, &mut info.name);
                info.classFlags = 0;
                copy_cstring(PLUGIN_SUBCATEGORIES, &mut info.subCategories);
                copy_wstring(VENDOR_NAME, &mut info.vendor);
                copy_wstring(PLUGIN_VERSION, &mut info.version);
                copy_wstring(SDK_VERSION, &mut info.sdkVersion);
                kResultOk
            }
            1 => {
                info.cid = StreamController::CID;
                info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
                copy_cstring("Component Controller Class", &mut info.category);
                copy_wstring(PLUGIN_NAME, &mut info.name);
                info.classFlags = 0;
                copy_cstring("", &mut info.subCategories);
                copy_wstring(VENDOR_NAME, &mut info.vendor);
                copy_wstring(PLUGIN_VERSION, &mut info.version);
                copy_wstring(SDK_VERSION, &mut info.sdkVersion);
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn setHostContext(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }
}

#[cfg(target_os = "windows")]
#[unsafe(no_mangle)]
extern "system" fn InitDll() -> bool {
    true
}

#[cfg(target_os = "windows")]
#[unsafe(no_mangle)]
extern "system" fn ExitDll() -> bool {
    true
}

#[cfg(target_os = "macos")]
#[unsafe(no_mangle)]
extern "C" fn bundleEntry(_bundle_ref: *mut c_void) -> bool {
    true
}

#[cfg(target_os = "macos")]
#[unsafe(no_mangle)]
extern "C" fn bundleExit() -> bool {
    true
}

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
extern "system" fn ModuleEntry(_library_handle: *mut c_void) -> bool {
    true
}

#[cfg(target_os = "linux")]
#[unsafe(no_mangle)]
extern "system" fn ModuleExit() -> bool {
    true
}

#[unsafe(no_mangle)]
extern "system" fn GetPluginFactory() -> *mut IPluginFactory {
    ComWrapper::new(Factory)
        .to_com_ptr::<IPluginFactory>()
        .unwrap()
        .into_raw()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_reset_token_clears_sender_state() {
        let processor = StreamProcessor::new();
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

        processor
            .sender
            .push_audio(params, 48_000, &input_channels, 24);
        assert_eq!(processor.sender.status_snapshot().queued_frames, 24);

        let spec = parameter_spec(PARAM_APPLY_SEQ).unwrap();
        processor.apply_parameter_change(PARAM_APPLY_SEQ, spec.plain_to_normalized(1));

        assert_eq!(processor.sender.status_snapshot().queued_frames, 0);
    }
}
