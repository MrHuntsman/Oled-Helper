// gamepad.rs — gamepad/joystick input detection for the Screen Dimmer.
//
// Windows' GetLastInputInfo only tracks keyboard and mouse, so controller
// input does not reset the system idle clock. A game played exclusively with
// a gamepad would otherwise let the Screen Dimmer fade the display mid-game.
// This module polls the two controller APIs that Windows games actually use —
// XInput (Xbox controllers) and legacy DirectInput (most other joysticks) —
// and reports whether any connected device is currently producing non-neutral
// input (a pressed button, a moved trigger, or a stick/hat outside its
// deadzone).
//
// Both DLLs are loaded dynamically (xinput1_4.dll → xinput9_1_0.dll →
// xinput1_3.dll, then dinput8.dll) so a missing controller runtime can never
// break startup, matching the optional-vendor-DLL pattern in vendor_info.rs.
// Once loaded the handles are intentionally retained for the process lifetime;
// both are system DLLs.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use windows::core::{GUID, PCSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};

fn utf16z(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn export<T>(module: HMODULE, name: &[u8]) -> Option<T> {
    let pointer = GetProcAddress(module, PCSTR(name.as_ptr()))?;
    Some(std::mem::transmute_copy(&pointer))
}

unsafe fn data_export(module: HMODULE, name: &[u8]) -> Option<*const c_void> {
    let pointer = GetProcAddress(module, PCSTR(name.as_ptr()))?;
    Some(std::mem::transmute_copy(&pointer))
}

// ── XInput (Xbox controllers) ─────────────────────────────────────

const XINPUT_ERROR_DEVICE_NOT_CONNECTED: u32 = 1167;
const XINPUT_GAMEPAD_LEFT_THUMB_DEADZONE: i32 = 7849;
const XINPUT_GAMEPAD_RIGHT_THUMB_DEADZONE: i32 = 8689;
const XINPUT_GAMEPAD_TRIGGER_THRESHOLD: u8 = 30;
const XUSER_MAX_COUNT: u32 = 4;

#[repr(C)]
struct XInputGamepad {
    w_buttons: u16,
    b_left_trigger: u8,
    b_right_trigger: u8,
    s_thumb_lx: i16,
    s_thumb_ly: i16,
    s_thumb_rx: i16,
    s_thumb_ry: i16,
}

#[repr(C)]
struct XInputState {
    dw_packet_number: u32,
    gamepad: XInputGamepad,
}

type XInputGetStateFn = unsafe extern "system" fn(u32, *mut XInputState) -> u32;

static XINPUT_GET_STATE: OnceLock<Option<XInputGetStateFn>> = OnceLock::new();

fn xinput_get_state() -> Option<XInputGetStateFn> {
    *XINPUT_GET_STATE.get_or_init(|| unsafe {
        for dll in ["xinput1_4.dll", "xinput9_1_0.dll", "xinput1_3.dll"] {
            let wide = utf16z(dll);
            if let Ok(module) = LoadLibraryW(windows::core::PCWSTR(wide.as_ptr())) {
                if let Some(function) = export::<XInputGetStateFn>(module, b"XInputGetState\0") {
                    return Some(function);
                }
            }
        }
        None
    })
}

fn xinput_gamepad_active(gamepad: &XInputGamepad) -> bool {
    gamepad.w_buttons != 0
        || gamepad.b_left_trigger >= XINPUT_GAMEPAD_TRIGGER_THRESHOLD
        || gamepad.b_right_trigger >= XINPUT_GAMEPAD_TRIGGER_THRESHOLD
        || (gamepad.s_thumb_lx as i32).abs() > XINPUT_GAMEPAD_LEFT_THUMB_DEADZONE
        || (gamepad.s_thumb_ly as i32).abs() > XINPUT_GAMEPAD_LEFT_THUMB_DEADZONE
        || (gamepad.s_thumb_rx as i32).abs() > XINPUT_GAMEPAD_RIGHT_THUMB_DEADZONE
        || (gamepad.s_thumb_ry as i32).abs() > XINPUT_GAMEPAD_RIGHT_THUMB_DEADZONE
}

fn poll_xinput() -> bool {
    let Some(get_state) = xinput_get_state() else {
        return false;
    };
    for index in 0..XUSER_MAX_COUNT {
        let mut state = XInputState {
            dw_packet_number: 0,
            gamepad: XInputGamepad {
                w_buttons: 0,
                b_left_trigger: 0,
                b_right_trigger: 0,
                s_thumb_lx: 0,
                s_thumb_ly: 0,
                s_thumb_rx: 0,
                s_thumb_ry: 0,
            },
        };
        let result = unsafe { get_state(index, &mut state) };
        if result == XINPUT_ERROR_DEVICE_NOT_CONNECTED {
            continue;
        }
        if result == 0 && xinput_gamepad_active(&state.gamepad) {
            return true;
        }
    }
    false
}

// ── DirectInput (legacy joysticks/gamepads) ────────────────────────

const DI8DEVCLASS_GAMECTRL: u32 = 4;
const DIEDFL_ALLDEVICES: u32 = 0;
const DISCL_BACKGROUND: u32 = 0x0000_0008;
const DISCL_NONEXCLUSIVE: u32 = 0x0000_0010;
const DIENUM_CONTINUE: i32 = 1;
const DIRECTINPUT_VERSION: u32 = 0x0800;
// Axis/pov changes below this magnitude are treated as analog jitter, not
// real input. The resting value itself is device-specific (signed vs unsigned
// ranges), so activity is measured as a delta from the previous poll instead
// of an absolute position.
const AXIS_DELTA: i32 = 100;

const IID_IDIRECTINPUT8W: GUID = GUID::from_values(
    0xBF79_8031,
    0x483A,
    0x4DA2,
    [0xAA, 0x99, 0x5D, 0x64, 0xED, 0x36, 0x97, 0x00],
);

// DIJOYSTATE2 — the immediate-state buffer DirectInput game controllers fill.
#[repr(C)]
#[derive(Clone, Copy)]
struct DiJoyState2 {
    l_x: i32,
    l_y: i32,
    l_z: i32,
    l_rx: i32,
    l_ry: i32,
    l_rz: i32,
    rgl_slider: [i32; 2],
    rgdw_pov: [u32; 4],
    rgb_buttons: [u8; 128],
    l_vx: i32,
    l_vy: i32,
    l_vz: i32,
    l_vrx: i32,
    l_vry: i32,
    l_vrz: i32,
    rgl_vslider: [i32; 2],
    l_ax: i32,
    l_ay: i32,
    l_az: i32,
    l_arx: i32,
    l_ary: i32,
    l_arz: i32,
    rgl_aslider: [i32; 2],
    l_fx: i32,
    l_fy: i32,
    l_fz: i32,
    l_frx: i32,
    l_fry: i32,
    l_frz: i32,
    rgl_fslider: [i32; 2],
}

// DIDEVICEINSTANCEW — only the leading fields are read during enumeration.
#[repr(C)]
struct DiDeviceInstanceW {
    dw_size: u32,
    guid_instance: GUID,
}

type DiCreateDeviceFn = unsafe extern "system" fn(
    *mut c_void,
    *const GUID,
    *mut *mut c_void,
    *mut c_void,
) -> i32;

type DiEnumCallback =
    unsafe extern "system" fn(*const DiDeviceInstanceW, *mut c_void) -> i32;

type DiEnumDevicesFn = unsafe extern "system" fn(
    *mut c_void,
    u32,
    Option<DiEnumCallback>,
    *mut c_void,
    u32,
) -> i32;

type DiReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;
type DiAcquireFn = unsafe extern "system" fn(*mut c_void) -> i32;
type DiUnacquireFn = unsafe extern "system" fn(*mut c_void) -> i32;
type DiGetDeviceStateFn = unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> i32;
type DiSetDataFormatFn = unsafe extern "system" fn(*mut c_void, *const c_void) -> i32;
type DiSetCooperativeLevelFn = unsafe extern "system" fn(*mut c_void, *mut c_void, u32) -> i32;

type DirectInput8CreateFn = unsafe extern "system" fn(
    *mut c_void,
    u32,
    *const GUID,
    *mut *mut c_void,
    *mut c_void,
) -> i32;

// The two COM interface vtables only spell out the methods we call; the usize
// slots are the same pointer size as a method and keep every later index in
// the correct position.
#[repr(C)]
struct IDirectInput8WVtbl {
    query_interface: usize,
    add_ref: usize,
    release: DiReleaseFn,
    create_device: DiCreateDeviceFn,
    enum_devices: DiEnumDevicesFn,
    get_device_status: usize,
    run_control_panel: usize,
    initialize: usize,
    find_device: usize,
    enum_devices_by_semantics: usize,
    configure_devices: usize,
}

#[repr(C)]
struct IDirectInputDevice8WVtbl {
    query_interface: usize,
    add_ref: usize,
    release: DiReleaseFn,
    get_capabilities: usize,
    enum_objects: usize,
    get_property: usize,
    set_property: usize,
    acquire: DiAcquireFn,
    unacquire: DiUnacquireFn,
    get_device_state: DiGetDeviceStateFn,
    get_device_data: usize,
    set_data_format: DiSetDataFormatFn,
    set_event_notification: usize,
    set_cooperative_level: DiSetCooperativeLevelFn,
}

struct DiDevice {
    device: *mut c_void,          // IDirectInputDevice8W*
    prev: Option<DiJoyState2>, // last polled state, for delta detection
}

struct DiState {
    dinput8: *mut c_void,       // IDirectInput8W*
    data_format: *const c_void, // c_dfDIJoystick2
    devices: Vec<DiDevice>,
    last_enumerate: Option<Instant>,
}

// The raw COM pointers are only ever dereferenced while the surrounding Mutex
// is held, so the state can safely be shared between threads even though the
// pointers themselves do not carry Send/Sync.
unsafe impl Send for DiState {}

static DI_STATE: OnceLock<Mutex<DiState>> = OnceLock::new();

fn direct_input_state() -> &'static Mutex<DiState> {
    DI_STATE.get_or_init(|| {
        Mutex::new(DiState {
            dinput8: std::ptr::null_mut(),
            data_format: std::ptr::null(),
            devices: Vec::new(),
            last_enumerate: None,
        })
    })
}

