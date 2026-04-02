#![allow(unsafe_op_in_unsafe_fn)]

use std::cell::RefCell;
use std::ffi::CStr;
use std::ffi::c_char;
use std::ffi::c_void;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSButton, NSColor, NSControlStateValueOff, NSControlStateValueOn,
    NSSegmentedControl, NSTextField, NSView,
};
use objc2_foundation::{NSPoint, NSSize, NSString, NSTimer, ns_string};
use objc2_quartz_core::CALayer;
use vst3::{
    Class, ComWrapper,
    Steinberg::{
        FIDString, IPlugFrame, IPlugView, IPlugViewTrait, TBool, ViewRect, kInvalidArgument,
        kPlatformTypeNSView, kResultFalse, kResultOk, kResultTrue, tresult,
    },
};

use crate::{
    editor_api::EditorControllerApi,
    network::{MAX_CHANNELS, StreamMode, StreamTransport},
    params::{
        PARAM_CHANNELS, PARAM_ENABLED, PARAM_IP_1, PARAM_IP_2, PARAM_IP_3, PARAM_IP_4, PARAM_MODE,
        PARAM_PORT, PARAM_TRANSPORT, parameter_spec,
    },
};

const VIEW_WIDTH: f64 = 500.0;
const VIEW_HEIGHT: f64 = 392.0;

struct EditorUi {
    root: Retained<NSView>,
    timer: Retained<NSTimer>,
    _target: Retained<EditorTarget>,
}

struct MacEditorView {
    controller: EditorControllerApi,
    ui: RefCell<Option<EditorUi>>,
}

impl Class for MacEditorView {
    type Interfaces = (IPlugView,);
}

impl MacEditorView {
    fn new(controller: EditorControllerApi) -> Self {
        Self {
            controller,
            ui: RefCell::new(None),
        }
    }
}

impl IPlugViewTrait for MacEditorView {
    unsafe fn isPlatformTypeSupported(&self, r#type: FIDString) -> tresult {
        if fid_string_matches(r#type, kPlatformTypeNSView) {
            kResultTrue
        } else {
            kResultFalse
        }
    }

    unsafe fn attached(&self, parent: *mut c_void, r#type: FIDString) -> tresult {
        if !fid_string_matches(r#type, kPlatformTypeNSView) || parent.is_null() {
            return kResultFalse;
        }

        let Some(mtm) = MainThreadMarker::new() else {
            return kResultFalse;
        };

        // The host passes an NSView parent for `kPlatformTypeNSView` attachment on macOS.
        let parent_view = &*(parent as *mut NSView);
        let ui = build_editor_ui(self.controller, mtm);
        ui.root.setFrame(parent_view.bounds());
        ui.root.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        ui.root.setAutoresizesSubviews(true);
        ui.root.setNeedsDisplay(true);
        parent_view.addSubview(&ui.root);
        *self.ui.borrow_mut() = Some(ui);

        kResultOk
    }

    unsafe fn removed(&self) -> tresult {
        if let Some(ui) = self.ui.borrow_mut().take() {
            ui.timer.invalidate();
            ui.root.removeFromSuperview();
        }
        kResultOk
    }

    unsafe fn onWheel(&self, _distance: f32) -> tresult {
        kResultFalse
    }

    unsafe fn onKeyDown(&self, _key: u16, _keyCode: i16, _modifiers: i16) -> tresult {
        kResultFalse
    }

    unsafe fn onKeyUp(&self, _key: u16, _keyCode: i16, _modifiers: i16) -> tresult {
        kResultFalse
    }

    unsafe fn getSize(&self, size: *mut ViewRect) -> tresult {
        if size.is_null() {
            return kInvalidArgument;
        }

        (*size).left = 0;
        (*size).top = 0;
        (*size).right = VIEW_WIDTH as i32;
        (*size).bottom = VIEW_HEIGHT as i32;
        kResultOk
    }

    unsafe fn onSize(&self, newSize: *mut ViewRect) -> tresult {
        if newSize.is_null() {
            return kInvalidArgument;
        }

        if let Some(ui) = self.ui.borrow().as_ref() {
            let width = ((*newSize).right - (*newSize).left) as f64;
            let height = ((*newSize).bottom - (*newSize).top) as f64;
            ui.root.setFrameSize(NSSize::new(width, height));
        }

        kResultOk
    }

