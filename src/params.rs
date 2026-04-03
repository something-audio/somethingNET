use std::ffi::c_char;

use crate::network::{ClockReference, MAX_CHANNELS, StreamMode, StreamParameters, StreamTransport};

pub(crate) const PARAM_ENABLED: u32 = 0;
pub(crate) const PARAM_MODE: u32 = 1;
pub(crate) const PARAM_TRANSPORT: u32 = 2;
pub(crate) const PARAM_CHANNELS: u32 = 3;
pub(crate) const PARAM_PORT: u32 = 4;
pub(crate) const PARAM_IP_1: u32 = 5;
pub(crate) const PARAM_IP_2: u32 = 6;
pub(crate) const PARAM_IP_3: u32 = 7;
pub(crate) const PARAM_IP_4: u32 = 8;
pub(crate) const PARAM_CLOCK_REF: u32 = 9;
pub(crate) const PARAM_PTP_DOMAIN: u32 = 10;
pub(crate) const PARAM_APPLY_SEQ: u32 = 11;
pub(crate) const PARAM_COUNT: i32 = 12;

#[derive(Clone, Copy)]
pub(crate) struct IntParamSpec {
    pub(crate) id: u32,
    pub(crate) title: &'static str,
    pub(crate) short_title: &'static str,
    pub(crate) units: &'static str,
    pub(crate) min: u32,
    pub(crate) max: u32,
    pub(crate) default: u32,
}

impl IntParamSpec {
    pub(crate) fn step_count(self) -> i32 {
        (self.max - self.min) as i32
    }

    pub(crate) fn clamp(self, value: u32) -> u32 {
        value.clamp(self.min, self.max)
    }

    pub(crate) fn normalized_to_plain(self, normalized: f64) -> u32 {
        if self.min == self.max {
            return self.min;
        }

        let span = (self.max - self.min) as f64;
        let clamped = normalized.clamp(0.0, 1.0);
        let plain = self.min as f64 + (clamped * span).round();
        self.clamp(plain as u32)
    }

    pub(crate) fn plain_to_normalized(self, plain: u32) -> f64 {
        if self.min == self.max {
            return 0.0;
        }

        let clamped = self.clamp(plain);
        (clamped - self.min) as f64 / (self.max - self.min) as f64
    }
}

pub(crate) fn default_stream_parameters() -> StreamParameters {
    StreamParameters {
        enabled: false,
        mode: StreamMode::Send,
        transport: StreamTransport::Unicast,
        channels: 2,
        port: 5004,
        ip: [127, 0, 0, 1],
        clock_reference: ClockReference::Local,
        ptp_domain: 0,
    }
}

pub(crate) fn parameter_spec(id: u32) -> Option<IntParamSpec> {
    let defaults = default_stream_parameters();
    let spec = match id {
        PARAM_ENABLED => IntParamSpec {
            id,
            title: "Enabled",
            short_title: "Enable",
            units: "",
            min: 0,
            max: 1,
            default: defaults.enabled as u32,
        },
        PARAM_MODE => IntParamSpec {
            id,
            title: "Mode",
            short_title: "Mode",
            units: "",
            min: 0,
            max: 1,
            default: defaults.mode.as_u8() as u32,
        },
        PARAM_TRANSPORT => IntParamSpec {
            id,
            title: "Transport",
            short_title: "Xport",
            units: "",
            min: 0,
            max: 1,
            default: defaults.transport.as_u8() as u32,
        },
        PARAM_CHANNELS => IntParamSpec {
            id,
            title: "Channels",
            short_title: "Ch",
            units: "ch",
            min: 1,
            max: MAX_CHANNELS as u32,
            default: defaults.channels as u32,
        },
        PARAM_PORT => IntParamSpec {
            id,
            title: "Port",
            short_title: "Port",
            units: "",
            min: 1,
            max: u16::MAX as u32,
            default: defaults.port as u32,
        },
        PARAM_IP_1 => IntParamSpec {
            id,
            title: "IP Octet 1",
            short_title: "IP1",
            units: "",
            min: 0,
            max: 255,
            default: defaults.ip[0] as u32,
        },
        PARAM_IP_2 => IntParamSpec {
            id,
            title: "IP Octet 2",
            short_title: "IP2",
            units: "",
            min: 0,
            max: 255,
            default: defaults.ip[1] as u32,
        },
        PARAM_IP_3 => IntParamSpec {
            id,
            title: "IP Octet 3",
            short_title: "IP3",
            units: "",
            min: 0,
            max: 255,
            default: defaults.ip[2] as u32,
        },
        PARAM_IP_4 => IntParamSpec {
            id,
            title: "IP Octet 4",
            short_title: "IP4",
            units: "",
            min: 0,
            max: 255,
            default: defaults.ip[3] as u32,
        },
        PARAM_CLOCK_REF => IntParamSpec {
            id,
            title: "Clock Reference",
            short_title: "Clock",
            units: "",
            min: 0,
            max: 1,
            default: defaults.clock_reference.as_u8() as u32,
        },
        PARAM_PTP_DOMAIN => IntParamSpec {
            id,
            title: "PTP Domain",
            short_title: "Domain",
            units: "",
            min: 0,
            max: 127,
            default: defaults.ptp_domain as u32,
        },
        PARAM_APPLY_SEQ => IntParamSpec {
            id,
            title: "Apply Sequence",
            short_title: "Apply",
            units: "",
            min: 0,
            max: u16::MAX as u32,
            default: 0,
        },
        _ => return None,
    };
    Some(spec)
}

pub(crate) fn copy_cstring(src: &str, dst: &mut [c_char]) {
    let c_string = std::ffi::CString::new(src).unwrap_or_else(|_| std::ffi::CString::default());
    let bytes = c_string.as_bytes_with_nul();

    for (src, dst) in bytes.iter().zip(dst.iter_mut()) {
        *dst = *src as c_char;
    }

    if bytes.len() > dst.len()
        && let Some(last) = dst.last_mut()
    {
        *last = 0;
    }
}