unsafe fn device_vtbl(device: *mut c_void) -> *mut IDirectInputDevice8WVtbl {
    *(device as *mut *mut IDirectInputDevice8WVtbl)
}

unsafe extern "system" fn enum_game_controllers(
    instance: *const DiDeviceInstanceW,
    context: *mut c_void,
) -> i32 {
    if instance.is_null() || context.is_null() {
        return DIENUM_CONTINUE;
    }
    let guids = &mut *(context as *mut Vec<GUID>);
    guids.push((*instance).guid_instance);
    DIENUM_CONTINUE
}

unsafe fn init_direct_input(state: &mut DiState) -> bool {
    let module = match LoadLibraryW(windows::core::w!("dinput8.dll")) {
        Ok(module) => module,
        Err(_) => return false,
    };
    let Some(create) = export::<DirectInput8CreateFn>(module, b"DirectInput8Create\0") else {
        return false;
    };
    let Some(data_format) = data_export(module, b"c_dfDIJoystick2\0") else {
        return false;
    };
    let hinst = GetModuleHandleW(None).unwrap_or_default();

    let mut dinput8: *mut c_void = std::ptr::null_mut();
    let hr = create(
        hinst.0 as *mut c_void,
        DIRECTINPUT_VERSION,
        &IID_IDIRECTINPUT8W,
        &mut dinput8,
        std::ptr::null_mut(),
    );
    if hr < 0 || dinput8.is_null() {
        return false;
    }
    state.dinput8 = dinput8;
    state.data_format = data_format;
    state.last_enumerate = None;
    true
}

