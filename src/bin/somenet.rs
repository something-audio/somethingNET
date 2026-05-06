use std::error::Error;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, FromSample, Sample, SampleFormat, SizedSample};
use eframe::egui;
use somenet_vst3::network::{
    ClockReference, MAX_CHANNELS, NetworkReceiver, NetworkSender, StreamMode, StreamParameters,
    StreamTransport, warning_text,
};

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;
const DEFAULT_PREALLOCATED_CALLBACK_FRAMES: usize = 2048;
const MAX_PREALLOCATED_CALLBACK_FRAMES: usize = 16_384;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Run,
    Ui,
    Devices,
    Help,
}

#[derive(Clone, Debug)]
struct AppConfig {
    command: Command,
    mode: StreamMode,
    transport: StreamTransport,
    channels: u8,
    port: u16,
    ip: [u8; 4],
    clock_reference: ClockReference,
    ptp_domain: u8,
    sample_rate_hz: u32,
    input_device: Option<String>,
    output_device: Option<String>,
    duration: Option<Duration>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            command: Command::Run,
            mode: StreamMode::Receive,
            transport: StreamTransport::Unicast,
            channels: 2,
            port: 5004,
            ip: [127, 0, 0, 1],
            clock_reference: ClockReference::Local,
            ptp_domain: 0,
            sample_rate_hz: 48_000,
            input_device: None,
            output_device: None,
            duration: None,
        }
    }
}

impl AppConfig {
    fn stream_parameters(&self) -> StreamParameters {
        StreamParameters {
            enabled: true,
            mode: self.mode,
            transport: self.transport,
            channels: self.channels,
            port: self.port,
            ip: self.ip,
            clock_reference: self.clock_reference,
            ptp_domain: self.ptp_domain,
        }
    }
}

fn main() -> AppResult<()> {
    let config = parse_args(std::env::args().skip(1))?;
    match config.command {
        Command::Help => {
            print_help();
            Ok(())
        }
        Command::Devices => list_devices(),
        Command::Ui => run_ui(config),
        Command::Run => run_app(config),
    }
}

fn run_app(config: AppConfig) -> AppResult<()> {
    let running = Arc::new(AtomicBool::new(true));
    let signal_running = running.clone();
    ctrlc::set_handler(move || {
        signal_running.store(false, Ordering::SeqCst);
    })?;

    match config.mode {
        StreamMode::Send => run_send(config, running),
        StreamMode::Receive => run_receive(config, running),
    }
}

fn run_send(config: AppConfig, running: Arc<AtomicBool>) -> AppResult<()> {
    let host = cpal::default_host();
    let device = select_device(
        &host,
        config.input_device.as_deref(),
        DeviceDirection::Input,
    )?;
    let supported_config = select_config(
        &device,
        DeviceDirection::Input,
        config.channels,
        config.sample_rate_hz,
    )?;
    let sample_format = supported_config.sample_format();
    let stream_config: cpal::StreamConfig = supported_config.into();
    validate_sample_rate(stream_config.sample_rate)?;

    let sender = Arc::new(NetworkSender::new());
    let params = config.stream_parameters();
    let device_label = device_label(&device);
    let stream = build_sender_stream(
        &device,
        sample_format,
        stream_config.clone(),
        sender.clone(),
        params,
    )?;

    print_launch_header("send", &device_label, &stream_config, params);
    stream.play()?;
    wait_for_shutdown(config.duration, running, || {
        let status = sender.status_snapshot();
        println!(
            "send   active={} packets={} dropped={} queued_frames={}",
            status.active as u8, status.packets_sent, status.packets_dropped, status.queued_frames
        );
    });
    drop(stream);
    sender.reset();
    Ok(())
}

fn run_receive(config: AppConfig, running: Arc<AtomicBool>) -> AppResult<()> {
    let host = cpal::default_host();
    let device = select_device(
        &host,
        config.output_device.as_deref(),
        DeviceDirection::Output,
    )?;
    let supported_config = select_config(
        &device,
        DeviceDirection::Output,
        config.channels,
        config.sample_rate_hz,
    )?;
    let sample_format = supported_config.sample_format();
    let stream_config: cpal::StreamConfig = supported_config.into();
    validate_sample_rate(stream_config.sample_rate)?;

    let receiver = Arc::new(NetworkReceiver::new());
    let params = config.stream_parameters();
    let device_label = device_label(&device);
    let stream = build_receiver_stream(
        &device,
        sample_format,
        stream_config.clone(),
        receiver.clone(),
        params,
    )?;

    print_launch_header("receive", &device_label, &stream_config, params);
    stream.play()?;
    wait_for_shutdown(config.duration, running, || {
        let status = receiver.status_snapshot();
        println!(
            "recv   active={} primed={} packets={} lost={} underruns={} queued={} target={}",
            status.active as u8,
            status.primed as u8,
            status.packets_received,
            status.packets_lost,
            status.underruns,
            status.queued_samples,
            status.target_buffer_samples
        );
    });
    drop(stream);
    receiver.reset();
    Ok(())
}

enum RuntimeEngine {
    Sender(Arc<NetworkSender>),
    Receiver(Arc<NetworkReceiver>),
}

struct RunningRuntime {
    _stream: cpal::Stream,
    engine: RuntimeEngine,
    started_at: Instant,
    device_label: String,
    stream_config: cpal::StreamConfig,
    params: StreamParameters,
}