    unsafe fn onFocus(&self, _state: TBool) -> tresult {
        kResultOk
    }

    unsafe fn setFrame(&self, _frame: *mut IPlugFrame) -> tresult {
        kResultOk
    }

    unsafe fn canResize(&self) -> tresult {
        kResultFalse
    }

    unsafe fn checkSizeConstraint(&self, rect: *mut ViewRect) -> tresult {
        if rect.is_null() {
            return kInvalidArgument;
        }

        (*rect).left = 0;
        (*rect).top = 0;
        (*rect).right = VIEW_WIDTH as i32;
        (*rect).bottom = VIEW_HEIGHT as i32;
        kResultTrue
    }
}

struct EditorTargetIvars {
    controller: EditorControllerApi,
    enabled: Retained<NSButton>,
    mode: Retained<NSSegmentedControl>,
    transport: Retained<NSSegmentedControl>,
    channels: Retained<NSTextField>,
    port: Retained<NSTextField>,
    ip: [Retained<NSTextField>; 4],
    endpoint_label: Retained<NSTextField>,
    endpoint_hint: Retained<NSTextField>,
    status: [Retained<NSTextField>; 4],
}

struct EditorControls {
    enabled: Retained<NSButton>,
    mode: Retained<NSSegmentedControl>,
    transport: Retained<NSSegmentedControl>,
    channels: Retained<NSTextField>,
    port: Retained<NSTextField>,
    ip: [Retained<NSTextField>; 4],
    endpoint_label: Retained<NSTextField>,
    endpoint_hint: Retained<NSTextField>,
    status: [Retained<NSTextField>; 4],
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = EditorTargetIvars]
    struct EditorTarget;

    unsafe impl NSObjectProtocol for EditorTarget {}

    impl EditorTarget {
        #[unsafe(method(modeChanged:))]
        fn mode_changed(&self, _sender: Option<&AnyObject>) {
            sync_transport_semantics(self);
        }

        #[unsafe(method(transportChanged:))]
        fn transport_changed(&self, _sender: Option<&AnyObject>) {
            sync_transport_semantics(self);
        }

        #[unsafe(method(applyPressed:))]
        fn apply_pressed(&self, _sender: Option<&AnyObject>) {
            let controller = self.ivars().controller;

            let enabled = if self.ivars().enabled.state() == NSControlStateValueOn {
                1
            } else {
                0
            };
            unsafe {
                (controller.apply_ui_parameter)(
                    controller.controller,
                    PARAM_ENABLED,
                    parameter_spec(PARAM_ENABLED)
                        .unwrap()
                        .plain_to_normalized(enabled),
                );
            }

            let mode = if self.ivars().mode.selectedSegment() == 1 { 1 } else { 0 };
            unsafe {
                (controller.apply_ui_parameter)(
                    controller.controller,
                    PARAM_MODE,
                    parameter_spec(PARAM_MODE).unwrap().plain_to_normalized(mode),
                );
            }

            let transport = if self.ivars().transport.selectedSegment() == 1 {
                1
            } else {
                0
            };
            unsafe {
                (controller.apply_ui_parameter)(
                    controller.controller,
                    PARAM_TRANSPORT,
                    parameter_spec(PARAM_TRANSPORT)
                        .unwrap()
                        .plain_to_normalized(transport),
                );
            }

            let current = unsafe { (controller.parameters)(controller.controller) };
            let channels = parse_field_u32(&self.ivars().channels)
                .unwrap_or(current.channels as u32)
                .clamp(1, MAX_CHANNELS as u32);
            unsafe {
                (controller.apply_ui_parameter)(
                    controller.controller,
                    PARAM_CHANNELS,
                    parameter_spec(PARAM_CHANNELS)
                        .unwrap()
                        .plain_to_normalized(channels),
                );
            }

            let port = parse_field_u32(&self.ivars().port)
                .unwrap_or(current.port as u32)
                .clamp(1, u16::MAX as u32);
            unsafe {
                (controller.apply_ui_parameter)(
                    controller.controller,
                    PARAM_PORT,
                    parameter_spec(PARAM_PORT).unwrap().plain_to_normalized(port),
                );
            }

            for (index, field) in self.ivars().ip.iter().enumerate() {
                let value = parse_field_u32(field)
                    .unwrap_or(current.ip[index] as u32)
                    .clamp(0, 255);
                let param = match index {
                    0 => PARAM_IP_1,
                    1 => PARAM_IP_2,
                    2 => PARAM_IP_3,
                    _ => PARAM_IP_4,
                };
                unsafe {
                    (controller.apply_ui_parameter)(
                        controller.controller,
                        param,
                        parameter_spec(param).unwrap().plain_to_normalized(value),
                    );
                }
            }

            unsafe {
                (controller.trigger_apply_reset)(controller.controller);
            }

            sync_controls_from_controller(self);
            sync_status_from_controller(self);
        }

        #[unsafe(method(timerFired:))]
        fn timer_fired(&self, _timer: Option<&NSTimer>) {
            sync_status_from_controller(self);
        }
    }
);

