// gamma_ramp.rs — 256-entry 16-bit gamma ramps using a Reinhard-based shadow-recovery curve.
// Pure black (index 0) is always left untouched.

use windows::Win32::Graphics::Gdi::{GetDC, HDC, ReleaseDC};

#[repr(C)]
#[derive(Clone, Copy, PartialEq)]
pub struct GammaRamp {
    pub red:   [u16; 256],
    pub green: [u16; 256],
    pub blue:  [u16; 256],
}

impl GammaRamp {
    fn zeroed() -> Self {
        Self { red: [0u16; 256], green: [0u16; 256], blue: [0u16; 256] }
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
            let t      = i as f64 / 255.0;
            let abs_bl = black_lift.unsigned_abs() as i32;
            let blend  = (1.0 - t) / (1.0 + t * 2.0);
            let floor  = (abs_bl as f64 / 2550.0) * blend;
            let lifted = floor + t * (1.0 - floor);
            if black_lift >= 0 {
                lifted.clamp(0.0, 1.0)
            } else {
                (2.0 * t - lifted).clamp(0.0, 1.0)
            }
        };
        let v = (out * 65535.0).round() as u16;
        ramp.red[i] = v; ramp.green[i] = v; ramp.blue[i] = v;
    }
    ramp
}

pub fn build_linear_ramp() -> GammaRamp {
    let mut ramp = GammaRamp::zeroed();
    for i in 0usize..256 {
        let v = (i as f64 / 255.0 * 65535.0).round() as u16;
        ramp.red[i] = v; ramp.green[i] = v; ramp.blue[i] = v;
    }
    ramp
}

pub unsafe fn set_display_ramp(ramp: &GammaRamp) -> bool {
    let hdc = GetDC(None);
    if hdc.0.is_null() { return false; }
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
    if hdc.0.is_null() { return None; }
    let mut ramp = GammaRamp::zeroed();
    let ok = GetDeviceGammaRamp(hdc, &mut ramp);
    ReleaseDC(None, hdc);
    if ok != 0 { Some(ramp) } else { None }
}
