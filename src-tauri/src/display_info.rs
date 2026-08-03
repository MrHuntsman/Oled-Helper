// display_info.rs — queries and changes the current display mode via Win32.
// Browser APIs (window.screen) can report resolution but never expose the real
// refresh rate or the complete set of supported display modes.

use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsW, EnumDisplaySettingsW, DEVMODEW,
    ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE, CDS_UPDATEREGISTRY,
    DISP_CHANGE_SUCCESSFUL, DM_DISPLAYFREQUENCY, DM_PELSWIDTH, DM_PELSHEIGHT,
};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::PCWSTR;

#[derive(serde::Serialize, Clone)]
pub struct ResolutionInfo {
    pub width: u32,
    pub height: u32,
}

#[derive(serde::Serialize)]
pub struct DisplayInfo {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub refresh_rates: Vec<u32>,
    pub resolutions: Vec<ResolutionInfo>,
}

fn fresh_devmode() -> DEVMODEW {
    DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    }
}

fn mode_matches_display(mode: &DEVMODEW, current: &DEVMODEW) -> bool {
    mode.dmBitsPerPel == current.dmBitsPerPel
        && mode.dmDisplayFrequency > 0
        && mode.dmPelsWidth > 0
        && mode.dmPelsHeight > 0
}

fn for_each_display_mode(mut callback: impl FnMut(&DEVMODEW) -> bool) {
    for mode_index in 0u32.. {
        let mut mode = fresh_devmode();
        let ok = unsafe {
            EnumDisplaySettingsW(
                PCWSTR::null(),
                ENUM_DISPLAY_SETTINGS_MODE(mode_index),
                &mut mode,
            )
        };
        if !ok.as_bool() || !callback(&mode) {
            break;
        }
    }
}

#[derive(Clone)]
struct CachedModeLists {
    width: u32,
    height: u32,
    bits_per_pixel: u32,
    refresh_rates: Vec<u32>,
    resolutions: Vec<ResolutionInfo>,
    captured_at: Instant,
}

static MODE_CACHE: OnceLock<Mutex<Option<CachedModeLists>>> = OnceLock::new();

fn mode_lists_for(current: &DEVMODEW) -> (Vec<u32>, Vec<ResolutionInfo>) {
    let cache = MODE_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some(cached) = guard.as_ref().filter(|cached| {
            cached.width == current.dmPelsWidth
                && cached.height == current.dmPelsHeight
                && cached.bits_per_pixel == current.dmBitsPerPel
                && cached.captured_at.elapsed() < Duration::from_secs(2)
        }) {
            return (cached.refresh_rates.clone(), cached.resolutions.clone());
        }
    }

    let mut rates = Vec::new();
    for_each_display_mode(|mode| {
        // Keep the active resolution and colour depth fixed: changing only
        // the frequency avoids accidentally switching pixel format, which is
        // especially important while HDR is active.
        if mode_matches_display(mode, current)
            && mode.dmPelsWidth == current.dmPelsWidth
            && mode.dmPelsHeight == current.dmPelsHeight
        {
            rates.push(mode.dmDisplayFrequency);
        }
        true
    });
    if current.dmDisplayFrequency > 0 {
        rates.push(current.dmDisplayFrequency);
    }
    rates.sort_unstable();
    rates.dedup();

    let mut resolutions = Vec::new();
    for_each_display_mode(|mode| {
        if mode_matches_display(mode, current)
            && !resolutions.iter().any(|resolution: &ResolutionInfo| {
                resolution.width == mode.dmPelsWidth && resolution.height == mode.dmPelsHeight
            })
        {
            resolutions.push(ResolutionInfo {
                width: mode.dmPelsWidth,
                height: mode.dmPelsHeight,
            });
        }
        true
    });

    if !resolutions.iter().any(|resolution| {
        resolution.width == current.dmPelsWidth && resolution.height == current.dmPelsHeight
    }) {
        resolutions.push(ResolutionInfo {
            width: current.dmPelsWidth,
            height: current.dmPelsHeight,
        });
    }
    resolutions.sort_unstable_by_key(|resolution| (resolution.width, resolution.height));

    if let Ok(mut guard) = cache.lock() {
        *guard = Some(CachedModeLists {
            width: current.dmPelsWidth,
            height: current.dmPelsHeight,
            bits_per_pixel: current.dmBitsPerPel,
            refresh_rates: rates.clone(),
            resolutions: resolutions.clone(),
            captured_at: Instant::now(),
        });
    }
    (rates, resolutions)
}

pub fn query_current_display() -> Option<DisplayInfo> {
    unsafe {
        // PCWSTR::null() targets the primary/default display, matching the
        // existing display-info behaviour in the frontend.
        let mut devmode = fresh_devmode();
        let ok = EnumDisplaySettingsW(PCWSTR::null(), ENUM_CURRENT_SETTINGS, &mut devmode);
        if !ok.as_bool() {
            return None;
        }
        let (refresh_rates, resolutions) = mode_lists_for(&devmode);
        Some(DisplayInfo {
            width: devmode.dmPelsWidth,
            height: devmode.dmPelsHeight,
            refresh_hz: devmode.dmDisplayFrequency,
            refresh_rates,
            resolutions,
        })
    }
}

pub fn set_refresh_rate(hz: u32) -> Result<(), String> {
    unsafe {
        let mut devmode = fresh_devmode();
        let ok = EnumDisplaySettingsW(PCWSTR::null(), ENUM_CURRENT_SETTINGS, &mut devmode);
        if !ok.as_bool() {
            return Err("unable to read the current display mode".into());
        }
        if !mode_lists_for(&devmode).0.contains(&hz) {
            return Err(format!("{hz} Hz is not available at the current resolution"));
        }

        devmode.dmDisplayFrequency = hz;
        devmode.dmFields |= DM_DISPLAYFREQUENCY;
        apply_display_mode(&devmode)
    }
}

pub fn set_resolution(width: u32, height: u32, preferred_hz: u32) -> Result<(), String> {
    unsafe {
        let mut current = fresh_devmode();
        let ok = EnumDisplaySettingsW(PCWSTR::null(), ENUM_CURRENT_SETTINGS, &mut current);
        if !ok.as_bool() {
            return Err("unable to read the current display mode".into());
        }
        if !mode_lists_for(&current).1.iter().any(|resolution| {
            resolution.width == width && resolution.height == height
        }) {
            return Err(format!("{width}×{height} is not available on this display"));
        }

        let mut selected = None;
        let mut fallback = None;
        for_each_display_mode(|mode| {
            if mode_matches_display(mode, &current)
                && mode.dmPelsWidth == width
                && mode.dmPelsHeight == height
            {
                if mode.dmDisplayFrequency == preferred_hz {
                    selected = Some(*mode);
                    return false;
                }
                if fallback.is_none() {
                    fallback = Some(*mode);
                }
            }
            true
        });

        let mut target = selected.or(fallback).ok_or_else(|| {
            format!("no usable display mode found for {width}×{height}")
        })?;
        // The enumerated DEVMODE is a complete mode, but explicitly mark the
        // fields being changed so ChangeDisplaySettingsW applies both the
        // resolution and its selected refresh rate.
        target.dmFields |= DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
        apply_display_mode(&target)
    }
}

unsafe fn apply_display_mode(devmode: &DEVMODEW) -> Result<(), String> {
    let result = ChangeDisplaySettingsW(
        Some(devmode as *const DEVMODEW),
        CDS_UPDATEREGISTRY,
    );
    if result == DISP_CHANGE_SUCCESSFUL {
        Ok(())
    } else {
        Err(format!("Windows rejected the display-mode change (code {})", result.0))
    }
}