impl RunningRuntime {
    fn start(config: &AppConfig) -> AppResult<Self> {
        let host = cpal::default_host();
        let params = config.stream_parameters();

        match config.mode {
            StreamMode::Send => {
                let device = select_device(
                    &host,
                    config.input_device.as_deref(),
                    DeviceDirection::Input,
                )?;
                let supported_config = select_config(
                    &device,
                    DeviceDirection::Input,
                    config.channels,
                    config.sample_rate_hz,
                )?;
                let sample_format = supported_config.sample_format();
                let stream_config: cpal::StreamConfig = supported_config.into();
                validate_sample_rate(stream_config.sample_rate)?;
                let sender = Arc::new(NetworkSender::new());
                let stream = build_sender_stream(
                    &device,
                    sample_format,
                    stream_config.clone(),
                    sender.clone(),
                    params,
                )?;
                stream.play()?;

                Ok(Self {
                    _stream: stream,
                    engine: RuntimeEngine::Sender(sender),
                    started_at: Instant::now(),
                    device_label: device_label(&device),
                    stream_config,
                    params,
                })
            }
            StreamMode::Receive => {
                let device = select_device(
                    &host,
                    config.output_device.as_deref(),
                    DeviceDirection::Output,
                )?;
                let supported_config = select_config(
                    &device,
                    DeviceDirection::Output,
                    config.channels,
                    config.sample_rate_hz,
                )?;
                let sample_format = supported_config.sample_format();
                let stream_config: cpal::StreamConfig = supported_config.into();
                validate_sample_rate(stream_config.sample_rate)?;
                let receiver = Arc::new(NetworkReceiver::new());
                let stream = build_receiver_stream(
                    &device,
                    sample_format,
                    stream_config.clone(),
                    receiver.clone(),
                    params,
                )?;
                stream.play()?;

                Ok(Self {
                    _stream: stream,
                    engine: RuntimeEngine::Receiver(receiver),
                    started_at: Instant::now(),
                    device_label: device_label(&device),
                    stream_config,
                    params,
                })
            }
        }
    }

    fn reset_engine(&self) {
        match &self.engine {
            RuntimeEngine::Sender(sender) => sender.reset(),
            RuntimeEngine::Receiver(receiver) => receiver.reset(),
        }
    }
}

impl Drop for RunningRuntime {
    fn drop(&mut self) {
        self.reset_engine();
    }
}

fn run_ui(config: AppConfig) -> AppResult<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SomeNET")
            .with_inner_size([620.0, 660.0])
            .with_min_inner_size([420.0, 360.0])
            .with_resizable(true),
        ..Default::default()
    };

    eframe::run_native(
        "SomeNET",
        options,
        Box::new(move |creation_context| {
            Ok(Box::new(SomeNetNativeApp::new(
                config.clone(),
                creation_context,
            )))
        }),
    )
    .map_err(|err| format!("failed to launch native UI: {err}").into())
}

#[derive(Clone, Debug)]
struct DeviceChoice {
    id: String,
    name: String,
    input: bool,
    output: bool,
    input_default: String,
    output_default: String,
}

impl DeviceChoice {
    fn query(&self) -> &str {
        if self.id.is_empty() {
            &self.name
        } else {
            &self.id
        }
    }

    fn matches_query(&self, query: &str) -> bool {
        self.id == query || self.name == query
    }
}

struct SomeNetNativeApp {
    config: AppConfig,
    runtime: Option<RunningRuntime>,
    last_error: Option<String>,
    active_tab: SomeNetTab,
    ip_text: String,
    host_name: String,
    default_input: String,
    default_output: String,
    devices: Vec<DeviceChoice>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SomeNetTab {
    Route,
    Monitor,
    Devices,
}

impl SomeNetNativeApp {
    fn new(mut config: AppConfig, creation_context: &eframe::CreationContext<'_>) -> Self {
        configure_native_style(&creation_context.egui_ctx);
        config.command = Command::Ui;
        config.duration = None;
        let ip_text = Ipv4Addr::from(config.ip).to_string();
        let mut app = Self {
            config,
            runtime: None,
            last_error: None,
            active_tab: SomeNetTab::Route,
            ip_text,
            host_name: String::new(),
            default_input: String::from("none"),
            default_output: String::from("none"),
            devices: Vec::new(),
        };
        app.refresh_devices();
        app
    }

    fn start_runtime(&mut self) {
        match parse_ipv4(&self.ip_text) {
            Ok(ip) => self.config.ip = ip,
            Err(err) => {
                self.last_error = Some(format!("invalid endpoint IP: {err}"));
                return;
            }
        }

        self.runtime = None;
        match RunningRuntime::start(&self.config) {
            Ok(runtime) => {
                self.runtime = Some(runtime);
                self.last_error = None;
            }
            Err(err) => {
                self.last_error = Some(err.to_string());
            }
        }
    }

    fn stop_runtime(&mut self) {
        self.runtime = None;
    }

    fn refresh_devices(&mut self) {
        let host = cpal::default_host();
        self.host_name = host.id().name().to_string();
        self.default_input = host
            .default_input_device()
            .map(|device| device_label(&device))
            .unwrap_or_else(|| "none".to_string());
        self.default_output = host
            .default_output_device()
            .map(|device| device_label(&device))
            .unwrap_or_else(|| "none".to_string());
        self.devices.clear();

        match host.devices() {
            Ok(devices) => {
                for device in devices {
                    let id = device
                        .id()
                        .ok()
                        .map(|id| id.to_string())
                        .unwrap_or_default();
                    let input_default = device
                        .default_input_config()
                        .ok()
                        .map(|config| format!("{config:?}"))
                        .unwrap_or_default();
                    let output_default = device
                        .default_output_config()
                        .ok()
                        .map(|config| format!("{config:?}"))
                        .unwrap_or_default();
                    self.devices.push(DeviceChoice {
                        id,
                        name: device_label(&device),
                        input: !input_default.is_empty(),
                        output: !output_default.is_empty(),
                        input_default,
                        output_default,
                    });
                }
            }
            Err(err) => {
                self.last_error = Some(format!("failed to enumerate audio devices: {err}"));
            }
        }
    }

