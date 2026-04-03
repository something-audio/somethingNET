#![allow(unsafe_op_in_unsafe_fn)]

use std::cell::Cell;
use std::ffi::{c_char, c_void};
use std::ptr;
use std::sync::OnceLock;

use vst3::{
    Class, ComWrapper,
    Steinberg::{
        FIDString, IPlugFrame, IPlugView, IPlugViewTrait, TBool, ViewRect, kInvalidArgument,
        kPlatformTypeHWND, kResultFalse, kResultOk, kResultTrue, tresult,
    },
};
use windows_sys::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DEFAULT_GUI_FONT, DT_CENTER, DT_LEFT, DT_SINGLELINE, DT_VCENTER,
    DeleteObject, DrawTextW, EndPaint, FillRect, FrameRect, GetStockObject, HDC, InvalidateRect,
    PAINTSTRUCT, SelectObject, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow,
    GWLP_USERDATA, GetClientRect, GetWindowLongPtrW, IDC_ARROW, LoadCursorW, RegisterClassW,
    SW_SHOW, SetTimer, SetWindowLongPtrW, ShowWindow, WM_LBUTTONUP, WM_NCCREATE, WM_NCDESTROY,
    WM_PAINT, WM_TIMER, WNDCLASSW, WS_CHILD, WS_VISIBLE,
};

use crate::{
    editor_api::EditorControllerApi,
    generic_gui::{
        EditorState, VIEW_HEIGHT, VIEW_WIDTH, apply_rect, arm_rect, channels_minus_rect,
        channels_plus_rect, channels_value_rect, clock_local_rect, clock_ptp_rect,
        endpoint_hint_text, endpoint_label_text, ip_minus_rect, ip_plus_rect, ip_value_rect,
        mode_recv_rect, mode_send_rect, port_minus_rect, port_plus_rect, port_value_rect,
        ptp_minus_rect, ptp_plus_rect, ptp_value_rect, runtime_panel_rect, theme,
        transport_multi_rect, transport_uni_rect,
    },
    network::{ClockReference, StreamMode, StreamTransport},
};

const TIMER_ID: usize = 1;
const WINDOW_CLASS_NAME: &str = "SomethingNetEditorWindow";

struct WinEditorView {
    controller: EditorControllerApi,
    hwnd: Cell<HWND>,
}

impl Class for WinEditorView {
    type Interfaces = (IPlugView,);
}

struct WindowState {
    controller: EditorControllerApi,
    editor: EditorState,
}

impl WinEditorView {
    fn new(controller: EditorControllerApi) -> Self {
        Self {
            controller,
            hwnd: Cell::new(ptr::null_mut()),
        }
    }
}

impl IPlugViewTrait for WinEditorView {
    unsafe fn isPlatformTypeSupported(&self, r#type: FIDString) -> tresult {
        if fid_string_matches(r#type, kPlatformTypeHWND) {
            kResultTrue
        } else {
            kResultFalse
        }
    }

    unsafe fn attached(&self, parent: *mut c_void, r#type: FIDString) -> tresult {
        if !fid_string_matches(r#type, kPlatformTypeHWND) || parent.is_null() {
            return kResultFalse;
        }

        let parent_hwnd = parent as HWND;
        if parent_hwnd.is_null() {
            return kResultFalse;
        }

        let instance = GetModuleHandleW(ptr::null());
        let Some(class_name) = wide_null(WINDOW_CLASS_NAME) else {
            return kResultFalse;
        };
        register_window_class(instance, &class_name);

        let state = Box::new(WindowState {
            controller: self.controller,
            editor: EditorState::new(self.controller),
        });
        let state_ptr = Box::into_raw(state);

        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            class_name.as_ptr(),
            WS_CHILD | WS_VISIBLE,
            0,
            0,
            VIEW_WIDTH,
            VIEW_HEIGHT,
            parent_hwnd,
            ptr::null_mut(),
            instance,
            state_ptr.cast(),
        );

        if hwnd.is_null() {
            let _ = Box::from_raw(state_ptr);
            return kResultFalse;
        }

