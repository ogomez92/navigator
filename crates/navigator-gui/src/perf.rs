//! Disable per-window animations / fades / smooth-scroll. Single-user
//! screen-reader-only build — visual transitions cost paint time and
//! delay screen-reader focus events without buying anything.
//!
//! Two switches per window:
//! - DWM `DWMWA_TRANSITIONS_FORCEDISABLED` kills compositor fade on
//!   show/hide/minimize and the open/close blur.
//! - UxTheme `SetWindowTheme(hwnd, " ", " ")` strips the theme service
//!   from this window: no hover fade, no focus rect animation, no
//!   smooth list-box scroll. The control still draws — just classically.
//!
//! Apply both to every top-level window we own (main hwnd, dialogs,
//! property sheets) and to the listview itself.

#![cfg(windows)]

use std::ffi::c_void;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{DWMWA_TRANSITIONS_FORCEDISABLED, DwmSetWindowAttribute};
use windows::Win32::UI::Controls::SetWindowTheme;
use windows::core::{BOOL, w};

pub fn disable_animations(hwnd: HWND) {
    if hwnd.is_invalid() {
        return;
    }
    unsafe {
        let yes = BOOL::from(true);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED,
            &yes as *const _ as *const c_void,
            std::mem::size_of::<BOOL>() as u32,
        );
        // Empty strings are documented to disable visual styles for the
        // window; we use single-space strings because that's the older
        // recipe Microsoft samples use and it round-trips through the
        // PCWSTR parameter the same way.
        let _ = SetWindowTheme(hwnd, w!(" "), w!(" "));
    }
}
