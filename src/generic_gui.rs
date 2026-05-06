use crate::{
    editor_api::EditorControllerApi,
    network::{ClockReference, StreamMode, StreamParameters, StreamTransport},
    params::{
        PARAM_CHANNELS, PARAM_CLOCK_REF, PARAM_ENABLED, PARAM_IP_1, PARAM_IP_2, PARAM_IP_3,
        PARAM_IP_4, PARAM_MODE, PARAM_PORT, PARAM_PTP_DOMAIN, PARAM_TRANSPORT, VST3_MAX_CHANNELS,
        parameter_spec,
    },
};

pub(crate) const VIEW_WIDTH: i32 = 500;
pub(crate) const VIEW_HEIGHT: i32 = 492;

#[derive(Clone, Copy)]
pub(crate) struct Rect {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) w: i32,
    pub(crate) h: i32,
}

impl Rect {
    pub(crate) fn contains(self, px: i32, py: i32) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Rgb {
    pub(crate) r: u8,
    pub(crate) g: u8,
    pub(crate) b: u8,
}

#[derive(Clone, Copy)]
pub(crate) struct Theme {
    pub(crate) background: Rgb,
    pub(crate) panel_fill: Rgb,
    pub(crate) field_fill: Rgb,
    pub(crate) rule: Rgb,
    pub(crate) text: Rgb,
    pub(crate) secondary_text: Rgb,
    pub(crate) accent: Rgb,
    pub(crate) selected_fill: Rgb,
}

pub(crate) fn theme(mode: StreamMode) -> Theme {
    match mode {
        StreamMode::Send => Theme {
            background: rgb(0.93, 0.91, 0.86),
            panel_fill: rgb(0.98, 0.97, 0.94),
            field_fill: rgb(0.98, 0.97, 0.94),
            rule: rgb(0.76, 0.73, 0.67),
            text: rgb(0.12, 0.12, 0.11),
            secondary_text: rgb(0.38, 0.39, 0.36),
            accent: rgb(0.46, 0.31, 0.18),
            selected_fill: rgb(0.89, 0.84, 0.75),
        },
        StreamMode::Receive => Theme {
            background: rgb(0.90, 0.92, 0.90),
            panel_fill: rgb(0.95, 0.97, 0.95),
            field_fill: rgb(0.97, 0.98, 0.97),
            rule: rgb(0.63, 0.69, 0.65),
            text: rgb(0.12, 0.12, 0.11),
            secondary_text: rgb(0.35, 0.40, 0.37),
            accent: rgb(0.19, 0.34, 0.27),
            selected_fill: rgb(0.83, 0.89, 0.85),
        },
    }
}

const ARM_RECT: Rect = Rect {
    x: 364,
    y: 36,
    w: 112,
    h: 30,
};

const MODE_SEND_RECT: Rect = Rect {
    x: 76,
    y: 112,
    w: 70,
    h: 28,
};
const MODE_RECV_RECT: Rect = Rect {
    x: 146,
    y: 112,
    w: 70,
    h: 28,
};
const TRANSPORT_UNI_RECT: Rect = Rect {
    x: 318,
    y: 112,
    w: 79,
    h: 28,
};
const TRANSPORT_MULTI_RECT: Rect = Rect {
    x: 397,
    y: 112,
    w: 79,
    h: 28,
};
const CLOCK_LOCAL_RECT: Rect = Rect {
    x: 76,
    y: 158,
    w: 70,
    h: 28,
};
const CLOCK_PTP_RECT: Rect = Rect {
    x: 146,
    y: 158,
    w: 70,
    h: 28,
};

const CHANNELS_MINUS_RECT: Rect = Rect {
    x: 110,
    y: 208,
    w: 24,
    h: 24,
};
const CHANNELS_VALUE_RECT: Rect = Rect {
    x: 136,
    y: 208,
    w: 44,
    h: 24,
};
const CHANNELS_PLUS_RECT: Rect = Rect {
    x: 182,
    y: 208,
    w: 24,
    h: 24,
};

const PORT_MINUS_RECT: Rect = Rect {
    x: 258,
    y: 208,
    w: 24,
    h: 24,
};
const PORT_VALUE_RECT: Rect = Rect {
    x: 284,
    y: 208,
    w: 58,
    h: 24,
};
const PORT_PLUS_RECT: Rect = Rect {
    x: 344,
    y: 208,
    w: 24,
    h: 24,
};

const PTP_MINUS_RECT: Rect = Rect {
    x: 334,
    y: 158,
    w: 24,
    h: 24,
};
const PTP_VALUE_RECT: Rect = Rect {
    x: 360,
    y: 158,
    w: 44,
    h: 24,
};
const PTP_PLUS_RECT: Rect = Rect {
    x: 406,
    y: 158,
    w: 24,
    h: 24,
};

const APPLY_RECT: Rect = Rect {
    x: 386,
    y: 274,
    w: 90,
    h: 32,
};
const RUNTIME_PANEL_RECT: Rect = Rect {
    x: 24,
    y: 374,
    w: 452,
    h: 100,
};

const IP_VALUE_RECTS: [Rect; 4] = [
    Rect {
        x: 48,
        y: 276,
        w: 22,
        h: 28,
    },
    Rect {
        x: 126,
        y: 276,
        w: 22,
        h: 28,
    },
    Rect {
        x: 204,
        y: 276,
        w: 22,
        h: 28,
    },
    Rect {
        x: 282,
        y: 276,
        w: 22,
        h: 28,
    },
];

const IP_MINUS_RECTS: [Rect; 4] = [
    Rect {
        x: 24,
        y: 276,
        w: 22,
        h: 28,
    },
    Rect {
        x: 102,
        y: 276,
        w: 22,
        h: 28,
    },
    Rect {
        x: 180,
        y: 276,
        w: 22,
        h: 28,
    },
    Rect {
        x: 258,
        y: 276,
        w: 22,
        h: 28,
    },
];

const IP_PLUS_RECTS: [Rect; 4] = [
    Rect {
        x: 72,
        y: 276,
        w: 22,
        h: 28,
    },
    Rect {
        x: 150,
        y: 276,
        w: 22,
        h: 28,
    },
    Rect {
        x: 228,
        y: 276,
        w: 22,
        h: 28,
    },
    Rect {
        x: 306,
        y: 276,
        w: 22,
        h: 28,
    },
];

pub(crate) struct EditorState {
    pub(crate) params: StreamParameters,
    pub(crate) status: [String; 4],
}

impl EditorState {
    pub(crate) fn new(controller: EditorControllerApi) -> Self {
        let mut state = Self {
            params: unsafe { (controller.parameters)(controller.controller) },
            status: unsafe { (controller.runtime_status_lines)(controller.controller) },
        };
        state.sync_from_controller(controller);
        state
    }