impl EditorTarget {
    fn new(
        controller: EditorControllerApi,
        controls: EditorControls,
        mtm: MainThreadMarker,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(EditorTargetIvars {
            controller,
            enabled: controls.enabled,
            mode: controls.mode,
            transport: controls.transport,
            channels: controls.channels,
            port: controls.port,
            ip: controls.ip,
            endpoint_label: controls.endpoint_label,
            endpoint_hint: controls.endpoint_hint,
            status: controls.status,
        });
        unsafe { msg_send![super(this), init] }
    }
}

pub(crate) fn create_editor_view(controller: EditorControllerApi) -> *mut IPlugView {
    ComWrapper::new(MacEditorView::new(controller))
        .to_com_ptr::<IPlugView>()
        .unwrap()
        .into_raw()
}

fn build_editor_ui(controller: EditorControllerApi, mtm: MainThreadMarker) -> EditorUi {
    let root = NSView::new(mtm);
    set_frame(&root, 0.0, 0.0, VIEW_WIDTH, VIEW_HEIGHT);
    root.setWantsLayer(true);
    if let Some(layer) = root.layer() {
        configure_root_layer(&layer);
    }

    let title = label("SOMETHINGNET", 24.0, 348.0, 240.0, 24.0, mtm);
    let subtitle = label(
        "Minimal network audio sender / receiver",
        24.0,
        326.0,
        320.0,
        18.0,
        mtm,
    );
    let top_rule = separator(24.0, 314.0, 452.0, mtm);

    let enabled = unsafe {
        NSButton::checkboxWithTitle_target_action(ns_string!("Enabled"), None, None, mtm)
    };
    set_frame(&enabled, 24.0, 280.0, 120.0, 24.0);

    let mode_label = label("Mode", 160.0, 282.0, 48.0, 18.0, mtm);
    let mode = NSSegmentedControl::new(mtm);
    set_frame(&mode, 214.0, 276.0, 176.0, 28.0);
    mode.setSegmentCount(2);
    mode.setLabel_forSegment(ns_string!("Send"), 0);
    mode.setLabel_forSegment(ns_string!("Receive"), 1);

    let transport_label = label("Transport", 24.0, 242.0, 84.0, 18.0, mtm);
    let transport = NSSegmentedControl::new(mtm);
    set_frame(&transport, 110.0, 236.0, 188.0, 28.0);
    transport.setSegmentCount(2);
    transport.setLabel_forSegment(ns_string!("Unicast"), 0);
    transport.setLabel_forSegment(ns_string!("Multicast"), 1);

    let channels_label = label("Channels", 24.0, 198.0, 80.0, 18.0, mtm);
    let channels = field("", 110.0, 192.0, 72.0, 24.0, mtm);
    let port_label = label("Port", 214.0, 198.0, 40.0, 18.0, mtm);
    let port = field("", 258.0, 192.0, 110.0, 24.0, mtm);

    let endpoint_label = label("Destination IP", 24.0, 154.0, 120.0, 18.0, mtm);
    let endpoint_hint = secondary_label("", 150.0, 154.0, 320.0, 18.0, mtm);
    let ip1 = field("", 24.0, 124.0, 70.0, 28.0, mtm);
    let ip2 = field("", 102.0, 124.0, 70.0, 28.0, mtm);
    let ip3 = field("", 180.0, 124.0, 70.0, 28.0, mtm);
    let ip4 = field("", 258.0, 124.0, 70.0, 28.0, mtm);

    let apply =
        unsafe { NSButton::buttonWithTitle_target_action(ns_string!("Apply"), None, None, mtm) };
    set_frame(&apply, 386.0, 120.0, 90.0, 32.0);

    let bottom_rule = separator(24.0, 96.0, 452.0, mtm);
    let debug_title = label("Runtime", 24.0, 72.0, 120.0, 18.0, mtm);
    let status_1 = secondary_label("", 24.0, 52.0, 452.0, 18.0, mtm);
    let status_2 = secondary_label("", 24.0, 34.0, 452.0, 18.0, mtm);
    let status_3 = secondary_label("", 24.0, 16.0, 452.0, 18.0, mtm);
    let status_4 = secondary_label("", 24.0, 0.0, 452.0, 18.0, mtm);

    let target = EditorTarget::new(
        controller,
        EditorControls {
            enabled: enabled.clone(),
            mode: mode.clone(),
            transport: transport.clone(),
            channels: channels.clone(),
            port: port.clone(),
            ip: [ip1.clone(), ip2.clone(), ip3.clone(), ip4.clone()],
            endpoint_label: endpoint_label.clone(),
            endpoint_hint: endpoint_hint.clone(),
            status: [
                status_1.clone(),
                status_2.clone(),
                status_3.clone(),
                status_4.clone(),
            ],
        },
        mtm,
    );

    unsafe {
        let target_obj: &AnyObject = &*(target.as_ref() as *const EditorTarget).cast();
        mode.setTarget(Some(target_obj));
        mode.setAction(Some(sel!(modeChanged:)));
        transport.setTarget(Some(target_obj));
        transport.setAction(Some(sel!(transportChanged:)));
        apply.setTarget(Some(target_obj));
        apply.setAction(Some(sel!(applyPressed:)));
    }

    sync_controls_from_controller(&target);
    sync_status_from_controller(&target);

    let timer = unsafe {
        let target_obj: &AnyObject = &*(target.as_ref() as *const EditorTarget).cast();
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            0.5,
            target_obj,
            sel!(timerFired:),
            None,
            true,
        )
    };

    root.addSubview(&title);
    root.addSubview(&subtitle);
    root.addSubview(&top_rule);
    root.addSubview(&enabled);
    root.addSubview(&mode_label);
    root.addSubview(&mode);
    root.addSubview(&transport_label);
    root.addSubview(&transport);
    root.addSubview(&channels_label);
    root.addSubview(&channels);
    root.addSubview(&port_label);
    root.addSubview(&port);
    root.addSubview(&endpoint_label);
    root.addSubview(&endpoint_hint);
    root.addSubview(&ip1);
    root.addSubview(&ip2);
    root.addSubview(&ip3);
    root.addSubview(&ip4);
    root.addSubview(&apply);
    root.addSubview(&bottom_rule);
    root.addSubview(&debug_title);
    root.addSubview(&status_1);
    root.addSubview(&status_2);
    root.addSubview(&status_3);
    root.addSubview(&status_4);

    EditorUi {
        root,
        timer,
        _target: target,
    }
}