unsafe fn re_enumerate_devices(state: &mut DiState) {
    for entry in &state.devices {
        let vtbl = device_vtbl(entry.device);
        let _ = ((*vtbl).release)(entry.device);
    }
    state.devices.clear();

    let dinput8 = state.dinput8;
    let vtbl = *(dinput8 as *mut *mut IDirectInput8WVtbl);
    let mut guids: Vec<GUID> = Vec::new();
    let _ = ((*vtbl).enum_devices)(
        dinput8,
        DI8DEVCLASS_GAMECTRL,
        Some(enum_game_controllers),
        &mut guids as *mut Vec<GUID> as *mut c_void,
        DIEDFL_ALLDEVICES,
    );

    for guid in &guids {
        let mut device: *mut c_void = std::ptr::null_mut();
        let hr = ((*vtbl).create_device)(dinput8, guid, &mut device, std::ptr::null_mut());
        if hr < 0 || device.is_null() {
            continue;
        }
        let dvtbl = device_vtbl(device);
        let _ = ((*dvtbl).set_data_format)(device, state.data_format);
        // DISCL_BACKGROUND | DISCL_NONEXCLUSIVE lets us read while another
        // window has focus; a NULL hwnd is valid for non-exclusive access.
        let _ = ((*dvtbl).set_cooperative_level)(
            device,
            std::ptr::null_mut(),
            DISCL_BACKGROUND | DISCL_NONEXCLUSIVE,
        );
        state.devices.push(DiDevice { device, prev: None });
    }
    state.last_enumerate = Some(Instant::now());
}