    pub(crate) fn sync_from_controller(&mut self, controller: EditorControllerApi) {
        self.params = unsafe { (controller.parameters)(controller.controller) };
        self.status = unsafe { (controller.runtime_status_lines)(controller.controller) };
    }

    pub(crate) fn refresh_status(&mut self, controller: EditorControllerApi) {
        self.status = unsafe { (controller.runtime_status_lines)(controller.controller) };
    }

    pub(crate) fn handle_click(&mut self, controller: EditorControllerApi, x: i32, y: i32) -> bool {
        let Some(action) = hit_test(x, y) else {
            return false;
        };

        match action {
            ClickAction::ToggleArm => {
                self.params.enabled = !self.params.enabled;
                apply_param(
                    controller,
                    PARAM_ENABLED,
                    parameter_spec(PARAM_ENABLED)
                        .unwrap()
                        .plain_to_normalized(self.params.enabled as u32),
                );
                self.sync_from_controller(controller);
            }
            ClickAction::SetMode(mode) => self.params.mode = mode,
            ClickAction::SetTransport(transport) => self.params.transport = transport,
            ClickAction::SetClockReference(clock_reference) => {
                self.params.clock_reference = clock_reference;
                if matches!(clock_reference, ClockReference::Local) {
                    self.params.ptp_domain = 0;
                }
            }
            ClickAction::AdjustChannels(delta) => {
                self.params.channels =
                    step_u8(self.params.channels, delta, 1, VST3_MAX_CHANNELS as u8);
            }
            ClickAction::AdjustPort(delta) => {
                self.params.port = step_u16(self.params.port, delta, 1, u16::MAX);
            }
            ClickAction::AdjustPtpDomain(delta) => {
                self.params.ptp_domain = step_u8(self.params.ptp_domain, delta, 0, 127);
            }
            ClickAction::AdjustIp(index, delta) => {
                self.params.ip[index] = step_u8(self.params.ip[index], delta, 0, 255);
            }
            ClickAction::Apply => {
                apply_stream_parameters(controller, self.params);
                unsafe {
                    (controller.trigger_apply_reset)(controller.controller);
                }
                self.sync_from_controller(controller);
            }
        }

        true
    }
}

#[derive(Clone, Copy)]
enum ClickAction {
    ToggleArm,
    SetMode(StreamMode),
    SetTransport(StreamTransport),
    SetClockReference(ClockReference),
    AdjustChannels(i32),
    AdjustPort(i32),
    AdjustPtpDomain(i32),
    AdjustIp(usize, i32),
    Apply,
}

fn hit_test(x: i32, y: i32) -> Option<ClickAction> {
    if ARM_RECT.contains(x, y) {
        return Some(ClickAction::ToggleArm);
    }
    if MODE_SEND_RECT.contains(x, y) {
        return Some(ClickAction::SetMode(StreamMode::Send));
    }
    if MODE_RECV_RECT.contains(x, y) {
        return Some(ClickAction::SetMode(StreamMode::Receive));
    }
    if TRANSPORT_UNI_RECT.contains(x, y) {
        return Some(ClickAction::SetTransport(StreamTransport::Unicast));
    }
    if TRANSPORT_MULTI_RECT.contains(x, y) {
        return Some(ClickAction::SetTransport(StreamTransport::Multicast));
    }
    if CLOCK_LOCAL_RECT.contains(x, y) {
        return Some(ClickAction::SetClockReference(ClockReference::Local));
    }
    if CLOCK_PTP_RECT.contains(x, y) {
        return Some(ClickAction::SetClockReference(ClockReference::Ptp));
    }
    if CHANNELS_MINUS_RECT.contains(x, y) {
        return Some(ClickAction::AdjustChannels(-1));
    }
    if CHANNELS_PLUS_RECT.contains(x, y) {
        return Some(ClickAction::AdjustChannels(1));
    }
    if PORT_MINUS_RECT.contains(x, y) {
        return Some(ClickAction::AdjustPort(-10));
    }
    if PORT_PLUS_RECT.contains(x, y) {
        return Some(ClickAction::AdjustPort(10));
    }
    if PTP_MINUS_RECT.contains(x, y) {
        return Some(ClickAction::AdjustPtpDomain(-1));
    }
    if PTP_PLUS_RECT.contains(x, y) {
        return Some(ClickAction::AdjustPtpDomain(1));
    }
    for (index, rect) in IP_MINUS_RECTS.iter().enumerate() {
        if rect.contains(x, y) {
            return Some(ClickAction::AdjustIp(index, -1));
        }
    }
    for (index, rect) in IP_PLUS_RECTS.iter().enumerate() {
        if rect.contains(x, y) {
            return Some(ClickAction::AdjustIp(index, 1));
        }
    }
    if APPLY_RECT.contains(x, y) {
        return Some(ClickAction::Apply);
    }
    None
}

fn apply_stream_parameters(controller: EditorControllerApi, params: StreamParameters) {
    apply_param(
        controller,
        PARAM_MODE,
        parameter_spec(PARAM_MODE)
            .unwrap()
            .plain_to_normalized(params.mode.as_u8() as u32),
    );
    apply_param(
        controller,
        PARAM_TRANSPORT,
        parameter_spec(PARAM_TRANSPORT)
            .unwrap()
            .plain_to_normalized(params.transport.as_u8() as u32),
    );
    apply_param(
        controller,
        PARAM_CLOCK_REF,
        parameter_spec(PARAM_CLOCK_REF)
            .unwrap()
            .plain_to_normalized(params.clock_reference.as_u8() as u32),
    );
    apply_param(
        controller,
        PARAM_PTP_DOMAIN,
        parameter_spec(PARAM_PTP_DOMAIN)
            .unwrap()
            .plain_to_normalized(params.ptp_domain as u32),
    );
    apply_param(
        controller,
        PARAM_CHANNELS,
        parameter_spec(PARAM_CHANNELS)
            .unwrap()
            .plain_to_normalized(params.channels as u32),
    );
    apply_param(
        controller,
        PARAM_PORT,
        parameter_spec(PARAM_PORT)
            .unwrap()
            .plain_to_normalized(params.port as u32),
    );
    apply_param(
        controller,
        PARAM_IP_1,
        parameter_spec(PARAM_IP_1)
            .unwrap()
            .plain_to_normalized(params.ip[0] as u32),
    );
    apply_param(
        controller,
        PARAM_IP_2,
        parameter_spec(PARAM_IP_2)
            .unwrap()
            .plain_to_normalized(params.ip[1] as u32),
    );
    apply_param(
        controller,
        PARAM_IP_3,
        parameter_spec(PARAM_IP_3)
            .unwrap()
            .plain_to_normalized(params.ip[2] as u32),
    );
    apply_param(
        controller,
        PARAM_IP_4,
        parameter_spec(PARAM_IP_4)
            .unwrap()
            .plain_to_normalized(params.ip[3] as u32),
    );
}

fn apply_param(controller: EditorControllerApi, param: u32, value: f64) {
    unsafe {
        (controller.apply_ui_parameter)(controller.controller, param, value);
    }
}

fn step_u8(current: u8, delta: i32, min: u8, max: u8) -> u8 {
    ((current as i32) + delta).clamp(min as i32, max as i32) as u8
}

fn step_u16(current: u16, delta: i32, min: u16, max: u16) -> u16 {
    ((current as i32) + delta).clamp(min as i32, max as i32) as u16
}

pub(crate) fn rgb(r: f64, g: f64, b: f64) -> Rgb {
    Rgb {
        r: (r * 255.0).round() as u8,
        g: (g * 255.0).round() as u8,
        b: (b * 255.0).round() as u8,
    }
}

pub(crate) fn endpoint_label_text(params: StreamParameters) -> &'static str {
    match (params.mode, params.transport) {
        (StreamMode::Send, StreamTransport::Unicast) => "Destination IP",
        (StreamMode::Send, StreamTransport::Multicast) => "Group IP",
        (StreamMode::Receive, StreamTransport::Unicast) => "Expected Source",
        (StreamMode::Receive, StreamTransport::Multicast) => "Group IP",
    }
}