fn sync_controls_from_controller(target: &EditorTarget) {
    let controller = target.ivars().controller;
    let params = unsafe { (controller.parameters)(controller.controller) };

    target.ivars().enabled.setState(if params.enabled {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    target.ivars().mode.setSelectedSegment(match params.mode {
        StreamMode::Send => 0,
        StreamMode::Receive => 1,
    });
    target
        .ivars()
        .transport
        .setSelectedSegment(match params.transport {
            StreamTransport::Unicast => 0,
            StreamTransport::Multicast => 1,
        });
    target
        .ivars()
        .channels
        .setStringValue(&NSString::from_str(&params.channels.to_string()));
    target
        .ivars()
        .port
        .setStringValue(&NSString::from_str(&params.port.to_string()));
    for (field, value) in target.ivars().ip.iter().zip(params.ip) {
        field.setStringValue(&NSString::from_str(&value.to_string()));
    }

    sync_transport_semantics(target);
}

fn sync_status_from_controller(target: &EditorTarget) {
    let controller = target.ivars().controller;
    let lines = unsafe { (controller.runtime_status_lines)(controller.controller) };
    for (field, line) in target.ivars().status.iter().zip(lines.iter()) {
        field.setStringValue(&NSString::from_str(line));
    }
}

fn sync_transport_semantics(target: &EditorTarget) {
    let mode = if target.ivars().mode.selectedSegment() == 1 {
        StreamMode::Receive
    } else {
        StreamMode::Send
    };
    let transport = if target.ivars().transport.selectedSegment() == 1 {
        StreamTransport::Multicast
    } else {
        StreamTransport::Unicast
    };

    let (label_text, hint_text) = match (mode, transport) {
        (StreamMode::Send, StreamTransport::Unicast) => {
            ("Destination IP", "Send directly to one host")
        }
        (StreamMode::Send, StreamTransport::Multicast) => {
            ("Group IP", "Publish once for many downstream receivers")
        }
        (StreamMode::Receive, StreamTransport::Unicast) => {
            ("Expected Source", "0.0.0.0 accepts any sender on this port")
        }
        (StreamMode::Receive, StreamTransport::Multicast) => (
            "Group IP",
            "Join this multicast group on the local interface",
        ),
    };

    target
        .ivars()
        .endpoint_label
        .setStringValue(&NSString::from_str(label_text));
    target
        .ivars()
        .endpoint_hint
        .setStringValue(&NSString::from_str(hint_text));
}

fn parse_field_u32(field: &NSTextField) -> Option<u32> {
    field.stringValue().to_string().trim().parse::<u32>().ok()
}

fn label(
    text: &str,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    label.setTextColor(Some(panel_text_color().as_ref()));
    set_frame(&label, x, y, width, height);
    label
}

fn secondary_label(
    text: &str,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    let label = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    label.setTextColor(Some(panel_secondary_text_color().as_ref()));
    set_frame(&label, x, y, width, height);
    label
}

fn field(
    text: &str,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    let field = NSTextField::textFieldWithString(&NSString::from_str(text), mtm);
    field.setEditable(true);
    field.setBezeled(true);
    field.setDrawsBackground(true);
    field.setTextColor(Some(panel_text_color().as_ref()));
    field.setBackgroundColor(Some(panel_field_background_color().as_ref()));
    set_frame(&field, x, y, width, height);
    field
}

fn set_frame(view: &NSView, x: f64, y: f64, width: f64, height: f64) {
    view.setFrameOrigin(NSPoint::new(x, y));
    view.setFrameSize(NSSize::new(width, height));
}

fn separator(x: f64, y: f64, width: f64, mtm: MainThreadMarker) -> Retained<NSView> {
    let line = NSView::new(mtm);
    set_frame(&line, x, y, width, 1.0);
    line.setWantsLayer(true);
    if let Some(layer) = line.layer() {
        let stroke = panel_rule_color().CGColor();
        layer.setBackgroundColor(Some(stroke.as_ref()));
    }
    line
}

fn fid_string_matches(value: FIDString, expected: FIDString) -> bool {
    if value.is_null() || expected.is_null() {
        return false;
    }

    unsafe {
        CStr::from_ptr(value as *const c_char).to_bytes()
            == CStr::from_ptr(expected as *const c_char).to_bytes()
    }
}

fn configure_root_layer(layer: &CALayer) {
    let background = panel_background_color().CGColor();
    layer.setBackgroundColor(Some(background.as_ref()));
}

fn panel_background_color() -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(0.93, 0.91, 0.86, 1.0)
}

fn panel_field_background_color() -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(0.98, 0.97, 0.94, 1.0)
}

fn panel_text_color() -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(0.12, 0.12, 0.11, 1.0)
}

fn panel_secondary_text_color() -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(0.38, 0.39, 0.36, 1.0)
}

fn panel_rule_color() -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(0.76, 0.73, 0.67, 1.0)
}
