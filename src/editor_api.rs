use std::ffi::c_void;

use crate::network::StreamParameters;

#[derive(Clone, Copy)]
pub(crate) struct EditorControllerApi {
    pub(crate) controller: *const c_void,
    pub(crate) parameters: unsafe fn(*const c_void) -> StreamParameters,
    pub(crate) apply_ui_parameter: unsafe fn(*const c_void, u32, f64),
    pub(crate) trigger_apply_reset: unsafe fn(*const c_void),
    pub(crate) runtime_status_lines: unsafe fn(*const c_void) -> [String; 4],
}