        ShowWindow(hwnd, SW_SHOW);
        SetTimer(hwnd, TIMER_ID, 500, None);
        self.hwnd.set(hwnd);
        kResultOk
    }

    unsafe fn removed(&self) -> tresult {
        let hwnd = self.hwnd.replace(ptr::null_mut());
        if !hwnd.is_null() {
            DestroyWindow(hwnd);
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
    ComWrapper::new(WinEditorView::new(controller))
        .to_com_ptr::<IPlugView>()
        .unwrap()
        .into_raw()
}

unsafe extern "system" fn editor_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let create = &*(lparam as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            1
        }
        WM_LBUTTONUP => {
            if let Some(state) = state_mut(hwnd) {
                let x = (lparam as u32 & 0xffff) as i16 as i32;
                let y = ((lparam as u32 >> 16) & 0xffff) as i16 as i32;
                if state.editor.handle_click(state.controller, x, y) {
                    InvalidateRect(hwnd, ptr::null(), 1);
                }
            }
            0
        }
        WM_TIMER => {
            if let Some(state) = state_mut(hwnd) {
                state.editor.refresh_status(state.controller);
                InvalidateRect(hwnd, ptr::null(), 1);
            }
            0
        }
        WM_PAINT => {
            let mut paint = PAINTSTRUCT {
                hdc: ptr::null_mut(),
                fErase: 0,
                rcPaint: RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                },
                fRestore: 0,
                fIncUpdate: 0,
                rgbReserved: [0; 32],
            };
            let hdc = BeginPaint(hwnd, &mut paint);
            if let Some(state) = state_ref(hwnd) {
                draw_editor(hwnd, hdc, &state.editor);
            }
            EndPaint(hwnd, &paint);
            0
        }
        WM_NCDESTROY => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState;
            if !ptr.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let _ = Box::from_raw(ptr);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn register_window_class(instance: HINSTANCE, class_name: &[u16]) {
    static REGISTERED: OnceLock<()> = OnceLock::new();
    REGISTERED.get_or_init(|| unsafe {
        let wnd = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(editor_window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: ptr::null_mut(),
            hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
            hbrBackground: ptr::null_mut(),
            lpszMenuName: ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        let _ = RegisterClassW(&wnd);
    });
}

unsafe fn state_ref(hwnd: HWND) -> Option<&'static WindowState> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowState;
    ptr.as_ref()
}

unsafe fn state_mut(hwnd: HWND) -> Option<&'static mut WindowState> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState;
    ptr.as_mut()
}

unsafe fn draw_editor(hwnd: HWND, hdc: HDC, editor: &EditorState) {
    let theme = theme(editor.params.mode);
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    GetClientRect(hwnd, &mut rect);
    fill_rect(hdc, rect, rgb_to_colorref(theme.background));

    let old_font = SelectObject(hdc, GetStockObject(DEFAULT_GUI_FONT));
    SetBkMode(hdc, TRANSPARENT as i32);
    SetTextColor(hdc, rgb_to_colorref(theme.text));

    draw_text(
        hdc,
        "SOMETHINGNET",
        24,
        34,
        220,
        22,
        theme.text,
        false,
        DT_LEFT,
    );
    draw_text(
        hdc,
        if matches!(editor.params.mode, StreamMode::Send) {
            "SEND"
        } else {
            "RECEIVE"
        },
        152,
        38,
        96,
        18,
        theme.accent,
        false,
        DT_LEFT,
    );

    draw_button(hdc, arm_rect(), "ARM", editor.params.enabled, theme);

    draw_label(hdc, "Mode", 24, 118, theme);
    draw_button(
        hdc,
        mode_send_rect(),
        "Send",
        matches!(editor.params.mode, StreamMode::Send),
        theme,
    );
    draw_button(
        hdc,
        mode_recv_rect(),
        "Receive",
        matches!(editor.params.mode, StreamMode::Receive),
        theme,
    );

    draw_label(hdc, "Transport", 236, 118, theme);
    draw_button(
        hdc,
        transport_uni_rect(),
        "Unicast",
        matches!(editor.params.transport, StreamTransport::Unicast),
        theme,
    );
    draw_button(
        hdc,
        transport_multi_rect(),
        "Multicast",
        matches!(editor.params.transport, StreamTransport::Multicast),
        theme,
    );

    draw_label(hdc, "Clock", 24, 164, theme);
    draw_button(
        hdc,
        clock_local_rect(),
        "Local",
        matches!(editor.params.clock_reference, ClockReference::Local),
        theme,
    );
    draw_button(
        hdc,
        clock_ptp_rect(),
        "PTP",
        matches!(editor.params.clock_reference, ClockReference::Ptp),
        theme,
    );

    draw_label(hdc, "PTP Domain", 236, 164, theme);
    draw_stepper(
        hdc,
        ptp_minus_rect(),
        ptp_value_rect(),
        ptp_plus_rect(),
        &editor.params.ptp_domain.to_string(),
        matches!(editor.params.clock_reference, ClockReference::Ptp),
        theme,
    );

    draw_label(hdc, "Channels", 24, 214, theme);
    draw_stepper(
        hdc,
        channels_minus_rect(),
        channels_value_rect(),
        channels_plus_rect(),
        &editor.params.channels.to_string(),
        true,
        theme,
    );

    draw_label(hdc, "Port", 214, 214, theme);
    draw_stepper(
        hdc,
        port_minus_rect(),
        port_value_rect(),
        port_plus_rect(),
        &editor.params.port.to_string(),
        true,
        theme,
    );

    draw_label(hdc, endpoint_label_text(editor.params), 24, 260, theme);
    draw_text(
        hdc,
        endpoint_hint_text(editor.params),
        150,
        260,
        320,
        18,
        theme.secondary_text,
        false,
        DT_LEFT,
    );

    for index in 0..4 {
        draw_stepper(
            hdc,
            ip_minus_rect(index),
            ip_value_rect(index),
            ip_plus_rect(index),
            &editor.params.ip[index].to_string(),
            true,
            theme,
        );
    }

    draw_button(hdc, apply_rect(), "Apply", false, theme);

    let runtime = runtime_panel_rect();
    draw_panel(hdc, runtime, theme);
    draw_text(hdc, "Runtime", 24, 350, 120, 18, theme.text, false, DT_LEFT);
    for (index, line) in editor.status.iter().enumerate() {
        draw_text(
            hdc,
            line,
            runtime.x + 16,
            runtime.y + 20 + (index as i32 * 20),
            runtime.w - 32,
            18,
            theme.text,
            false,
            DT_LEFT,
        );
    }

    SelectObject(hdc, old_font);
}

