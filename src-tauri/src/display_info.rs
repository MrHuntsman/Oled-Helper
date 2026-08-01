// display_info.rs — queries the current display mode (resolution + refresh
// rate) via EnumDisplaySettingsW. Browser APIs (window.screen) can report
// resolution but never expose real refresh rate, so this goes through Win32
// directly instead.

use windows::Win32::Graphics::Gdi::{
    EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS,
};
use windows::core::PCWSTR;

#[derive(serde::Serialize)]
pub struct DisplayInfo {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

pub fn query_current_display() -> Option<DisplayInfo> {
    unsafe {
        let mut devmode = DEVMODEW::default();
        devmode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        // PCWSTR::null() targets the primary/default display device, matching
        // what window.screen reports in the frontend for the common single- or
        // primary-monitor case.
        let ok = EnumDisplaySettingsW(PCWSTR::null(), ENUM_CURRENT_SETTINGS, &mut devmode);
        if !ok.as_bool() {
            return None;
        }
        Some(DisplayInfo {
            width: devmode.dmPelsWidth,
            height: devmode.dmPelsHeight,
            refresh_hz: devmode.dmDisplayFrequency,
        })
    }
}