    fn render_top_bar(&self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new("SomeNET")
                    .font(egui::FontId::new(28.0, egui::FontFamily::Proportional))
                    .strong()
                    .color(color_text()),
            );
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new("RTP/L24 NETWORK AUDIO")
                    .font(egui::FontId::new(12.0, egui::FontFamily::Monospace))
                    .color(color_muted()),
            );
            ui.add_space(8.0);
            let (label, color) = if self.runtime.is_some() {
                ("STREAMING", color_green())
            } else {
                ("STANDBY", color_muted())
            };
            let (rect, _) =
                ui.allocate_exact_size(egui::Vec2::new(10.0, 10.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 4.0, color);
            ui.label(
                egui::RichText::new(label)
                    .font(egui::FontId::new(12.0, egui::FontFamily::Monospace))
                    .color(color),
            );
        });
    }

    fn render_transport_lane(&self, ui: &mut egui::Ui) {
        panel_frame().show(ui, |ui| {
            let accent = mode_accent(self.config.mode);
            if ui.available_width() < 700.0 {
                let node_width = ui.available_width();
                lane_node(
                    ui,
                    "INPUT RIG",
                    self.input_label(),
                    self.config.mode == StreamMode::Send,
                    node_width,
                );
                compact_lane_beam(ui, accent);
                lane_node(ui, "SomeNET", self.stream_summary(), true, node_width);
                compact_lane_beam(ui, accent);
                lane_node(
                    ui,
                    "OUTPUT RIG",
                    self.output_label(),
                    self.config.mode == StreamMode::Receive,
                    node_width,
                );
            } else {
                let available = ui.available_width();
                let beam_width = ((available - 560.0) / 2.0).clamp(24.0, 52.0);
                let node_width =
                    ((available - (beam_width * 2.0) - 28.0) / 3.0).clamp(150.0, 210.0);
                ui.horizontal(|ui| {
                    lane_node(
                        ui,
                        "INPUT RIG",
                        self.input_label(),
                        self.config.mode == StreamMode::Send,
                        node_width,
                    );
                    lane_beam(ui, accent, beam_width);
                    lane_node(ui, "SomeNET", self.stream_summary(), true, node_width);
                    lane_beam(ui, accent, beam_width);
                    lane_node(
                        ui,
                        "OUTPUT RIG",
                        self.output_label(),
                        self.config.mode == StreamMode::Receive,
                        node_width,
                    );
                });
            }
        });
    }

    fn render_controls(&mut self, ui: &mut egui::Ui) {
        panel_frame().show(ui, |ui| {
            section_title(ui, "Control");
            ui.add_space(4.0);

            ui.horizontal_wrapped(|ui| {
                let previous_mode = self.config.mode;
                ui.selectable_value(&mut self.config.mode, StreamMode::Receive, "RECEIVE");
                ui.selectable_value(&mut self.config.mode, StreamMode::Send, "SEND");
                if self.config.mode != previous_mode && self.runtime.is_some() {
                    self.stop_runtime();
                }

                ui.separator();

                ui.selectable_value(
                    &mut self.config.transport,
                    StreamTransport::Unicast,
                    "UNICAST",
                );
                ui.selectable_value(
                    &mut self.config.transport,
                    StreamTransport::Multicast,
                    "MULTICAST",
                );
            });

            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                let start = egui::Button::new(
                    egui::RichText::new("START")
                        .font(egui::FontId::new(13.0, egui::FontFamily::Monospace))
                        .strong(),
                )
                .fill(color_red())
                .stroke(egui::Stroke::new(1.0, color_red_hot()));
                if ui.add_sized([88.0, 30.0], start).clicked() {
                    self.start_runtime();
                }

                let stop = egui::Button::new(
                    egui::RichText::new("STOP")
                        .font(egui::FontId::new(13.0, egui::FontFamily::Monospace)),
                )
                .fill(color_surface_2())
                .stroke(egui::Stroke::new(1.0, color_line()));
                if ui
                    .add_enabled_ui(self.runtime.is_some(), |ui| {
                        ui.add_sized([74.0, 30.0], stop)
                    })
                    .inner
                    .clicked()
                {
                    self.stop_runtime();
                }

                if ui.button("REFRESH").clicked() {
                    self.refresh_devices();
                }
            });
        });
    }

    fn render_settings(&mut self, ui: &mut egui::Ui) {
        panel_frame().show(ui, |ui| {
            section_title(ui, "Stream");
            ui.add_space(4.0);
            responsive_field(ui, "Channels", |ui| {
                ui.add(
                    egui::DragValue::new(&mut self.config.channels)
                        .range(1..=MAX_CHANNELS as u8)
                        .speed(1),
                );
            });

            responsive_field(ui, "Sample rate", |ui| {
                egui::ComboBox::from_id_salt("sample_rate")
                    .selected_text(format!("{} Hz", self.config.sample_rate_hz))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.config.sample_rate_hz, 44_100, "44100 Hz");
                        ui.selectable_value(&mut self.config.sample_rate_hz, 48_000, "48000 Hz");
                        ui.selectable_value(&mut self.config.sample_rate_hz, 96_000, "96000 Hz");
                    });
            });

            responsive_field(
                ui,
                endpoint_label(self.config.mode, self.config.transport),
                |ui| {
                    let width = ui.available_width().clamp(118.0, 180.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.ip_text)
                            .desired_width(width)
                            .font(egui::TextStyle::Monospace),
                    );
                },
            );

            responsive_field(ui, "UDP port", |ui| {
                ui.add(
                    egui::DragValue::new(&mut self.config.port)
                        .range(1..=u16::MAX)
                        .speed(1),
                );
            });

            responsive_field(ui, "Input", |ui| {
                device_combo(
                    ui,
                    "input_device",
                    &mut self.config.input_device,
                    &self.devices,
                    &self.default_input,
                    DeviceDirection::Input,
                );
            });

            responsive_field(ui, "Output", |ui| {
                device_combo(
                    ui,
                    "output_device",
                    &mut self.config.output_device,
                    &self.devices,
                    &self.default_output,
                    DeviceDirection::Output,
                );
            });
        });
    }

    fn render_clocking(&mut self, ui: &mut egui::Ui) {
        panel_frame().show(ui, |ui| {
            section_title(ui, "Clock");
            ui.add_space(4.0);
            let mut ptp = self.config.clock_reference == ClockReference::Ptp;
            if ui.checkbox(&mut ptp, "PTP reference").changed() {
                self.config.clock_reference = if ptp {
                    ClockReference::Ptp
                } else {
                    ClockReference::Local
                };
            }
            responsive_field(ui, "Domain", |ui| {
                ui.add_enabled(
                    ptp,
                    egui::DragValue::new(&mut self.config.ptp_domain)
                        .range(0..=127)
                        .speed(1),
                );
            });
            ui.add_space(8.0);
            self.render_channel_lanes(ui);
        });
    }

    fn render_channel_lanes(&self, ui: &mut egui::Ui) {
        let accent = mode_accent(self.config.mode);
        let cell_width = 11.0;
        let spacing = 3.0;
        let columns = ((ui.available_width() + spacing) / (cell_width + spacing))
            .floor()
            .clamp(8.0, 24.0) as usize;
        egui::Grid::new("channel_lanes")
            .num_columns(columns)
            .spacing([spacing, 4.0])
            .show(ui, |ui| {
                for channel in 0..MAX_CHANNELS {
                    let active = channel < usize::from(self.config.channels);
                    let fill = if active { accent } else { color_surface_3() };
                    let (rect, _) = ui.allocate_exact_size(
                        egui::Vec2::new(cell_width, 5.0),
                        egui::Sense::hover(),
                    );
                    ui.painter()
                        .rect_filled(rect, egui::CornerRadius::same(1), fill);
                    if (channel + 1) % columns == 0 {
                        ui.end_row();
                    }
                }
            });
    }

    fn render_runtime_status(&self, ui: &mut egui::Ui) {
        panel_frame().show(ui, |ui| {
            section_title(ui, "Status");
            ui.add_space(4.0);

            if let Some(runtime) = &self.runtime {
                egui::Grid::new("runtime_status")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        status_row(ui, "Mode", mode_title(runtime.params.mode));
                        status_row(ui, "Transport", transport_title(runtime.params.transport));
                        status_row(ui, "Device", &runtime.device_label);
                        status_row(
                            ui,
                            "Audio",
                            format!(
                                "{}ch {}Hz",
                                runtime.stream_config.channels, runtime.stream_config.sample_rate
                            ),
                        );
                        status_row(
                            ui,
                            "Buffer",
                            format!("{:?}", runtime.stream_config.buffer_size),
                        );
                        status_row(ui, "Uptime", uptime_text(runtime.started_at.elapsed()));
                    });

                ui.add_space(10.0);
                match &runtime.engine {
                    RuntimeEngine::Sender(sender) => {
                        let status = sender.status_snapshot();
                        metric_row(
                            ui,
                            [
                                ("Packets", status.packets_sent.to_string()),
                                ("Dropped", status.packets_dropped.to_string()),
                                ("Queued", status.queued_frames.to_string()),
                            ],
                        );
                    }
                    RuntimeEngine::Receiver(receiver) => {
                        let status = receiver.status_snapshot();
                        metric_row(
                            ui,
                            [
                                ("Packets", status.packets_received.to_string()),
                                ("Lost", status.packets_lost.to_string()),
                                ("Underruns", status.underruns.to_string()),
                            ],
                        );
                        ui.add_space(6.0);
                        metric_row(
                            ui,
                            [
                                ("Queued", status.queued_samples.to_string()),
                                ("Target", status.target_buffer_samples.to_string()),
                                ("Primed", bool_label(status.primed).to_string()),
                            ],
                        );
                    }
                }

                let warning = warning_text(
                    runtime.stream_config.sample_rate,
                    runtime.params.channels,
                    runtime.params.clock_reference,
                );
                if !warning.is_empty() {
                    ui.add_space(8.0);
                    ui.colored_label(color_amber(), warning);
                }
            } else {
                ui.label(egui::RichText::new("Runtime stopped").color(color_muted()));
            }

            if let Some(error) = &self.last_error {
                ui.add_space(8.0);
                ui.colored_label(color_red_hot(), error);
            }
        });
    }

    fn render_devices(&mut self, ui: &mut egui::Ui) {
        panel_frame().show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                section_title(ui, "Devices");
                ui.add_space(8.0);
                if ui.button("REFRESH").clicked() {
                    self.refresh_devices();
                }
            });
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!("Host: {}", self.host_name))
                    .font(egui::FontId::new(12.0, egui::FontFamily::Monospace))
                    .color(color_muted()),
            );
            ui.add_space(6.0);
            egui::ScrollArea::both()
                .max_height(360.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Grid::new("device_table")
                        .num_columns(4)
                        .spacing([10.0, 5.0])
                        .striped(true)
                        .show(ui, |ui| {
                            field_label(ui, "Name");
                            field_label(ui, "In");
                            field_label(ui, "Out");
                            field_label(ui, "Default format");
                            ui.end_row();
                            for device in &self.devices {
                                ui.label(&device.name);
                                ui.label(bool_label(device.input));
                                ui.label(bool_label(device.output));
                                ui.label(device_format(device));
                                ui.end_row();
                            }
                        });
                });
        });
    }

    fn render_tab_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            tab_button(ui, &mut self.active_tab, SomeNetTab::Route, "ROUTE");
            tab_button(ui, &mut self.active_tab, SomeNetTab::Monitor, "MONITOR");
            tab_button(ui, &mut self.active_tab, SomeNetTab::Devices, "DEVICES");
        });
    }

    fn render_active_tab(&mut self, ui: &mut egui::Ui) {
        match self.active_tab {
            SomeNetTab::Route => self.render_route_tab(ui),
            SomeNetTab::Monitor => self.render_monitor_tab(ui),
            SomeNetTab::Devices => self.render_devices(ui),
        }
    }

    fn render_route_tab(&mut self, ui: &mut egui::Ui) {
        self.render_controls(ui);
        ui.add_space(8.0);
        self.render_settings(ui);
        ui.add_space(8.0);
        self.render_clocking(ui);
    }

    fn render_monitor_tab(&mut self, ui: &mut egui::Ui) {
        self.render_runtime_status(ui);
        ui.add_space(8.0);
        panel_frame().show(ui, |ui| {
            section_title(ui, "Channels");
            ui.add_space(6.0);
            self.render_channel_lanes(ui);
        });
    }

    fn stream_summary(&self) -> String {
        format!(
            "{} {}ch {}:{}",
            transport_title(self.config.transport),
            self.config.channels,
            self.ip_text,
            self.config.port
        )
    }

    fn input_label(&self) -> String {
        selected_device_label(
            self.config.input_device.as_deref(),
            &self.devices,
            &self.default_input,
        )
    }

    fn output_label(&self) -> String {
        selected_device_label(
            self.config.output_device.as_deref(),
            &self.devices,
            &self.default_output,
        )
    }
}