pub(crate) fn endpoint_hint_text(params: StreamParameters) -> &'static str {
    match (params.mode, params.transport) {
        (StreamMode::Send, StreamTransport::Unicast) => "Send directly to one host",
        (StreamMode::Send, StreamTransport::Multicast) => {
            "Publish once for many downstream receivers"
        }
        (StreamMode::Receive, StreamTransport::Unicast) => {
            "0.0.0.0 accepts any sender on this port"
        }
        (StreamMode::Receive, StreamTransport::Multicast) => {
            "Join this multicast group on the local interface"
        }
    }
}

pub(crate) fn arm_rect() -> Rect {
    ARM_RECT
}
pub(crate) fn mode_send_rect() -> Rect {
    MODE_SEND_RECT
}
pub(crate) fn mode_recv_rect() -> Rect {
    MODE_RECV_RECT
}
pub(crate) fn transport_uni_rect() -> Rect {
    TRANSPORT_UNI_RECT
}
pub(crate) fn transport_multi_rect() -> Rect {
    TRANSPORT_MULTI_RECT
}
pub(crate) fn clock_local_rect() -> Rect {
    CLOCK_LOCAL_RECT
}
pub(crate) fn clock_ptp_rect() -> Rect {
    CLOCK_PTP_RECT
}
pub(crate) fn channels_minus_rect() -> Rect {
    CHANNELS_MINUS_RECT
}
pub(crate) fn channels_value_rect() -> Rect {
    CHANNELS_VALUE_RECT
}
pub(crate) fn channels_plus_rect() -> Rect {
    CHANNELS_PLUS_RECT
}
pub(crate) fn port_minus_rect() -> Rect {
    PORT_MINUS_RECT
}
pub(crate) fn port_value_rect() -> Rect {
    PORT_VALUE_RECT
}
pub(crate) fn port_plus_rect() -> Rect {
    PORT_PLUS_RECT
}
pub(crate) fn ptp_minus_rect() -> Rect {
    PTP_MINUS_RECT
}
pub(crate) fn ptp_value_rect() -> Rect {
    PTP_VALUE_RECT
}
pub(crate) fn ptp_plus_rect() -> Rect {
    PTP_PLUS_RECT
}
pub(crate) fn ip_minus_rect(index: usize) -> Rect {
    IP_MINUS_RECTS[index]
}
pub(crate) fn ip_value_rect(index: usize) -> Rect {
    IP_VALUE_RECTS[index]
}
pub(crate) fn ip_plus_rect(index: usize) -> Rect {
    IP_PLUS_RECTS[index]
}
pub(crate) fn apply_rect() -> Rect {
    APPLY_RECT
}
pub(crate) fn runtime_panel_rect() -> Rect {
    RUNTIME_PANEL_RECT
}