unsafe fn draw_label(hdc: HDC, text: &str, x: i32, y: i32, theme: crate::generic_gui::Theme) {
    draw_text(hdc, text, x, y, 160, 18, theme.text, false, DT_LEFT);
}

unsafe fn draw_button(
    hdc: HDC,
    rect: crate::generic_gui::Rect,
    text: &str,
    selected: bool,
    theme: crate::generic_gui::Theme,
) {
    let fill = if selected {
        theme.selected_fill
    } else {
        theme.field_fill
    };
    draw_panel_fill(hdc, rect, fill, theme.rule);
    draw_text(
        hdc,
        text,
        rect.x,
        rect.y + 1,
        rect.w,
        rect.h,
        if selected { theme.accent } else { theme.text },
        true,
        DT_CENTER,
    );
}

unsafe fn draw_stepper(
    hdc: HDC,
    minus: crate::generic_gui::Rect,
    value: crate::generic_gui::Rect,
    plus: crate::generic_gui::Rect,
    text: &str,
    enabled: bool,
    theme: crate::generic_gui::Theme,
) {
    let effective = if enabled {
        theme
    } else {
        crate::generic_gui::Theme {
            secondary_text: theme.secondary_text,
            text: theme.secondary_text,
            selected_fill: theme.field_fill,
            accent: theme.secondary_text,
            ..theme
        }
    };
    draw_button(hdc, minus, "-", false, effective);
    draw_panel_fill(hdc, value, effective.field_fill, effective.rule);
    draw_text(
        hdc,
        text,
        value.x,
        value.y + 1,
        value.w,
        value.h,
        effective.text,
        true,
        DT_CENTER,
    );
    draw_button(hdc, plus, "+", false, effective);
}

unsafe fn draw_panel(hdc: HDC, rect: crate::generic_gui::Rect, theme: crate::generic_gui::Theme) {
    draw_panel_fill(hdc, rect, theme.panel_fill, theme.rule);
}

unsafe fn draw_panel_fill(
    hdc: HDC,
    rect: crate::generic_gui::Rect,
    fill: crate::generic_gui::Rgb,
    stroke: crate::generic_gui::Rgb,
) {
    let win_rect = RECT {
        left: rect.x,
        top: rect.y,
        right: rect.x + rect.w,
        bottom: rect.y + rect.h,
    };
    fill_rect(hdc, win_rect, rgb_to_colorref(fill));
    frame_rect(hdc, win_rect, rgb_to_colorref(stroke));
}

unsafe fn draw_text(
    hdc: HDC,
    text: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: crate::generic_gui::Rgb,
    vcenter: bool,
    align: u32,
) {
    let Some(mut wide) = wide_null(text) else {
        return;
    };
    let mut rect = RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    };
    SetTextColor(hdc, rgb_to_colorref(color));
    let mut flags = align | DT_SINGLELINE;
    if vcenter {
        flags |= DT_VCENTER;
    }
    DrawTextW(hdc, wide.as_mut_ptr(), -1, &mut rect, flags);
}

unsafe fn fill_rect(hdc: HDC, rect: RECT, color: COLORREF) {
    let brush = CreateSolidBrush(color);
    FillRect(hdc, &rect, brush);
    DeleteObject(brush as _);
}

unsafe fn frame_rect(hdc: HDC, rect: RECT, color: COLORREF) {
    let brush = CreateSolidBrush(color);
    FrameRect(hdc, &rect, brush);
    DeleteObject(brush as _);
}

fn fid_string_matches(value: FIDString, expected: FIDString) -> bool {
    if value.is_null() || expected.is_null() {
        return false;
    }

    unsafe {
        std::ffi::CStr::from_ptr(value as *const c_char).to_bytes()
            == std::ffi::CStr::from_ptr(expected as *const c_char).to_bytes()
    }
}

fn wide_null(text: &str) -> Option<Vec<u16>> {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    Some(wide)
}

fn rgb_to_colorref(rgb: crate::generic_gui::Rgb) -> COLORREF {
    (rgb.r as u32) | ((rgb.g as u32) << 8) | ((rgb.b as u32) << 16)
}