impl eframe::App for SomeNetNativeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.runtime.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }

        egui::Frame::NONE
            .fill(color_bg())
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        self.render_top_bar(ui);
                        ui.add_space(10.0);
                        self.render_transport_lane(ui);
                        ui.add_space(8.0);
                        self.render_tab_bar(ui);
                        ui.add_space(8.0);
                        self.render_active_tab(ui);
                    });
            });
    }
}

fn configure_native_style(ctx: &egui::Context) {
    let mut style = (*ctx.global_style()).clone();
    style.spacing.item_spacing = egui::Vec2::new(7.0, 6.0);
    style.spacing.button_padding = egui::Vec2::new(9.0, 5.0);
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = color_bg();
    style.visuals.window_fill = color_surface();
    style.visuals.extreme_bg_color = color_black();
    style.visuals.widgets.noninteractive.bg_fill = color_surface();
    style.visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, color_line());
    style.visuals.widgets.inactive.bg_fill = color_surface_2();
    style.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, color_line());
    style.visuals.widgets.hovered.bg_fill = color_surface_3();
    style.visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, color_red());
    style.visuals.widgets.active.bg_fill = color_red();
    style.visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, color_red_hot());
    style.visuals.selection.bg_fill = color_red();
    style.visuals.selection.stroke = egui::Stroke::new(1.0, color_red_hot());
    ctx.set_global_style(style);
}