fn axis_changed(prev: i32, current: i32) -> bool {
    (prev - current).abs() > AXIS_DELTA
}

fn di_joy_active(prev: &DiJoyState2, current: &DiJoyState2) -> bool {
    // Buttons are the one unambiguous signal: released is exactly zero, so a
    // held button keeps the dimmer from activating during play.
    if current.rgb_buttons.iter().any(|b| *b != 0) {
        return true;
    }
    // Axes and the POV hat have device-specific resting values (signed vs
    // unsigned ranges, missing-hat quirks). A connected-but-idle controller
    // must therefore not read as active: only a change from the previous poll
    // counts as input.
    axis_changed(prev.l_x, current.l_x)
        || axis_changed(prev.l_y, current.l_y)
        || axis_changed(prev.l_z, current.l_z)
        || axis_changed(prev.l_rx, current.l_rx)
        || axis_changed(prev.l_ry, current.l_ry)
        || axis_changed(prev.l_rz, current.l_rz)
        || prev
            .rgl_slider
            .iter()
            .zip(current.rgl_slider.iter())
            .any(|(p, c)| axis_changed(*p, *c))
        || prev.rgdw_pov != current.rgdw_pov
}

fn poll_direct_input() -> bool {
    let Ok(mut state) = direct_input_state().lock() else {
        return false;
    };
    unsafe {
        if state.dinput8.is_null() && !init_direct_input(&mut state) {
            return false;
        }
        // Re-enumerate occasionally to pick up hot-plugged controllers. This
        // releases and re-acquires every device, so keep it infrequent to
        // avoid re-acquire artifacts feeding the delta detector.
        if state
            .last_enumerate
            .map_or(true, |t| t.elapsed().as_secs() >= 30)
        {
            re_enumerate_devices(&mut state);
        }
        for entry in &mut state.devices {
            let vtbl = device_vtbl(entry.device);
            if ((*vtbl).acquire)(entry.device) < 0 {
                continue;
            }
            let mut joy = std::mem::zeroed::<DiJoyState2>();
            let got = ((*vtbl).get_device_state)(
                entry.device,
                std::mem::size_of::<DiJoyState2>() as u32,
                &mut joy as *mut DiJoyState2 as *mut c_void,
            );
            let _ = ((*vtbl).unacquire)(entry.device);
            if got < 0 {
                continue;
            }
            // The first read only establishes a baseline so a controller that
            // reports a non-zero resting position is not mistaken for input.
            let active = entry
                .prev
                .map_or(false, |previous| di_joy_active(&previous, &joy));
            entry.prev = Some(joy);
            if active {
                return true;
            }
        }
    }
    false
}

/// Returns true if any connected controller is currently producing input.
/// Cheap enough to call on a ~100 ms cadence while the Screen Dimmer is
/// enabled; loads the runtime DLLs lazily on first use. Exposed as a command
/// so the Debug tab can live-test controller detection.
#[tauri::command]
pub fn gamepad_input_active() -> bool {
    poll_xinput() || poll_direct_input()
}
