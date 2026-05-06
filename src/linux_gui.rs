#![allow(unsafe_op_in_unsafe_fn)]

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::mem::{ManuallyDrop, zeroed};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use vst3::{
    Class, ComWrapper,
    Steinberg::{
        FIDString, IPlugFrame, IPlugView, IPlugViewTrait, TBool, ViewRect, kInvalidArgument,
        kPlatformTypeX11EmbedWindowID, kResultFalse, kResultOk, kResultTrue, tresult,
    },
};

use crate::{
    editor_api::EditorControllerApi,
    generic_gui::{
        EditorState, Rgb, Theme, VIEW_HEIGHT, VIEW_WIDTH, apply_rect, arm_rect,
        channels_minus_rect, channels_plus_rect, channels_value_rect, clock_local_rect,
        clock_ptp_rect, endpoint_hint_text, endpoint_label_text, ip_minus_rect, ip_plus_rect,
        ip_value_rect, mode_recv_rect, mode_send_rect, port_minus_rect, port_plus_rect,
        port_value_rect, ptp_minus_rect, ptp_plus_rect, ptp_value_rect, runtime_panel_rect, theme,
        transport_multi_rect, transport_uni_rect,
    },
    network::{ClockReference, StreamMode},
};

#[repr(C)]
struct Display {
    _private: [u8; 0],
}

type Window = c_ulong;
type GC = *mut c_void;

#[repr(C)]
struct XAnyEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut Display,
    window: Window,
}

#[repr(C)]
struct XButtonEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut Display,
    window: Window,
    root: Window,
    subwindow: Window,
    time: c_ulong,
    x: c_int,
    y: c_int,
    x_root: c_int,
    y_root: c_int,
    state: c_uint,
    button: c_uint,
    same_screen: c_int,
}

#[repr(C)]
union XEvent {
    type_: c_int,
    xany: ManuallyDrop<XAnyEvent>,
    xbutton: ManuallyDrop<XButtonEvent>,
    pad: [u64; 24],
}

unsafe extern "C" {
    fn XOpenDisplay(display_name: *const c_char) -> *mut Display;
    fn XCloseDisplay(display: *mut Display) -> c_int;
    fn XCreateSimpleWindow(
        display: *mut Display,
        parent: Window,
        x: c_int,
        y: c_int,
        width: c_uint,
        height: c_uint,
        border_width: c_uint,
        border: c_ulong,
        background: c_ulong,
    ) -> Window;
    fn XDestroyWindow(display: *mut Display, window: Window) -> c_int;
    fn XMapWindow(display: *mut Display, window: Window) -> c_int;
    fn XSelectInput(display: *mut Display, window: Window, event_mask: c_long) -> c_int;
    fn XPending(display: *mut Display) -> c_int;
    fn XNextEvent(display: *mut Display, event_return: *mut XEvent) -> c_int;
    fn XFlush(display: *mut Display) -> c_int;
    fn XCreateGC(
        display: *mut Display,
        drawable: Window,
        valuemask: c_ulong,
        values: *mut c_void,
    ) -> GC;
    fn XFreeGC(display: *mut Display, gc: GC) -> c_int;
    fn XSetForeground(display: *mut Display, gc: GC, foreground: c_ulong) -> c_int;
    fn XFillRectangle(
        display: *mut Display,
        drawable: Window,
        gc: GC,
        x: c_int,
        y: c_int,
        width: c_uint,
        height: c_uint,
    ) -> c_int;
    fn XDrawRectangle(
        display: *mut Display,
        drawable: Window,
        gc: GC,
        x: c_int,
        y: c_int,
        width: c_uint,
        height: c_uint,
    ) -> c_int;
    fn XDrawString(
        display: *mut Display,
        drawable: Window,
        gc: GC,
        x: c_int,
        y: c_int,
        string: *const c_char,
        length: c_int,
    ) -> c_int;
}

const EXPOSE: c_int = 12;
const DESTROY_NOTIFY: c_int = 17;
const BUTTON_RELEASE: c_int = 5;
const BUTTON_RELEASE_MASK: c_long = 1 << 3;
const EXPOSURE_MASK: c_long = 1 << 15;
const STRUCTURE_NOTIFY_MASK: c_long = 1 << 17;

struct LinuxEditorView {
    controller: EditorControllerApi,
    runtime: RefCell<Option<LinuxRuntime>>,
}

