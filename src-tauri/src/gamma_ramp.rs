// gamma_ramp.rs — 256-entry 16-bit gamma ramps using a Reinhard-based shadow-recovery curve.
// Pure black (index 0) is always left untouched.

use windows::Win32::Graphics::Gdi::{GetDC, ReleaseDC, HDC};

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

#[link(name = "gdi32")]
extern "system" {
    pub fn SetDeviceGammaRamp(hdc: HDC, lpramp: *const GammaRamp) -> i32;
    pub fn GetDeviceGammaRamp(hdc: HDC, lpramp: *mut GammaRamp) -> i32;
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

pub unsafe fn set_display_ramp(ramp: &GammaRamp) -> bool {
    let hdc = GetDC(None);
    if hdc.0.is_null() {
        return false;
    }
    let ok = SetDeviceGammaRamp(hdc, ramp);
    ReleaseDC(None, hdc);
    ok != 0
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
    let ok = GetDeviceGammaRamp(hdc, &mut ramp);
    ReleaseDC(None, hdc);
    if ok != 0 {
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
}
