// gamma_ramp.rs ,  256-entry 16-bit gamma ramps using a Reinhard-based shadow-recovery curve.
// Pure black (index 0) is always left untouched.

use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::{CreateDCW, DeleteDC, GetDC, ReleaseDC};
// Use the windows crate's typed bindings (gdi32!SetDeviceGammaRamp) rather than
// a hand-rolled `#[link(name = "gdi32")]` extern block. The crate models the
// ramp payload as an opaque `*const c_void`, so we keep our own `GammaRamp`
// struct below; its layout (red/green/blue, 256 × u16 each) matches the Win32
// GAMMA_RAMP memory layout.
use windows::Win32::UI::ColorSystem::{GetDeviceGammaRamp, SetDeviceGammaRamp};

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct GammaRamp {
    pub red: [u16; 256],
    pub green: [u16; 256],
    pub blue: [u16; 256],
}

impl GammaRamp {
    fn zeroed() -> Self {
        Self {
            red: [0u16; 256],
            green: [0u16; 256],
            blue: [0u16; 256],
        }
    }
}

pub fn build_ramp(black_lift: i32) -> GammaRamp {
    let mut ramp = GammaRamp::zeroed();
    for i in 0usize..256 {
        let out: f64 = if i == 0 {
            0.0
        } else {
            let t = i as f64 / 255.0;
            let abs_bl = black_lift.unsigned_abs() as i32;
            let blend = (1.0 - t) / (1.0 + t * 2.0);
            let floor = (abs_bl as f64 / 2550.0) * blend;
            let lifted = floor + t * (1.0 - floor);
            if black_lift >= 0 {
                lifted.clamp(0.0, 1.0)
            } else {
                (2.0 * t - lifted).clamp(0.0, 1.0)
            }
        };
        let v = (out * 65535.0).round() as u16;
        ramp.red[i] = v;
        ramp.green[i] = v;
        ramp.blue[i] = v;
    }
    ramp
}

pub fn build_linear_ramp() -> GammaRamp {
    let mut ramp = GammaRamp::zeroed();
    for i in 0usize..256 {
        let v = (i as f64 / 255.0 * 65535.0).round() as u16;
        ramp.red[i] = v;
        ramp.green[i] = v;
        ramp.blue[i] = v;
    }
    ramp
}

/// Builds the existing Black Crush ramp with an additional per-channel
/// brightness multiplier. `brightness = 1.0` preserves the normal ramp;
/// `brightness = 0.0` produces black while keeping the ramp shape available
/// for a smooth restore. This is the screen protector's gamma-based dim path.
pub fn build_dimmed_ramp(black_lift: i32, brightness: f32) -> GammaRamp {
    let mut ramp = build_ramp(black_lift);
    let brightness = brightness.clamp(0.0, 1.0) as f64;
    for channel in [&mut ramp.red, &mut ramp.green, &mut ramp.blue] {
        for value in channel.iter_mut() {
            *value = (*value as f64 * brightness).round().clamp(0.0, 65535.0) as u16;
        }
    }
    ramp
}

/// Applies a ramp to the PRIMARY display only (`GetDC(None)` targets the
/// desktop DC of the primary monitor). The Black Crush profile keys in the
/// frontend are scoped per-monitor, but the gamma write always lands on the
/// primary; on multi-monitor setups a profile selected for a secondary monitor
/// therefore cannot actually be applied there. Keep this in mind when
/// extending per-monitor calibration.
pub unsafe fn set_display_ramp(ramp: &GammaRamp) -> bool {
    let hdc = GetDC(None);
    if hdc.0.is_null() {
        return false;
    }
    let ok = SetDeviceGammaRamp(hdc, ramp as *const GammaRamp as *const std::ffi::c_void);
    ReleaseDC(None, hdc);
    ok.as_bool()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Applies a ramp to a specific display device (e.g. r"\\.\DISPLAY1").
/// `GetDC(None)` always targets the primary monitor, so per-monitor
/// calibration must CreateDCW on the device name instead — this is what lets
/// Black Crush land on secondary displays.
pub unsafe fn set_monitor_ramp(device_name: &str, ramp: &GammaRamp) -> bool {
    let device = wide(device_name);
    let hdc = CreateDCW(PCWSTR::null(), PCWSTR(device.as_ptr()), PCWSTR::null(), None);
    if hdc.0.is_null() {
        return false;
    }
    let ok = SetDeviceGammaRamp(hdc, ramp as *const GammaRamp as *const std::ffi::c_void);
    let _ = DeleteDC(hdc);
    ok.as_bool()
}

/// Restores a specific display device to the linear (OS default) ramp.
pub unsafe fn reset_monitor_ramp(device_name: &str) {
    let ramp = build_linear_ramp();
    set_monitor_ramp(device_name, &ramp);
}

pub unsafe fn reset_display_ramp() {
    let ramp = build_linear_ramp();
    set_display_ramp(&ramp);
}

/// Reads the current system gamma ramp from the primary display.
#[allow(dead_code)]
pub unsafe fn get_display_ramp() -> Option<GammaRamp> {
    let hdc = GetDC(None);
    if hdc.0.is_null() {
        return None;
    }
    let mut ramp = GammaRamp::zeroed();
    let ok = GetDeviceGammaRamp(hdc, &mut ramp as *mut GammaRamp as *mut std::ffi::c_void);
    ReleaseDC(None, hdc);
    if ok.as_bool() {
        Some(ramp)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_lift_is_linear() {
        assert_eq!(build_ramp(0), build_linear_ramp());
    }

    #[test]
    fn pure_black_is_never_lifted() {
        for lift in [-150, -50, -1, 0, 1, 50, 150] {
            assert_eq!(build_ramp(lift).red[0], 0, "lift {lift}");
        }
    }

    #[test]
    fn positive_lift_brightens_shadows() {
        let lifted = build_ramp(100);
        let linear = build_linear_ramp();
        for i in 1..64 {
            assert!(lifted.red[i] > linear.red[i], "index {i}");
        }
    }

    #[test]
    fn negative_lift_darkens_shadows() {
        let crushed = build_ramp(-100);
        let linear = build_linear_ramp();
        for i in 1..64 {
            assert!(crushed.red[i] < linear.red[i], "index {i}");
        }
    }

    #[test]
    fn ramps_are_monotonic() {
        for lift in [-150, 0, 150] {
            let ramp = build_ramp(lift);
            for i in 1..256 {
                assert!(
                    ramp.red[i] >= ramp.red[i - 1],
                    "lift {lift} not monotonic at index {i}"
                );
            }
        }
    }

    #[test]
    fn white_point_is_full_brightness() {
        for lift in [-150, 150] {
            let ramp = build_ramp(lift);
            assert_eq!(ramp.red[255], 65535, "lift {lift}");
        }
    }

    #[test]
    fn dimmed_ramp_scales_existing_calibration() {
        let base = build_ramp(100);
        let dimmed = build_dimmed_ramp(100, 0.25);
        assert_eq!(build_dimmed_ramp(100, 1.0), base);
        assert!(dimmed.red[32] < base.red[32]);
        assert_eq!(dimmed.red[0], 0);
        assert!((dimmed.red[255] as i32 - 16384).abs() <= 1);
    }
}