struct LinuxRuntime {
    shared: Arc<SharedState>,
    thread: JoinHandle<()>,
}

struct SharedState {
    editor: Mutex<EditorState>,
    running: AtomicBool,
    redraw: AtomicBool,
}

impl Class for LinuxEditorView {
    type Interfaces = (IPlugView,);
}

impl LinuxEditorView {
    fn new(controller: EditorControllerApi) -> Self {
        Self {
            controller,
            runtime: RefCell::new(None),
        }
    }
}

impl IPlugViewTrait for LinuxEditorView {
    unsafe fn isPlatformTypeSupported(&self, r#type: FIDString) -> tresult {
        if fid_string_matches(r#type, kPlatformTypeX11EmbedWindowID) {
            kResultTrue
        } else {
            kResultFalse
        }
    }

    unsafe fn attached(&self, parent: *mut c_void, r#type: FIDString) -> tresult {
        if !fid_string_matches(r#type, kPlatformTypeX11EmbedWindowID) || parent.is_null() {
            return kResultFalse;
        }

        let parent_window = parent as usize as Window;
        if parent_window == 0 {
            return kResultFalse;
        }

        let shared = Arc::new(SharedState {
            editor: Mutex::new(EditorState::new(self.controller)),
            running: AtomicBool::new(true),
            redraw: AtomicBool::new(true),
        });
        let (tx, rx) = sync_channel(1);
        let thread_shared = Arc::clone(&shared);
        let controller = self.controller;
        let thread = thread::spawn(move || {
            linux_ui_thread(thread_shared, controller, parent_window, tx);
        });

        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(true) => {
                *self.runtime.borrow_mut() = Some(LinuxRuntime { shared, thread });
                kResultOk
            }
            _ => {
                shared.running.store(false, Ordering::Relaxed);
                let _ = thread.join();
                kResultFalse
            }
        }
    }

    unsafe fn removed(&self) -> tresult {
        if let Some(runtime) = self.runtime.borrow_mut().take() {
            runtime.shared.running.store(false, Ordering::Relaxed);
            let _ = runtime.thread.join();
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
        (*size).right = VIEW_WIDTH;
        (*size).bottom = VIEW_HEIGHT;
        kResultOk
    }

    unsafe fn onSize(&self, _newSize: *mut ViewRect) -> tresult {
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
        (*rect).right = VIEW_WIDTH;
        (*rect).bottom = VIEW_HEIGHT;
        kResultTrue
    }
}

pub(crate) fn create_editor_view(controller: EditorControllerApi) -> *mut IPlugView {
    ComWrapper::new(LinuxEditorView::new(controller))
        .to_com_ptr::<IPlugView>()
        .unwrap()
        .into_raw()
}