fn panel_frame() -> egui::Frame {
    egui::Frame::NONE
        .fill(color_surface())
        .stroke(egui::Stroke::new(1.0, color_line()))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(10, 9))
}

fn lane_node(ui: &mut egui::Ui, label: &str, detail: String, active: bool, width: f32) {
    let fill = if active {
        color_surface_2()
    } else {
        color_black()
    };
    egui::Frame::NONE
        .fill(fill)
        .stroke(egui::Stroke::new(
            1.0,
            if active { color_red() } else { color_line() },
        ))
        .corner_radius(egui::CornerRadius::same(3))
        .inner_margin(egui::Margin::symmetric(9, 7))
        .show(ui, |ui| {
            let content_width = (width - 22.0).max(116.0);
            ui.set_width(content_width);
            ui.label(
                egui::RichText::new(label)
                    .font(egui::FontId::new(11.0, egui::FontFamily::Monospace))
                    .color(if active { color_text() } else { color_muted() }),
            );
            ui.add(
                egui::Label::new(
                    egui::RichText::new(detail)
                        .font(egui::FontId::new(12.0, egui::FontFamily::Monospace))
                        .color(color_muted()),
                )
                .truncate(),
            );
        });
}

fn lane_beam(ui: &mut egui::Ui, color: egui::Color32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::new(width, 28.0), egui::Sense::hover());
    let center_y = rect.center().y;
    ui.painter().line_segment(
        [
            egui::pos2(rect.left(), center_y),
            egui::pos2(rect.right(), center_y),
        ],
        egui::Stroke::new(2.0, color),
    );
    ui.painter().line_segment(
        [
            egui::pos2(rect.right() - 8.0, center_y - 5.0),
            egui::pos2(rect.right(), center_y),
        ],
        egui::Stroke::new(2.0, color),
    );
    ui.painter().line_segment(
        [
            egui::pos2(rect.right() - 8.0, center_y + 5.0),
            egui::pos2(rect.right(), center_y),
        ],
        egui::Stroke::new(2.0, color),
    );
}

fn compact_lane_beam(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::Vec2::new(18.0, 16.0), egui::Sense::hover());
    let x = rect.center().x;
    ui.painter().line_segment(
        [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
        egui::Stroke::new(2.0, color),
    );
    ui.painter().line_segment(
        [
            egui::pos2(x - 5.0, rect.bottom() - 5.0),
            egui::pos2(x, rect.bottom()),
        ],
        egui::Stroke::new(2.0, color),
    );
    ui.painter().line_segment(
        [
            egui::pos2(x + 5.0, rect.bottom() - 5.0),
            egui::pos2(x, rect.bottom()),
        ],
        egui::Stroke::new(2.0, color),
    );
}

fn section_title(ui: &mut egui::Ui, title: &str) {
    ui.label(
        egui::RichText::new(title)
            .font(egui::FontId::new(13.0, egui::FontFamily::Monospace))
            .strong()
            .color(color_text()),
    );
}

fn field_label(ui: &mut egui::Ui, label: &str) {
    ui.label(field_label_text(label));
}

fn field_label_text(label: &str) -> egui::RichText {
    egui::RichText::new(label)
        .font(egui::FontId::new(11.0, egui::FontFamily::Monospace))
        .color(color_muted())
}

fn responsive_field(ui: &mut egui::Ui, label: &str, add_control: impl FnOnce(&mut egui::Ui)) {
    if ui.available_width() < 330.0 {
        field_label(ui, label);
        add_control(ui);
    } else {
        ui.horizontal(|ui| {
            ui.add_sized(
                [88.0, 20.0],
                egui::Label::new(field_label_text(label)).truncate(),
            );
            add_control(ui);
        });
    }
    ui.add_space(2.0);
}

fn status_row(ui: &mut egui::Ui, label: &str, value: impl ToString) {
    let value = value.to_string();
    field_label(ui, label);
    ui.add(egui::Label::new(value.clone()).truncate())
        .on_hover_text(value);
    ui.end_row();
}

fn metric_row(ui: &mut egui::Ui, metrics: [(&str, String); 3]) {
    let metric_width = ((ui.available_width() - 16.0) / 3.0).clamp(82.0, 106.0);
    ui.horizontal_wrapped(|ui| {
        for (label, value) in metrics {
            egui::Frame::NONE
                .fill(color_black())
                .stroke(egui::Stroke::new(1.0, color_line()))
                .corner_radius(egui::CornerRadius::same(3))
                .inner_margin(egui::Margin::symmetric(9, 6))
                .show(ui, |ui| {
                    ui.set_min_width(metric_width);
                    field_label(ui, label);
                    ui.label(
                        egui::RichText::new(value)
                            .font(egui::FontId::new(16.0, egui::FontFamily::Monospace))
                            .color(color_text()),
                    );
                });
        }
    });
}

fn tab_button(ui: &mut egui::Ui, active_tab: &mut SomeNetTab, tab: SomeNetTab, label: &str) {
    let active = *active_tab == tab;
    let text = egui::RichText::new(label)
        .font(egui::FontId::new(12.0, egui::FontFamily::Monospace))
        .strong()
        .color(if active { color_text() } else { color_muted() });
    let response = ui.add_sized(
        [92.0, 28.0],
        egui::Button::new(text)
            .fill(if active {
                color_surface_3()
            } else {
                color_black()
            })
            .stroke(egui::Stroke::new(
                1.0,
                if active {
                    color_red_hot()
                } else {
                    color_line()
                },
            )),
    );
    if response.clicked() {
        *active_tab = tab;
    }
}