fn linux_ui_thread(
    shared: Arc<SharedState>,
    controller: EditorControllerApi,
    parent_window: Window,
    ready: std::sync::mpsc::SyncSender<bool>,
) {
    unsafe {
        let display = XOpenDisplay(ptr::null());
        if display.is_null() {
            let _ = ready.send(false);
            return;
        }

        let window = XCreateSimpleWindow(
            display,
            parent_window,
            0,
            0,
            VIEW_WIDTH as c_uint,
            VIEW_HEIGHT as c_uint,
            0,
            0,
            0,
        );
        if window == 0 {
            let _ = ready.send(false);
            XCloseDisplay(display);
            return;
        }

        XSelectInput(
            display,
            window,
            BUTTON_RELEASE_MASK | EXPOSURE_MASK | STRUCTURE_NOTIFY_MASK,
        );
        XMapWindow(display, window);
        XFlush(display);
        let _ = ready.send(true);

        let mut last_status = Instant::now();
        while shared.running.load(Ordering::Relaxed) {
            while XPending(display) > 0 {
                let mut event: XEvent = zeroed();
                XNextEvent(display, &mut event);
                match event.type_ {
                    EXPOSE => {
                        shared.redraw.store(true, Ordering::Relaxed);
                    }
                    BUTTON_RELEASE => {
                        let button = &event.xbutton;
                        if let Ok(mut editor) = shared.editor.lock() {
                            if editor.handle_click(controller, button.x, button.y) {
                                shared.redraw.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                    DESTROY_NOTIFY => {
                        shared.running.store(false, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }

            if last_status.elapsed() >= Duration::from_millis(500) {
                if let Ok(mut editor) = shared.editor.lock() {
                    editor.refresh_status(controller);
                }
                shared.redraw.store(true, Ordering::Relaxed);
                last_status = Instant::now();
            }

            if shared.redraw.swap(false, Ordering::AcqRel) {
                if let Ok(editor) = shared.editor.lock() {
                    draw_editor(display, window, &editor);
                    XFlush(display);
                }
            }

            thread::sleep(Duration::from_millis(16));
        }

        XDestroyWindow(display, window);
        XCloseDisplay(display);
    }
}

unsafe fn draw_editor(display: *mut Display, window: Window, editor: &EditorState) {
    let gc = XCreateGC(display, window, 0, ptr::null_mut());
    if gc.is_null() {
        return;
    }

    let theme = theme(editor.params.mode);
    fill_rect(
        display,
        window,
        gc,
        0,
        0,
        VIEW_WIDTH,
        VIEW_HEIGHT,
        theme.background,
    );

    draw_text(display, window, gc, "SomeNET", 24, 42, theme.text);
    draw_text(
        display,
        window,
        gc,
        if matches!(editor.params.mode, StreamMode::Send) {
            "SEND"
        } else {
            "RECEIVE"
        },
        152,
        44,
        theme.accent,
    );

    draw_button(
        display,
        window,
        gc,
        arm_rect(),
        "ARM",
        editor.params.enabled,
        theme,
    );

    draw_label(display, window, gc, "Mode", 24, 132, theme);
    draw_button(
        display,
        window,
        gc,
        mode_send_rect(),
        "Send",
        matches!(editor.params.mode, StreamMode::Send),
        theme,
    );
    draw_button(
        display,
        window,
        gc,
        mode_recv_rect(),
        "Receive",
        matches!(editor.params.mode, StreamMode::Receive),
        theme,
    );

    draw_label(display, window, gc, "Transport", 236, 132, theme);
    draw_button(
        display,
        window,
        gc,
        transport_uni_rect(),
        "Unicast",
        editor.params.transport.as_u8() == 0,
        theme,
    );
    draw_button(
        display,
        window,
        gc,
        transport_multi_rect(),
        "Multicast",
        editor.params.transport.as_u8() == 1,
        theme,
    );

    draw_label(display, window, gc, "Clock", 24, 178, theme);
    draw_button(
        display,
        window,
        gc,
        clock_local_rect(),
        "Local",
        matches!(editor.params.clock_reference, ClockReference::Local),
        theme,
    );
    draw_button(
        display,
        window,
        gc,
        clock_ptp_rect(),
        "PTP",
        matches!(editor.params.clock_reference, ClockReference::Ptp),
        theme,
    );

    draw_label(display, window, gc, "PTP Domain", 236, 178, theme);
    draw_stepper(
        display,
        window,
        gc,
        ptp_minus_rect(),
        ptp_value_rect(),
        ptp_plus_rect(),
        &editor.params.ptp_domain.to_string(),
        matches!(editor.params.clock_reference, ClockReference::Ptp),
        theme,
    );

    draw_label(display, window, gc, "Channels", 24, 228, theme);
    draw_stepper(
        display,
        window,
        gc,
        channels_minus_rect(),
        channels_value_rect(),
        channels_plus_rect(),
        &editor.params.channels.to_string(),
        true,
        theme,
    );

    draw_label(display, window, gc, "Port", 214, 228, theme);
    draw_stepper(
        display,
        window,
        gc,
        port_minus_rect(),
        port_value_rect(),
        port_plus_rect(),
        &editor.params.port.to_string(),
        true,
        theme,
    );

    draw_label(
        display,
        window,
        gc,
        endpoint_label_text(editor.params),
        24,
        274,
        theme,
    );
    draw_text(
        display,
        window,
        gc,
        endpoint_hint_text(editor.params),
        150,
        274,
        theme.secondary_text,
    );

    for index in 0..4 {
        draw_stepper(
            display,
            window,
            gc,
            ip_minus_rect(index),
            ip_value_rect(index),
            ip_plus_rect(index),
            &editor.params.ip[index].to_string(),
            true,
            theme,
        );
    }

    draw_button(display, window, gc, apply_rect(), "Apply", false, theme);

    let runtime = runtime_panel_rect();
    draw_panel(display, window, gc, runtime, theme);
    draw_label(display, window, gc, "Runtime", 24, 364, theme);
    for (index, line) in editor.status.iter().enumerate() {
        draw_text(
            display,
            window,
            gc,
            line,
            runtime.x + 16,
            runtime.y + 34 + (index as i32 * 20),
            theme.text,
        );
    }

    XFreeGC(display, gc);
}

unsafe fn draw_label(
    display: *mut Display,
    window: Window,
    gc: GC,
    text: &str,
    x: i32,
    y: i32,
    theme: Theme,
) {
    draw_text(display, window, gc, text, x, y, theme.text);
}

unsafe fn draw_button(
    display: *mut Display,
    window: Window,
    gc: GC,
    rect: crate::generic_gui::Rect,
    text: &str,
    selected: bool,
    theme: Theme,
) {
    let fill = if selected {
        theme.selected_fill
    } else {
        theme.field_fill
    };
    draw_panel_fill(display, window, gc, rect, fill, theme.rule);
    draw_centered_text(
        display,
        window,
        gc,
        text,
        rect,
        if selected { theme.accent } else { theme.text },
    );
}

unsafe fn draw_stepper(
    display: *mut Display,
    window: Window,
    gc: GC,
    minus: crate::generic_gui::Rect,
    value: crate::generic_gui::Rect,
    plus: crate::generic_gui::Rect,
    text: &str,
    enabled: bool,
    theme: Theme,
) {
    let effective = if enabled {
        theme
    } else {
        Theme {
            secondary_text: theme.secondary_text,
            text: theme.secondary_text,
            selected_fill: theme.field_fill,
            accent: theme.secondary_text,
            ..theme
        }
    };
    draw_button(display, window, gc, minus, "-", false, effective);
    draw_panel_fill(
        display,
        window,
        gc,
        value,
        effective.field_fill,
        effective.rule,
    );
    draw_centered_text(display, window, gc, text, value, effective.text);
    draw_button(display, window, gc, plus, "+", false, effective);
}

unsafe fn draw_panel(
    display: *mut Display,
    window: Window,
    gc: GC,
    rect: crate::generic_gui::Rect,
    theme: Theme,
) {
    draw_panel_fill(display, window, gc, rect, theme.panel_fill, theme.rule);
}

unsafe fn draw_panel_fill(
    display: *mut Display,
    window: Window,
    gc: GC,
    rect: crate::generic_gui::Rect,
    fill: Rgb,
    stroke: Rgb,
) {
    fill_rect(display, window, gc, rect.x, rect.y, rect.w, rect.h, fill);
    stroke_rect(display, window, gc, rect.x, rect.y, rect.w, rect.h, stroke);
}

unsafe fn fill_rect(
    display: *mut Display,
    window: Window,
    gc: GC,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: Rgb,
) {
    XSetForeground(display, gc, rgb_to_pixel(color));
    XFillRectangle(
        display,
        window,
        gc,
        x,
        y,
        w.max(0) as c_uint,
        h.max(0) as c_uint,
    );
}

unsafe fn stroke_rect(
    display: *mut Display,
    window: Window,
    gc: GC,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: Rgb,
) {
    XSetForeground(display, gc, rgb_to_pixel(color));
    XDrawRectangle(
        display,
        window,
        gc,
        x,
        y,
        w.saturating_sub(1) as c_uint,
        h.saturating_sub(1) as c_uint,
    );
}

unsafe fn draw_text(
    display: *mut Display,
    window: Window,
    gc: GC,
    text: &str,
    x: i32,
    y: i32,
    color: Rgb,
) {
    let Ok(cstring) = CString::new(text.replace('\0', " ")) else {
        return;
    };
    XSetForeground(display, gc, rgb_to_pixel(color));
    XDrawString(
        display,
        window,
        gc,
        x,
        y,
        cstring.as_ptr(),
        cstring.as_bytes().len() as c_int,
    );
}

unsafe fn draw_centered_text(
    display: *mut Display,
    window: Window,
    gc: GC,
    text: &str,
    rect: crate::generic_gui::Rect,
    color: Rgb,
) {
    let x = rect.x + 8;
    let y = rect.y + (rect.h / 2) + 5;
    draw_text(display, window, gc, text, x, y, color);
}

fn rgb_to_pixel(rgb: Rgb) -> c_ulong {
    ((rgb.r as c_ulong) << 16) | ((rgb.g as c_ulong) << 8) | (rgb.b as c_ulong)
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