fn device_combo(
    ui: &mut egui::Ui,
    id: &'static str,
    selected: &mut Option<String>,
    devices: &[DeviceChoice],
    default_label: &str,
    direction: DeviceDirection,
) {
    let selected_text = selected_device_label(selected.as_deref(), devices, default_label);
    let width = ui.available_width().clamp(150.0, 240.0);
    egui::ComboBox::from_id_salt(id)
        .selected_text(selected_text)
        .width(width)
        .show_ui(ui, |ui| {
            ui.selectable_value(selected, None, format!("Default ({default_label})"));
            for device in devices.iter().filter(|device| match direction {
                DeviceDirection::Input => device.input,
                DeviceDirection::Output => device.output,
            }) {
                ui.selectable_value(
                    selected,
                    Some(device.query().to_string()),
                    device.name.as_str(),
                );
            }
        });
}

fn selected_device_label(
    selected: Option<&str>,
    devices: &[DeviceChoice],
    default_label: &str,
) -> String {
    let Some(query) = selected else {
        return format!("Default ({default_label})");
    };
    devices
        .iter()
        .find(|device| device.matches_query(query))
        .map(|device| device.name.clone())
        .unwrap_or_else(|| query.to_string())
}

fn device_format(device: &DeviceChoice) -> &str {
    if !device.input_default.is_empty() {
        &device.input_default
    } else if !device.output_default.is_empty() {
        &device.output_default
    } else {
        ""
    }
}

fn endpoint_label(mode: StreamMode, transport: StreamTransport) -> &'static str {
    match (mode, transport) {
        (StreamMode::Send, StreamTransport::Unicast) => "Destination",
        (StreamMode::Send, StreamTransport::Multicast) => "Group",
        (StreamMode::Receive, StreamTransport::Unicast) => "Source",
        (StreamMode::Receive, StreamTransport::Multicast) => "Group",
    }
}

fn mode_accent(mode: StreamMode) -> egui::Color32 {
    match mode {
        StreamMode::Send => color_red_hot(),
        StreamMode::Receive => color_green(),
    }
}

fn mode_title(mode: StreamMode) -> &'static str {
    match mode {
        StreamMode::Send => "SEND",
        StreamMode::Receive => "RECEIVE",
    }
}

fn transport_title(transport: StreamTransport) -> &'static str {
    match transport {
        StreamTransport::Unicast => "UNICAST",
        StreamTransport::Multicast => "MULTICAST",
    }
}

fn bool_label(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn uptime_text(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds / 60) % 60;
    let seconds = seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

fn color_black() -> egui::Color32 {
    egui::Color32::from_rgb(5, 6, 6)
}

fn color_bg() -> egui::Color32 {
    egui::Color32::from_rgb(9, 10, 10)
}

fn color_surface() -> egui::Color32 {
    egui::Color32::from_rgb(18, 20, 19)
}

fn color_surface_2() -> egui::Color32 {
    egui::Color32::from_rgb(28, 31, 29)
}

fn color_surface_3() -> egui::Color32 {
    egui::Color32::from_rgb(42, 45, 42)
}

fn color_line() -> egui::Color32 {
    egui::Color32::from_rgb(63, 68, 63)
}

fn color_text() -> egui::Color32 {
    egui::Color32::from_rgb(231, 231, 221)
}

fn color_muted() -> egui::Color32 {
    egui::Color32::from_rgb(143, 149, 139)
}

fn color_red() -> egui::Color32 {
    egui::Color32::from_rgb(124, 27, 27)
}

fn color_red_hot() -> egui::Color32 {
    egui::Color32::from_rgb(229, 48, 40)
}

fn color_green() -> egui::Color32 {
    egui::Color32::from_rgb(80, 181, 123)
}

fn color_amber() -> egui::Color32 {
    egui::Color32::from_rgb(209, 156, 63)
}

fn build_sender_stream(
    device: &cpal::Device,
    sample_format: SampleFormat,
    stream_config: cpal::StreamConfig,
    sender: Arc<NetworkSender>,
    params: StreamParameters,
) -> AppResult<cpal::Stream> {
    match sample_format {
        SampleFormat::F32 => {
            build_sender_stream_typed::<f32>(device, stream_config, sender, params)
        }
        SampleFormat::F64 => {
            build_sender_stream_typed::<f64>(device, stream_config, sender, params)
        }
        SampleFormat::I16 => {
            build_sender_stream_typed::<i16>(device, stream_config, sender, params)
        }
        SampleFormat::I32 => {
            build_sender_stream_typed::<i32>(device, stream_config, sender, params)
        }
        SampleFormat::U16 => {
            build_sender_stream_typed::<u16>(device, stream_config, sender, params)
        }
        SampleFormat::U32 => {
            build_sender_stream_typed::<u32>(device, stream_config, sender, params)
        }
        other => Err(format!("unsupported input sample format: {other}").into()),
    }
}

fn build_sender_stream_typed<T>(
    device: &cpal::Device,
    stream_config: cpal::StreamConfig,
    sender: Arc<NetworkSender>,
    params: StreamParameters,
) -> AppResult<cpal::Stream>
where
    T: Sample + SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let device_channels = usize::from(stream_config.channels).max(1);
    let network_channels = usize::from(params.channels).min(MAX_CHANNELS);
    let sample_rate_hz = stream_config.sample_rate;
    let initial_frames = preallocated_callback_frames(stream_config.buffer_size);
    let mut channel_buffers = vec![vec![0.0; initial_frames]; network_channels];

    let stream = device.build_input_stream(
        &stream_config,
        move |data: &[T], _| {
            let frames = data.len() / device_channels;
            for buffer in &mut channel_buffers {
                buffer.resize(frames, 0.0);
            }

            for (frame, input_frame) in data.chunks_exact(device_channels).enumerate() {
                for channel_index in 0..network_channels {
                    channel_buffers[channel_index][frame] = if channel_index < device_channels {
                        f32::from_sample(input_frame[channel_index])
                    } else {
                        0.0
                    };
                }
            }

            let input_channels: [Option<&[f32]>; MAX_CHANNELS] = std::array::from_fn(|index| {
                if index < network_channels {
                    Some(channel_buffers[index].as_slice())
                } else {
                    None
                }
            });
            sender.push_audio(params, sample_rate_hz, &input_channels, frames);
        },
        log_stream_error,
        None,
    )?;

    Ok(stream)
}

fn build_receiver_stream(
    device: &cpal::Device,
    sample_format: SampleFormat,
    stream_config: cpal::StreamConfig,
    receiver: Arc<NetworkReceiver>,
    params: StreamParameters,
) -> AppResult<cpal::Stream> {
    match sample_format {
        SampleFormat::F32 => {
            build_receiver_stream_typed::<f32>(device, stream_config, receiver, params)
        }
        SampleFormat::F64 => {
            build_receiver_stream_typed::<f64>(device, stream_config, receiver, params)
        }
        SampleFormat::I16 => {
            build_receiver_stream_typed::<i16>(device, stream_config, receiver, params)
        }
        SampleFormat::I32 => {
            build_receiver_stream_typed::<i32>(device, stream_config, receiver, params)
        }
        SampleFormat::U16 => {
            build_receiver_stream_typed::<u16>(device, stream_config, receiver, params)
        }
        SampleFormat::U32 => {
            build_receiver_stream_typed::<u32>(device, stream_config, receiver, params)
        }
        other => Err(format!("unsupported output sample format: {other}").into()),
    }
}

fn build_receiver_stream_typed<T>(
    device: &cpal::Device,
    stream_config: cpal::StreamConfig,
    receiver: Arc<NetworkReceiver>,
    params: StreamParameters,
) -> AppResult<cpal::Stream>
where
    T: Sample + SizedSample + FromSample<f32> + Send + 'static,
{
    let device_channels = usize::from(stream_config.channels).max(1);
    let network_channels = usize::from(params.channels).min(MAX_CHANNELS);
    let sample_rate_hz = stream_config.sample_rate;
    let initial_frames = preallocated_callback_frames(stream_config.buffer_size);
    let mut channel_buffers = vec![vec![0.0; initial_frames]; network_channels];

    let stream = device.build_output_stream(
        &stream_config,
        move |data: &mut [T], _| {
            let frames = data.len() / device_channels;
            for buffer in &mut channel_buffers {
                buffer.resize(frames, 0.0);
            }

            {
                let mut output_channels: [Option<&mut [f32]>; MAX_CHANNELS] =
                    std::array::from_fn(|_| None);
                for (index, buffer) in channel_buffers.iter_mut().enumerate() {
                    output_channels[index] = Some(buffer.as_mut_slice());
                }
                receiver.pull_audio(params, sample_rate_hz, &mut output_channels, frames);
            }

            for (frame, output_frame) in data.chunks_exact_mut(device_channels).enumerate() {
                for (channel_index, output_sample) in output_frame.iter_mut().enumerate() {
                    let sample = if channel_index < network_channels {
                        channel_buffers[channel_index][frame]
                    } else {
                        0.0
                    };
                    *output_sample = T::from_sample(sample);
                }
            }
        },
        log_stream_error,
        None,
    )?;

    Ok(stream)
}

fn preallocated_callback_frames(buffer_size: BufferSize) -> usize {
    match buffer_size {
        BufferSize::Fixed(frames) => (frames as usize).clamp(1, MAX_PREALLOCATED_CALLBACK_FRAMES),
        BufferSize::Default => DEFAULT_PREALLOCATED_CALLBACK_FRAMES,
    }
}

#[derive(Clone, Copy)]
enum DeviceDirection {
    Input,
    Output,
}

fn select_device(
    host: &cpal::Host,
    query: Option<&str>,
    direction: DeviceDirection,
) -> AppResult<cpal::Device> {
    if let Some(query) = query {
        let query = query.to_ascii_lowercase();
        let devices = match direction {
            DeviceDirection::Input => host.input_devices()?,
            DeviceDirection::Output => host.output_devices()?,
        };

        for device in devices {
            let id = device
                .id()
                .ok()
                .map(|id| id.to_string())
                .unwrap_or_default();
            let label = device_label(&device);
            if id.to_ascii_lowercase().contains(&query)
                || label.to_ascii_lowercase().contains(&query)
            {
                return Ok(device);
            }
        }

        return Err(format!("no matching audio device for '{query}'").into());
    }

    let device = match direction {
        DeviceDirection::Input => host.default_input_device(),
        DeviceDirection::Output => host.default_output_device(),
    };
    device.ok_or_else(|| "no default audio device available".into())
}

fn select_config(
    device: &cpal::Device,
    direction: DeviceDirection,
    channels: u8,
    sample_rate_hz: u32,
) -> AppResult<cpal::SupportedStreamConfig> {
    let target_rate = sample_rate_hz;
    let min_channels = channels as u16;
    let selected = match direction {
        DeviceDirection::Input => device
            .supported_input_configs()?
            .filter(|config| {
                config.channels() >= min_channels
                    && config.min_sample_rate() <= target_rate
                    && config.max_sample_rate() >= target_rate
            })
            .min_by_key(|config| {
                (
                    sample_format_rank(config.sample_format()),
                    config.channels().saturating_sub(min_channels),
                )
            }),
        DeviceDirection::Output => device
            .supported_output_configs()?
            .filter(|config| {
                config.channels() >= min_channels
                    && config.min_sample_rate() <= target_rate
                    && config.max_sample_rate() >= target_rate
            })
            .min_by_key(|config| {
                (
                    sample_format_rank(config.sample_format()),
                    config.channels().saturating_sub(min_channels),
                )
            }),
    };

    if let Some(config) = selected {
        Ok(config.with_sample_rate(target_rate))
    } else {
        let fallback = match direction {
            DeviceDirection::Input => device.default_input_config()?,
            DeviceDirection::Output => device.default_output_config()?,
        };
        Ok(fallback)
    }
}

fn sample_format_rank(format: SampleFormat) -> u8 {
    match format {
        SampleFormat::F32 => 0,
        SampleFormat::F64 => 1,
        SampleFormat::I32 => 2,
        SampleFormat::I16 => 3,
        SampleFormat::U32 => 4,
        SampleFormat::U16 => 5,
        _ => 20,
    }
}

fn validate_sample_rate(sample_rate_hz: u32) -> AppResult<()> {
    match sample_rate_hz {
        44_100 | 48_000 | 96_000 => Ok(()),
        _ => Err(format!(
            "unsupported sample rate {sample_rate_hz}; SomeNET currently supports 44100, 48000, or 96000 Hz"
        )
        .into()),
    }
}

fn wait_for_shutdown<F>(duration: Option<Duration>, running: Arc<AtomicBool>, mut print_status: F)
where
    F: FnMut(),
{
    let started = Instant::now();
    let mut next_status = Instant::now();
    while running.load(Ordering::SeqCst) {
        if duration.is_some_and(|duration| started.elapsed() >= duration) {
            break;
        }

        if Instant::now() >= next_status {
            print_status();
            next_status = Instant::now() + Duration::from_millis(1000);
        }

        thread::sleep(Duration::from_millis(50));
    }
}

fn list_devices() -> AppResult<()> {
    let host = cpal::default_host();
    println!("SomeNET audio devices");
    println!("host={}", host.id().name());
    println!();

    let default_input = host
        .default_input_device()
        .map(|device| device_label(&device))
        .unwrap_or_else(|| "none".to_string());
    let default_output = host
        .default_output_device()
        .map(|device| device_label(&device))
        .unwrap_or_else(|| "none".to_string());
    println!("default_input={default_input}");
    println!("default_output={default_output}");
    println!();

    for (index, device) in host.devices()?.enumerate() {
        println!("{}. {}", index + 1, device_label(&device));
        if let Ok(id) = device.id() {
            println!("   id={id}");
        }
        if let Ok(config) = device.default_input_config() {
            println!("   input_default={config:?}");
        }
        if let Ok(config) = device.default_output_config() {
            println!("   output_default={config:?}");
        }
    }

    Ok(())
}

fn print_launch_header(
    mode: &str,
    device_label: &str,
    config: &cpal::StreamConfig,
    params: StreamParameters,
) {
    let transport = match params.transport {
        StreamTransport::Unicast => "unicast",
        StreamTransport::Multicast => "multicast",
    };
    let endpoint = Ipv4Addr::from(params.ip);
    println!("SomeNET {}", env!("CARGO_PKG_VERSION"));
    println!("mode={mode}");
    println!("device={device_label}");
    println!(
        "audio={}ch {}Hz buffer={:?}",
        config.channels, config.sample_rate, config.buffer_size
    );
    println!(
        "stream={}ch {} {}:{} clock={} ptp_domain={}",
        params.channels,
        transport,
        endpoint,
        params.port,
        params.clock_reference.status_name(),
        params.ptp_domain
    );
    let warning = warning_text(config.sample_rate, params.channels, params.clock_reference);
    if !warning.is_empty() {
        println!("warning={warning}");
    }
    println!("Press Ctrl-C to stop.");
}

fn device_label(device: &cpal::Device) -> String {
    device
        .description()
        .map(|description| description.to_string())
        .or_else(|_| device.id().map(|id| id.to_string()))
        .unwrap_or_else(|_| "unknown device".to_string())
}

fn log_stream_error(err: cpal::StreamError) {
    eprintln!("audio stream error: {err}");
}

fn parse_args(args: impl Iterator<Item = String>) -> AppResult<AppConfig> {
    let mut config = AppConfig::default();
    let mut args = args.peekable();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" | "help" => config.command = Command::Help,
            "ui" | "--ui" => config.command = Command::Ui,
            "devices" | "--devices" | "--list-devices" => config.command = Command::Devices,
            "send" => config.mode = StreamMode::Send,
            "receive" | "recv" => config.mode = StreamMode::Receive,
            "--mode" => {
                let value = next_value(&mut args, "--mode")?;
                config.mode = parse_mode(&value)?;
            }
            "--channels" | "-c" => {
                let value = next_value(&mut args, "--channels")?;
                config.channels = value.parse::<u8>()?.clamp(1, MAX_CHANNELS as u8);
            }
            "--port" | "-p" => {
                let value = next_value(&mut args, "--port")?;
                config.port = value.parse::<u16>()?.max(1);
            }
            "--ip" => {
                let value = next_value(&mut args, "--ip")?;
                config.ip = parse_ipv4(&value)?;
            }
            "--multicast" => config.transport = StreamTransport::Multicast,
            "--unicast" => config.transport = StreamTransport::Unicast,
            "--ptp" => config.clock_reference = ClockReference::Ptp,
            "--local-clock" => config.clock_reference = ClockReference::Local,
            "--ptp-domain" => {
                let value = next_value(&mut args, "--ptp-domain")?;
                config.ptp_domain = value.parse::<u8>()?.min(127);
            }
            "--sample-rate" | "-r" => {
                let value = next_value(&mut args, "--sample-rate")?;
                config.sample_rate_hz = value.parse::<u32>()?;
            }
            "--input" => {
                config.input_device = Some(next_value(&mut args, "--input")?);
            }
            "--output" => {
                config.output_device = Some(next_value(&mut args, "--output")?);
            }
            "--duration" => {
                let value = next_value(&mut args, "--duration")?;
                config.duration = Some(Duration::from_secs(value.parse::<u64>()?));
            }
            unknown => return Err(format!("unknown argument: {unknown}").into()),
        }
    }

    Ok(config)
}

fn next_value(
    args: &mut std::iter::Peekable<impl Iterator<Item = String>>,
    flag: &str,
) -> AppResult<String> {
    args.next()
        .ok_or_else(|| format!("missing value for {flag}").into())
}

fn parse_mode(value: &str) -> AppResult<StreamMode> {
    match value.to_ascii_lowercase().as_str() {
        "send" => Ok(StreamMode::Send),
        "receive" | "recv" => Ok(StreamMode::Receive),
        _ => Err(format!("invalid mode: {value}").into()),
    }
}

fn parse_ipv4(value: &str) -> AppResult<[u8; 4]> {
    let addr: Ipv4Addr = value.parse()?;
    Ok(addr.octets())
}

fn print_help() {
    println!(
        "SomeNET standalone app

Usage:
  somenet [receive|send] [options]
  somenet ui [options]
  somenet devices

Default launch:
  somenet
    Starts receive mode on the default output device at 48 kHz, 2ch, UDP port 5004.

Options:
  --mode send|receive
  --channels N          Audio channels on the SomeNET stream, 1-96
  --sample-rate HZ      44100, 48000, or 96000
  --ip A.B.C.D          Destination, multicast group, or expected source
  --port N              UDP port
  --unicast             Use direct unicast routing
  --multicast           Use multicast routing
  --input TEXT          Input device id/name substring for send mode
  --output TEXT         Output device id/name substring for receive mode
  --ptp                 Advertise PTP clock reference in SDP/status
  --ptp-domain N        PTP domain, 0-127
  --duration SECONDS    Stop automatically after a test run
"
    );
}
