// vendor_info.rs — queries GPU-driver-specific display properties that have no
// generic Win32 API. NVIDIA uses NVAPI, AMD uses the dynamically loaded ADLX
// runtime, and Intel uses the dynamically loaded IGCL/ControlLib runtime. The
// vendor DLLs are optional: absence of a driver SDK is always a blank result,
// never a startup failure.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex, OnceLock,
};
use std::time::{Duration, Instant};

use windows::core::PCSTR;
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, DXGI_ADAPTER_DESC, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
};
use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

fn log_vendor(message: &str) {
    if crate::detailed_logging_enabled() {
        log::info!("[vendor-info] {message}");
    }
}

fn log_vendor_lifecycle(message: &str) {
    // Teardown happens only during an explicit process shutdown or a cache
    // replacement, so these lifecycle records are intentionally unconditional
    // rather than hidden behind Detailed logging. They confirm whether the
    // process actually released the NVAPI resources before app.exit(0).
    log::info!("[vendor-info] {message}");
}

fn log_vendor_warn(message: &str) {
    // Driver capability probes can report the same unsupported function on
    // every five-second refresh. Keep the first occurrence visible, then
    // suppress identical repeats for one minute unless Detailed logging is
    // enabled. This still surfaces a recurring problem if it changes or lasts
    // long enough, without flooding the runtime log.
    if crate::detailed_logging_enabled() {
        log::warn!("[vendor-info] {message}");
        return;
    }
    static REPORTED_WARNINGS: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    let reported = REPORTED_WARNINGS.get_or_init(|| Mutex::new(HashMap::new()));
    let now = Instant::now();
    let should_report = reported
        .lock()
        .map(|mut seen| {
            let should_report = seen
                .get(message)
                .map(|timestamp| timestamp.elapsed() >= Duration::from_secs(60))
                .unwrap_or(true);
            // Only move the throttle window when the warning was emitted.
            // A continuously repeating warning must become visible again
            // after one minute rather than extending its own suppression.
            if should_report {
                seen.insert(message.to_string(), now);
            }
            should_report
        })
        .unwrap_or(true);
    if should_report {
        log::warn!("[vendor-info] {message}");
    }
}

#[derive(Clone, Default, serde::Serialize)]
pub struct VendorDisplayInfo {
    // The primary adapter's full model name from DXGI (e.g. "NVIDIA GeForce
    // RTX 3080") — vendor-agnostic, so it works for NVIDIA, AMD, Intel and
    // any other vendor without a driver SDK. The WebGPU adapter info that
    // feeds the System Info GPU Adapter row leaves its device field blank on
    // Windows WebView2, so this is the authoritative model name.
    pub gpu_model: String,
    // Canonical vendor id string: "nvidia" / "amd" / "intel" / "" — lets
    // the frontend gate NVIDIA-only features (e.g. the RTX HDR dropdown)
    // without string-matching the model name.
    pub vendor: String,
    pub dynamic_range: String,
    pub vrr: String,
    // NVIDIA's Control Panel "Override to reference mode" (also called
    // Color Accuracy Mode) prevents SetDeviceGammaRamp from taking effect.
    // This is a driver-control setting, not a display capability, so it is
    // detected separately and exposed for the Black Level warning.
    pub reference_mode: bool,
}

const VENDOR_NVIDIA: u32 = 0x10DE;
const VENDOR_AMD: u32 = 0x1002;
const VENDOR_INTEL: u32 = 0x8086;

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

// NVIDIA does not expose a stable public NVAPI query for the Control Panel's
// "Override to reference mode" / Color Accuracy Mode switch. The old native
// implementation detected the driver's persisted state in this registry tree;
// keep that proven fallback and make it independent of the NVAPI display
// capability queries. Missing keys mean the override is not enabled.
struct CachedReferenceMode {
    enabled: bool,
    captured_at: Instant,
}

static REFERENCE_MODE_CACHE: OnceLock<Mutex<Option<CachedReferenceMode>>> = OnceLock::new();
const REFERENCE_MODE_CACHE_TTL: Duration = Duration::from_secs(5);

pub fn nvidia_reference_mode_enabled() -> bool {
    // Gamma can be requested repeatedly while a slider is dragged. Reuse the
    // result briefly so those writes do not repeatedly enumerate DXGI and the
    // NVIDIA registry tree. Five seconds keeps this guard off the gamma hot
    // path and the 5 s vendor refresh while still noticing a driver setting
    // change within a couple of refreshes. The vendor/GPU lookup behind the
    // scan is itself cached (ADAPTER_INFO_CACHE), so the remaining cost is a
    // handful of registry reads per TTL expiry.
    let cache = REFERENCE_MODE_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some(cached) = guard.as_ref() {
            if cached.captured_at.elapsed() < REFERENCE_MODE_CACHE_TTL {
                return cached.enabled;
            }
        }
    }

    // Do not let a stale NVIDIA registry tree affect AMD/Intel systems. The
    // check is intentionally limited to gamma/vendor-info paths, not display
    // mode enumeration.
    let enabled = if primary_gpu_vendor() != Some(VENDOR_NVIDIA) {
        false
    } else {
        unsafe {
            let root_path = wide("SOFTWARE\\\\NVIDIA Corporation\\\\Global\\\\NVTweak\\\\Devices");
            let mut root = HKEY::default();
            if RegOpenKeyExW(
                HKEY_CURRENT_USER,
                windows::core::PCWSTR(root_path.as_ptr()),
                Some(0),
                KEY_READ,
                &mut root,
            ) != windows::Win32::Foundation::ERROR_SUCCESS
            {
                false
            } else {
                let color_suffix = wide("\\\\Color");
                let value_name = wide("3538970");
                let mut index = 0u32;
                let enabled = loop {
                    let mut name = [0u16; 256];
                    let mut name_len = name.len() as u32;
                    let status = RegEnumKeyExW(
                        root,
                        index,
                        Some(windows::core::PWSTR(name.as_mut_ptr())),
                        &mut name_len,
                        None,
                        Some(windows::core::PWSTR::null()),
                        None,
                        None,
                    );
                    if status != windows::Win32::Foundation::ERROR_SUCCESS {
                        break false;
                    }

                    let device_len = name_len as usize;
                    let suffix_len = color_suffix.len() - 1;
                    let mut color_path = vec![0u16; device_len + suffix_len + 1];
                    color_path[..device_len].copy_from_slice(&name[..device_len]);
                    color_path[device_len..device_len + suffix_len]
                        .copy_from_slice(&color_suffix[..suffix_len]);

                    let mut color = HKEY::default();
                    let opened = RegOpenKeyExW(
                        root,
                        windows::core::PCWSTR(color_path.as_ptr()),
                        Some(0),
                        KEY_READ,
                        &mut color,
                    );
                    if opened == windows::Win32::Foundation::ERROR_SUCCESS {
                        let mut data = 0u32;
                        let mut size = std::mem::size_of::<u32>() as u32;
                        let queried = RegQueryValueExW(
                            color,
                            windows::core::PCWSTR(value_name.as_ptr()),
                            None,
                            None,
                            Some(&mut data as *mut u32 as *mut u8),
                            Some(&mut size),
                        );
                        let is_enabled = queried == windows::Win32::Foundation::ERROR_SUCCESS
                            && size >= std::mem::size_of::<u32>() as u32
                            && data == 2;
                        let _ = RegCloseKey(color);
                        if is_enabled {
                            log_vendor("NVIDIA reference mode detected in registry");
                            break true;
                        }
                    }
                    index += 1;
                };
                let _ = RegCloseKey(root);
                enabled
            }
        }
    };

    if let Ok(mut guard) = cache.lock() {
        *guard = Some(CachedReferenceMode {
            enabled,
            captured_at: Instant::now(),
        });
    }
    enabled
}

// DXGI_ADAPTER_DESC.Description is a fixed 128-wide-char buffer.
fn adapter_description(desc: &DXGI_ADAPTER_DESC) -> String {
    let end = desc
        .Description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(desc.Description.len());
    String::from_utf16_lossy(&desc.Description[..end])
        .trim()
        .to_string()
}

// Finds the DXGI adapter driving the primary display and returns its vendor
// ID plus full model name. DXGI adapter order is not the same thing as the
// adapter driving the primary monitor on hybrid laptops/docks — match the
// primary HMONITOR first, then retain adapter 0 as a last-resort fallback.
// The model name is vendor-agnostic (NVIDIA/AMD/Intel alike), which is why
// the System Info GPU Adapter row prefers it over the WebGPU device field
// (left blank by WebView2 on Windows).
//
// The result is cached: the app-wide vendor refresh calls this every 5 s, and
// the reference-mode check calls it again — two full DXGI enumerations per
// tick was real work. The GPU itself essentially never changes while the app
// runs (hotplug/dock events go through the display-changed path, which
// invalidates this cache).
struct CachedAdapterInfo {
    vendor_id: u32,
    model: String,
    captured_at: Instant,
}

static ADAPTER_INFO_CACHE: OnceLock<Mutex<Option<CachedAdapterInfo>>> = OnceLock::new();
const ADAPTER_INFO_CACHE_TTL: Duration = Duration::from_secs(30);

fn primary_adapter_info() -> Option<(u32, String)> {
    let cache = ADAPTER_INFO_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some(cached) = guard.as_ref() {
            if cached.captured_at.elapsed() < ADAPTER_INFO_CACHE_TTL {
                return Some((cached.vendor_id, cached.model.clone()));
            }
        }
    }
    let queried = query_primary_adapter_info();
    if let Ok(mut guard) = cache.lock() {
        if let Some((vendor_id, model)) = &queried {
            *guard = Some(CachedAdapterInfo {
                vendor_id: *vendor_id,
                model: model.clone(),
                captured_at: Instant::now(),
            });
        }
    }
    queried
}

// Called by primary_adapter_info (above) — the actual DXGI enumeration.
pub(crate) fn invalidate_adapter_info_cache() {
    if let Some(cache) = ADAPTER_INFO_CACHE.get() {
        if let Ok(mut guard) = cache.lock() {
            *guard = None;
        }
    }
}

// A display-topology change invalidates everything derived from the old
// topology: the cached primary-adapter identity, the NVAPI display ID, and
// (because its destroy fn-ptr comes from the context) the DRS session. The
// DRS cache is cleared first so the session is destroyed through the still-
// loaded old context before the context itself is dropped.
pub(crate) fn invalidate_display_caches() {
    // Serialize against IPC command threads that may be mid-query holding a
    // &'static reference into the cached context. Called from the window-
    // message path, so it must not deadlock: command threads hold this lock
    // only for the duration of a vendor query (milliseconds) and never block
    // on the message thread in return, so the brief wait here is safe.
    let _query_guard = VENDOR_QUERY_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    invalidate_adapter_info_cache();
    if let Some(cache) = RTX_HDR_DRS_CACHE.get() {
        if let Ok(mut guard) = cache.lock() {
            *guard = None;
        }
    }
    if let Some(cache) = RTX_HDR_NVAPI_CACHE.get() {
        if let Ok(mut guard) = cache.lock() {
            *guard = None;
        }
    }
}

// Release the optional vendor-driver resources while the app is hidden in the
// tray. This deliberately does not set NVAPI_SHUTDOWN_STARTED: the next
// vendor query after the window is shown can initialize NVAPI and DRS lazily
// again. Keep this narrower than display invalidation: hiding the window does
// not change the active adapter, so the cached adapter identity can remain.
// The query lock and DRS-before-context clearing order keep this safe if a poll
// is finishing at hide time.
pub(crate) fn release_for_tray() {
    let _query_guard = VENDOR_QUERY_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    log_vendor_lifecycle("vendor driver session release for tray starting");

    if let Some(cache) = RTX_HDR_DRS_CACHE.get() {
        if let Ok(mut guard) = cache.lock() {
            *guard = None;
        }
    }
    if let Some(cache) = RTX_HDR_NVAPI_CACHE.get() {
        if let Ok(mut guard) = cache.lock() {
            *guard = None;
        }
    }

    log_vendor_lifecycle("vendor driver session release for tray complete");
}

// Explicitly tears down process-lifetime NVAPI resources before Tauri exits.
// AppHandle::exit ultimately terminates the process without running Rust Drop,
// so relying on NvapiDrsSession/NvapiContext Drop alone would skip both the
// DRS destroy and NvAPI_Unload logs. The DRS cache is taken and dropped first,
// while the NVAPI context/DLL is still alive; then the context is taken and
// dropped. The atomic makes this safe when both an exit command and a final
// window-destroy event observe the same shutdown.
pub(crate) fn shutdown() {
    if NVAPI_SHUTDOWN_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let _query_guard = VENDOR_QUERY_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    log_vendor_lifecycle("NVAPI shutdown starting");

    let drs = RTX_HDR_DRS_CACHE.get().and_then(|cache| {
        cache
            .lock()
            .map(|mut guard| guard.take())
            .unwrap_or_else(|error| {
                let mut guard = error.into_inner();
                guard.take()
            })
    });
    drop(drs);

    let context = RTX_HDR_NVAPI_CACHE.get().and_then(|cache| {
        cache
            .lock()
            .map(|mut guard| guard.take())
            .unwrap_or_else(|error| {
                let mut guard = error.into_inner();
                guard.take()
            })
    });
    drop(context);
    log_vendor_lifecycle("NVAPI shutdown complete");
}

fn query_primary_adapter_info() -> Option<(u32, String)> {
    unsafe {
        let Ok(factory): windows::core::Result<IDXGIFactory1> = CreateDXGIFactory1() else {
            log_vendor_warn("CreateDXGIFactory1 failed");
            return None;
        };
        let primary_monitor = MonitorFromWindow(GetDesktopWindow(), MONITOR_DEFAULTTOPRIMARY);
        for adapter_index in 0u32.. {
            let Ok(adapter) = factory.EnumAdapters1(adapter_index) else {
                break;
            };
            let Ok(desc) = adapter.GetDesc() else {
                continue;
            };
            let model = adapter_description(&desc);
            for output_index in 0u32.. {
                let Ok(output) = adapter.EnumOutputs(output_index) else {
                    break;
                };
                if let Ok(output_desc) = output.GetDesc() {
                    if output_desc.Monitor == primary_monitor {
                        log_vendor(&format!(
                            "primary display GPU vendor=0x{:04X} adapter={adapter_index} model='{model}'",
                            desc.VendorId
                        ));
                        return Some((desc.VendorId, model));
                    }
                }
            }
            if adapter_index == 0 {
                log_vendor(&format!(
                    "primary display GPU fallback vendor=0x{:04X} adapter=0 model='{model}'",
                    desc.VendorId
                ));
                return Some((desc.VendorId, model));
            }
        }
        log_vendor_warn("no DXGI adapter found for the primary display");
        None
    }
}

fn primary_gpu_vendor() -> Option<u32> {
    primary_adapter_info().map(|(vendor, _)| vendor)
}

pub(crate) fn is_nvidia_primary_gpu() -> bool {
    primary_gpu_vendor() == Some(VENDOR_NVIDIA)
}

struct CachedVendorInfo {
    info: VendorDisplayInfo,
    vendor_id: u32,
    gpu_model: String,
    captured_at: Instant,
}

static VENDOR_CACHE: OnceLock<Mutex<Option<CachedVendorInfo>>> = OnceLock::new();
static VENDOR_QUERY_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub fn query_vendor_display_info() -> VendorDisplayInfo {
    let _query_guard = VENDOR_QUERY_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    query_vendor_display_info_locked()
}

fn query_vendor_display_info_locked() -> VendorDisplayInfo {
    log_vendor("query_vendor_display_info called");
    let (vendor_id, gpu_model) = match primary_adapter_info() {
        Some(info) => info,
        None => return VendorDisplayInfo::default(),
    };

    let cache = VENDOR_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some(cached) = guard.as_ref() {
            let max_age = if cached.info.dynamic_range.is_empty() && cached.info.vrr.is_empty() {
                Duration::from_secs(5)
            } else {
                Duration::from_secs(60)
            };
            if cached.vendor_id == vendor_id
                && cached.gpu_model == gpu_model
                && cached.captured_at.elapsed() < max_age
            {
                log_vendor("returning cached vendor info");
                return cached.info.clone();
            }
        }
    }

    let mut info = match vendor_id {
        VENDOR_NVIDIA => query_nvapi(),
        VENDOR_AMD => query_adlx(),
        VENDOR_INTEL => query_igcl(),
        other => {
            log_vendor_warn(&format!("unsupported GPU vendor 0x{other:04X}"));
            VendorDisplayInfo::default()
        }
    };
    info.gpu_model = gpu_model.clone();
    info.vendor = match vendor_id {
        VENDOR_NVIDIA => "nvidia",
        VENDOR_AMD => "amd",
        VENDOR_INTEL => "intel",
        _ => "",
    }
    .to_string();
    if vendor_id == VENDOR_NVIDIA {
        info.reference_mode = nvidia_reference_mode_enabled();
        log_vendor(&format!(
            "NVIDIA reference mode={}",
            if info.reference_mode {
                "enabled"
            } else {
                "disabled"
            }
        ));
    }

    if let Ok(mut guard) = cache.lock() {
        *guard = Some(CachedVendorInfo {
            info: info.clone(),
            vendor_id,
            gpu_model,
            captured_at: Instant::now(),
        });
    }
    info
}

// Bypasses the short/long-lived cache for settings changed directly in a
// vendor control panel. Those changes do not reliably produce WM_DISPLAYCHANGE
// or WM_SETTINGCHANGE, so the frontend can call this from a low-frequency,
// informational refresh without re-querying all display modes and EDID data.
pub fn query_vendor_display_info_fresh() -> VendorDisplayInfo {
    let _query_guard = VENDOR_QUERY_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(cache) = VENDOR_CACHE.get() {
        if let Ok(mut guard) = cache.lock() {
            *guard = None;
        }
    }
    query_vendor_display_info_locked()
}

// ── AMD ADLX (optional runtime) ───────────────────────────────────────────
// ADLX is a COM-like C ABI. Keep the bindings local and minimal so the app
// does not link against or redistribute AMD's SDK. The driver supplies
// amdadlx64.dll when AMD Software is installed.
const ADLX_OK: i32 = 0;
const ADLX_PIXEL_RGB_FULL: i32 = 1;
const ADLX_PIXEL_YCBCR444: i32 = 2;
const ADLX_PIXEL_YCBCR422: i32 = 3;
const ADLX_PIXEL_RGB_LIMITED: i32 = 4;
const ADLX_PIXEL_YCBCR420: i32 = 5;
const ADLX_FULL_VERSION: u64 = (1u64 << 48) | (5u64 << 32) | 124;

type AdlxResult = i32;
type AdlxInitialize = unsafe extern "C" fn(u64, *mut *mut AdlxSystem) -> AdlxResult;
type AdlxTerminate = unsafe extern "C" fn() -> AdlxResult;
type AdlxQueryFullVersion = unsafe extern "C" fn(*mut u64) -> AdlxResult;

type AdlxAcquire<T> = unsafe extern "system" fn(*mut T) -> i32;
type AdlxRelease<T> = unsafe extern "system" fn(*mut T) -> i32;
type AdlxSystemQueryInterface =
    unsafe extern "system" fn(*mut AdlxSystem, *const u16, *mut *mut c_void) -> AdlxResult;
type AdlxGetDisplaysServices =
    unsafe extern "system" fn(*mut AdlxSystem, *mut *mut AdlxDisplayServices) -> AdlxResult;
#[repr(C)]
struct AdlxSystemVtbl {
    get_hybrid_graphics_type: *const c_void,
    get_gpus: *const c_void,
    query_interface: AdlxSystemQueryInterface,
    get_displays_services: AdlxGetDisplaysServices,
    rest: [*const c_void; 8],
}
#[repr(C)]
struct AdlxSystem {
    vtbl: *const AdlxSystemVtbl,
}

type AdlxDisplayServicesQuery =
    unsafe extern "system" fn(*mut AdlxDisplayServices, *const u16, *mut *mut c_void) -> AdlxResult;
type AdlxGetDisplays =
    unsafe extern "system" fn(*mut AdlxDisplayServices, *mut *mut AdlxDisplayList) -> AdlxResult;
type AdlxGetFreeSync = unsafe extern "system" fn(
    *mut AdlxDisplayServices,
    *mut AdlxDisplay,
    *mut *mut AdlxDisplayFreeSync,
) -> AdlxResult;
type AdlxGetPixelFormat = unsafe extern "system" fn(
    *mut AdlxDisplayServices,
    *mut AdlxDisplay,
    *mut *mut AdlxPixelFormat,
) -> AdlxResult;
#[repr(C)]
struct AdlxDisplayServicesVtbl {
    acquire: AdlxAcquire<AdlxDisplayServices>,
    release: AdlxRelease<AdlxDisplayServices>,
    query_interface: AdlxDisplayServicesQuery,
    get_number: *const c_void,
    get_displays: AdlxGetDisplays,
    get_3dlut: *const c_void,
    get_gamut: *const c_void,
    get_gamma: *const c_void,
    get_changed: *const c_void,
    get_freesync: AdlxGetFreeSync,
    get_vsr: *const c_void,
    get_scaling: *const c_void,
    get_scaling_mode: *const c_void,
    get_integer_scaling: *const c_void,
    get_color_depth: *const c_void,
    get_pixel_format: AdlxGetPixelFormat,
    // GetCustomColor, GetHDCP, GetCustomResolution, GetVariBright.
    rest: [*const c_void; 4],
}
#[repr(C)]
struct AdlxDisplayServices {
    vtbl: *const AdlxDisplayServicesVtbl,
}

type AdlxListAt =
    unsafe extern "system" fn(*mut AdlxDisplayList, u32, *mut *mut AdlxDisplay) -> AdlxResult;
#[repr(C)]
struct AdlxDisplayListVtbl {
    acquire: AdlxAcquire<AdlxDisplayList>,
    release: AdlxRelease<AdlxDisplayList>,
    query_interface: *const c_void,
    size: *const c_void,
    empty: *const c_void,
    begin: *const c_void,
    end: *const c_void,
    at: *const c_void,
    clear: *const c_void,
    remove_back: *const c_void,
    add_back: *const c_void,
    at_display_list: AdlxListAt,
    add_back_display_list: *const c_void,
}
#[repr(C)]
struct AdlxDisplayList {
    vtbl: *const AdlxDisplayListVtbl,
}

#[repr(C)]
struct AdlxDisplayVtbl {
    acquire: AdlxAcquire<AdlxDisplay>,
    release: AdlxRelease<AdlxDisplay>,
    query_interface: *const c_void,
    manufacturer_id: *const c_void,
    display_type: *const c_void,
    connector_type: *const c_void,
    name: *const c_void,
    edid: *const c_void,
    native_resolution: *const c_void,
    refresh_rate: *const c_void,
    pixel_clock: *const c_void,
    scan_type: *const c_void,
    get_gpu: *const c_void,
    unique_id: *const c_void,
}
#[repr(C)]
struct AdlxDisplay {
    vtbl: *const AdlxDisplayVtbl,
}

type AdlxFreeSyncQuery = unsafe extern "system" fn(*mut AdlxDisplayFreeSync, *mut u8) -> AdlxResult;
#[repr(C)]
struct AdlxFreeSyncVtbl {
    acquire: AdlxAcquire<AdlxDisplayFreeSync>,
    release: AdlxRelease<AdlxDisplayFreeSync>,
    query_interface: *const c_void,
    is_supported: AdlxFreeSyncQuery,
    is_enabled: AdlxFreeSyncQuery,
    set_enabled: *const c_void,
}
#[repr(C)]
struct AdlxDisplayFreeSync {
    vtbl: *const AdlxFreeSyncVtbl,
}

type AdlxPixelGetValue = unsafe extern "system" fn(*mut AdlxPixelFormat, *mut i32) -> AdlxResult;
type AdlxPixelSupport = unsafe extern "system" fn(*mut AdlxPixelFormat, *mut u8) -> AdlxResult;
#[repr(C)]
struct AdlxPixelFormatVtbl {
    acquire: AdlxAcquire<AdlxPixelFormat>,
    release: AdlxRelease<AdlxPixelFormat>,
    query_interface: *const c_void,
    is_supported: AdlxPixelSupport,
    get_value: AdlxPixelGetValue,
    set_value: *const c_void,
    is_supported_format: *const c_void,
    is_supported_rgb_full: AdlxPixelSupport,
    is_supported_ycbcr444: AdlxPixelSupport,
    is_supported_ycbcr422: AdlxPixelSupport,
    is_supported_rgb_limited: AdlxPixelSupport,
    is_supported_ycbcr420: AdlxPixelSupport,
}
#[repr(C)]
struct AdlxPixelFormat {
    vtbl: *const AdlxPixelFormatVtbl,
}

struct AdlxLibrary {
    handle: HMODULE,
    terminate: AdlxTerminate,
}
impl Drop for AdlxLibrary {
    fn drop(&mut self) {
        unsafe {
            let status = (self.terminate)();
            log_vendor(&format!("ADLXTerminate status={status}"));
            // Do not unload a live ADLX runtime if termination failed. The
            // driver may still own interface objects; invalidating their code
            // pages could crash a later driver callback.
            if status == ADLX_OK {
                let _ = FreeLibrary(self.handle);
            } else {
                log_vendor_warn("ADLXTerminate failed; leaving ADLX runtime loaded");
            }
        }
    }
}

fn utf16z(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn adlx_export<T>(module: HMODULE, name: &[u8]) -> Option<T> {
    let pointer = GetProcAddress(module, PCSTR(name.as_ptr()))?;
    Some(std::mem::transmute_copy(&pointer))
}

unsafe fn adlx_release<T>(object: *mut T, release: unsafe extern "system" fn(*mut T) -> i32) {
    if !object.is_null() {
        let _ = release(object);
    }
}

fn query_adlx() -> VendorDisplayInfo {
    log_vendor("query_adlx starting");
    unsafe {
        let module = match LoadLibraryW(windows::core::w!("amdadlx64.dll")) {
            Ok(module) => module,
            Err(error) => {
                log_vendor_warn(&format!("ADLX runtime unavailable: {error:?}"));
                return VendorDisplayInfo::default();
            }
        };
        let initialize = match adlx_export::<AdlxInitialize>(module, b"ADLXInitialize\0") {
            Some(function) => function,
            None => {
                let _ = FreeLibrary(module);
                log_vendor_warn("ADLXInitialize export not found");
                return VendorDisplayInfo::default();
            }
        };
        let terminate = match adlx_export::<AdlxTerminate>(module, b"ADLXTerminate\0") {
            Some(function) => function,
            None => {
                let _ = FreeLibrary(module);
                log_vendor_warn("ADLXTerminate export not found");
                return VendorDisplayInfo::default();
            }
        };
        let full_version = adlx_export::<AdlxQueryFullVersion>(module, b"ADLXQueryFullVersion\0")
            .and_then(|query| {
                let mut version = 0u64;
                if (query)(&mut version) == ADLX_OK {
                    Some(version)
                } else {
                    None
                }
            })
            .unwrap_or(ADLX_FULL_VERSION);
        let mut system = std::ptr::null_mut();
        let status = initialize(full_version, &mut system);
        log_vendor(&format!(
            "ADLXInitialize status={status} version=0x{full_version:016X}"
        ));
        // Do not treat ALREADY_INITIALIZED as ownership of another ADLX
        // client’s session. Only terminate a session this call initialized.
        if status != ADLX_OK || system.is_null() {
            let _ = FreeLibrary(module);
            return VendorDisplayInfo::default();
        }
        let library = AdlxLibrary {
            handle: module,
            terminate,
        };
        let system_vtbl = (*system).vtbl;
        let mut services = std::ptr::null_mut();
        if ((*system_vtbl).get_displays_services)(system, &mut services) != ADLX_OK
            || services.is_null()
        {
            drop(library);
            return VendorDisplayInfo::default();
        }
        let services_vtbl = (*services).vtbl;
        let mut displays = std::ptr::null_mut();
        if ((*services_vtbl).get_displays)(services, &mut displays) != ADLX_OK || displays.is_null()
        {
            adlx_release(services, (*services_vtbl).release);
            drop(library);
            return VendorDisplayInfo::default();
        }
        let list_vtbl = (*displays).vtbl;
        let mut display = std::ptr::null_mut();
        let result = if ((*list_vtbl).at_display_list)(displays, 0, &mut display) != ADLX_OK
            || display.is_null()
        {
            VendorDisplayInfo::default()
        } else {
            let display_vtbl = (*display).vtbl;
            let mut info = VendorDisplayInfo::default();
            let mut pixel = std::ptr::null_mut();
            if ((*services_vtbl).get_pixel_format)(services, display, &mut pixel) == ADLX_OK
                && !pixel.is_null()
            {
                let pixel_vtbl = (*pixel).vtbl;
                let mut format = 0i32;
                if ((*pixel_vtbl).get_value)(pixel, &mut format) == ADLX_OK {
                    info.dynamic_range = match format {
                        ADLX_PIXEL_RGB_FULL => "Full (0–255)",
                        ADLX_PIXEL_RGB_LIMITED => "Limited (16–235)",
                        // ADLX reports YCbCr encoding here, not the
                        // quantization-range switch. Do not claim Limited
                        // without evidence; the driver may use a mode-specific
                        // range for the selected YCbCr transport.
                        ADLX_PIXEL_YCBCR444 | ADLX_PIXEL_YCBCR422 | ADLX_PIXEL_YCBCR420 => {
                            "YCbCr (driver-defined)"
                        }
                        _ => "",
                    }
                    .to_string();
                }
                adlx_release(pixel, (*pixel_vtbl).release);
            }
            let mut freesync = std::ptr::null_mut();
            if ((*services_vtbl).get_freesync)(services, display, &mut freesync) == ADLX_OK
                && !freesync.is_null()
            {
                let fs_vtbl = (*freesync).vtbl;
                let mut supported = 0u8;
                let mut enabled = 0u8;
                let supports_ok = ((*fs_vtbl).is_supported)(freesync, &mut supported) == ADLX_OK;
                let enabled_ok = ((*fs_vtbl).is_enabled)(freesync, &mut enabled) == ADLX_OK;
                info.vrr = if supports_ok && supported != 0 {
                    if enabled_ok && enabled != 0 {
                        "Enabled (FreeSync)"
                    } else {
                        "Supported (FreeSync)"
                    }
                } else {
                    "Not supported"
                }
                .to_string();
                adlx_release(freesync, (*fs_vtbl).release);
            }
            adlx_release(display, (*display_vtbl).release);
            info
        };
        adlx_release(displays, (*list_vtbl).release);
        adlx_release(services, (*services_vtbl).release);
        drop(library);
        log_vendor(&format!(
            "ADLX result: dynamic_range='{}', vrr='{}'",
            result.dynamic_range, result.vrr
        ));
        result
    }
}

// ── Intel IGCL / ControlLib (optional runtime) ────────────────────────────
// Intel ships the Control API runtime with its graphics driver. It is a flat
// C API, so unlike ADLX it needs no vtable or COM lifetime management.
const CTL_OK: u32 = 0;
const CTL_VERSION_1_1: u32 = (1 << 16) | 1;
const CTL_QUANTIZATION_FLAG: u32 = 1 << 3;
const CTL_QUANTIZATION_DEFAULT: u32 = 0;
const CTL_QUANTIZATION_LIMITED: u32 = 1;
const CTL_QUANTIZATION_FULL: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CtlApplicationId {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}
#[repr(C)]
#[derive(Clone, Copy)]
struct CtlInitArgs {
    size: u32,
    version: u8,
    _pad: [u8; 3],
    app_version: u32,
    flags: u32,
    supported_version: u32,
    application_uid: CtlApplicationId,
}
impl Default for CtlInitArgs {
    fn default() -> Self {
        Self {
            size: std::mem::size_of::<Self>() as u32,
            version: 0,
            _pad: [0; 3],
            app_version: CTL_VERSION_1_1,
            flags: 0,
            supported_version: 0,
            application_uid: CtlApplicationId::default(),
        }
    }
}
type CtlResult = u32;
type CtlApiHandle = *mut c_void;
type CtlAdapterHandle = *mut c_void;
type CtlDisplayHandle = *mut c_void;
type CtlInit = unsafe extern "C" fn(*mut CtlInitArgs, *mut CtlApiHandle) -> CtlResult;
type CtlClose = unsafe extern "C" fn(CtlApiHandle) -> CtlResult;
type CtlEnumDevices =
    unsafe extern "C" fn(CtlApiHandle, *mut u32, *mut CtlAdapterHandle) -> CtlResult;
type CtlEnumDisplays =
    unsafe extern "C" fn(CtlAdapterHandle, *mut u32, *mut CtlDisplayHandle) -> CtlResult;
type CtlGetSettings = unsafe extern "C" fn(CtlDisplayHandle, *mut CtlDisplaySettings) -> CtlResult;
type CtlGetArcInfo = unsafe extern "C" fn(CtlDisplayHandle, *mut CtlArcMonitorParams) -> CtlResult;
type CtlGetArcProfile =
    unsafe extern "C" fn(CtlDisplayHandle, *mut CtlArcProfileParams) -> CtlResult;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CtlDisplaySettings {
    size: u32,
    version: u8,
    set: u8,
    _pad: [u8; 2],
    supported_flags: u32,
    controllable_flags: u32,
    valid_flags: u32,
    low_latency: u32,
    source_tm: u32,
    content_type: u32,
    quantization_range: u32,
    supported_picture_ar: u32,
    picture_ar: u32,
    audio: u32,
    reserved: [u32; 25],
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CtlArcMonitorParams {
    size: u32,
    version: u8,
    supported: u8,
    _pad: [u8; 2],
    min_hz: f32,
    max_hz: f32,
    max_increase_us: u32,
    max_decrease_us: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CtlArcProfileParams {
    size: u32,
    version: u8,
    _pad: [u8; 3],
    profile: u32,
    max_hz: f32,
    min_hz: f32,
    max_increase_us: u32,
    max_decrease_us: u32,
}

struct CtlLibrary {
    handle: HMODULE,
    close: CtlClose,
    api: CtlApiHandle,
}
impl Drop for CtlLibrary {
    fn drop(&mut self) {
        unsafe {
            let status = (self.close)(self.api);
            log_vendor(&format!("ctlClose status=0x{status:08X}"));
            // SUCCESS_STILL_OPEN_BY_ANOTHER_CALLER is also a successful close
            // from this client's perspective. Never FreeLibrary after a
            // failed close because IGCL may still execute through this module.
            if status == CTL_OK || status == 1 {
                let _ = FreeLibrary(self.handle);
            } else {
                log_vendor_warn("ctlClose failed; leaving Intel ControlLib loaded");
            }
        }
    }
}

unsafe fn ctl_export<T>(module: HMODULE, name: &[u8]) -> Option<T> {
    let pointer = GetProcAddress(module, PCSTR(name.as_ptr()))?;
    Some(std::mem::transmute_copy(&pointer))
}

fn query_igcl() -> VendorDisplayInfo {
    log_vendor("query_igcl starting");
    unsafe {
        let names = ["ControlLib.dll", "ControlLib1.dll"];
        let mut module = None;
        for name in names {
            let wide = utf16z(name);
            if let Ok(handle) = LoadLibraryW(windows::core::PCWSTR(wide.as_ptr())) {
                module = Some(handle);
                break;
            }
        }
        let Some(module) = module else {
            log_vendor_warn("Intel ControlLib runtime unavailable");
            return VendorDisplayInfo::default();
        };
        let Some(init) = ctl_export::<CtlInit>(module, b"ctlInit\0") else {
            let _ = FreeLibrary(module);
            log_vendor_warn("ctlInit export not found");
            return VendorDisplayInfo::default();
        };
        let close = match ctl_export::<CtlClose>(module, b"ctlClose\0") {
            Some(function) => function,
            None => {
                let _ = FreeLibrary(module);
                log_vendor_warn("ctlClose export not found");
                return VendorDisplayInfo::default();
            }
        };
        let enum_devices = ctl_export::<CtlEnumDevices>(module, b"ctlEnumerateDevices\0");
        let enum_displays = ctl_export::<CtlEnumDisplays>(module, b"ctlEnumerateDisplayOutputs\0");
        let get_settings = ctl_export::<CtlGetSettings>(module, b"ctlGetSetDisplaySettings\0");
        let get_arc = ctl_export::<CtlGetArcInfo>(module, b"ctlGetIntelArcSyncInfoForMonitor\0");
        let get_profile = ctl_export::<CtlGetArcProfile>(module, b"ctlGetIntelArcSyncProfile\0");
        let (Some(enum_devices), Some(enum_displays), Some(get_settings)) =
            (enum_devices, enum_displays, get_settings)
        else {
            let _ = FreeLibrary(module);
            log_vendor_warn("Intel ControlLib missing required display exports");
            return VendorDisplayInfo::default();
        };
        let mut args = CtlInitArgs::default();
        let mut api = std::ptr::null_mut();
        let status = init(&mut args, &mut api);
        log_vendor(&format!(
            "ctlInit status=0x{status:08X} version=0x{:08X}",
            args.supported_version
        ));
        if status != CTL_OK || api.is_null() {
            let _ = FreeLibrary(module);
            return VendorDisplayInfo::default();
        }
        let library = CtlLibrary {
            handle: module,
            close,
            api,
        };
        let mut device_count = 0u32;
        if enum_devices(api, &mut device_count, std::ptr::null_mut()) != CTL_OK || device_count == 0
        {
            drop(library);
            return VendorDisplayInfo::default();
        }
        let mut devices = vec![std::ptr::null_mut(); device_count as usize];
        if enum_devices(api, &mut device_count, devices.as_mut_ptr()) != CTL_OK {
            drop(library);
            return VendorDisplayInfo::default();
        }
        let mut output_count = 0u32;
        if enum_displays(devices[0], &mut output_count, std::ptr::null_mut()) != CTL_OK
            || output_count == 0
        {
            drop(library);
            return VendorDisplayInfo::default();
        }
        let mut outputs = vec![std::ptr::null_mut(); output_count as usize];
        if enum_displays(devices[0], &mut output_count, outputs.as_mut_ptr()) != CTL_OK
            || outputs[0].is_null()
        {
            drop(library);
            return VendorDisplayInfo::default();
        }
        let output = outputs[0];
        let mut info = VendorDisplayInfo::default();
        let mut settings = CtlDisplaySettings::default();
        settings.size = std::mem::size_of::<CtlDisplaySettings>() as u32;
        settings.set = 0;
        if get_settings(output, &mut settings) == CTL_OK
            && settings.supported_flags & CTL_QUANTIZATION_FLAG != 0
        {
            info.dynamic_range = match settings.quantization_range {
                CTL_QUANTIZATION_FULL => "Full (0–255)",
                CTL_QUANTIZATION_LIMITED => "Limited (16–235)",
                CTL_QUANTIZATION_DEFAULT => "Default",
                _ => "",
            }
            .to_string();
        }
        let mut arc = CtlArcMonitorParams {
            size: std::mem::size_of::<CtlArcMonitorParams>() as u32,
            ..Default::default()
        };
        if let Some(get_arc) = get_arc {
            let arc_status = get_arc(output, &mut arc);
            if arc_status == CTL_OK && arc.supported != 0 {
                let profile_status = if let Some(get_profile) = get_profile {
                    let mut profile = CtlArcProfileParams {
                        size: std::mem::size_of::<CtlArcProfileParams>() as u32,
                        ..Default::default()
                    };
                    let status = get_profile(output, &mut profile);
                    if status == CTL_OK {
                        info.vrr = if profile.profile == 5 {
                            "Disabled (Arc Sync)"
                        } else {
                            "Enabled (Arc Sync)"
                        }
                        .to_string();
                    }
                    status
                } else {
                    CTL_OK + 1
                };
                if profile_status != CTL_OK && info.vrr.is_empty() {
                    info.vrr = "Supported (Arc Sync)".to_string();
                }
            } else if arc_status == CTL_OK {
                info.vrr = "Not supported".to_string();
            }
        }
        drop(library);
        log_vendor(&format!(
            "IGCL result: dynamic_range='{}', vrr='{}'",
            info.dynamic_range, info.vrr
        ));
        info
    }
}

// ── Public NVAPI interface IDs ────────────────────────────────────────────
// These are the fixed NVIDIA-assigned IDs from NVIDIA's public
// nvapi_interface.h, not CRC32 values and not guessed IDs.
const NVAPI_INITIALIZE_ID: u32 = 0x0150E828;
const NVAPI_UNLOAD_ID: u32 = 0xD22BDD7E;
const NVAPI_ENUM_PHYSICAL_GPUS_ID: u32 = 0xE5AC921F;
const NVAPI_GPU_GET_CONNECTED_DISPLAY_IDS_ID: u32 = 0x0078DBA2;
const NVAPI_DISP_COLOR_CONTROL_ID: u32 = 0x92F9D80D;
const NVAPI_DISP_GET_MONITOR_CAPABILITIES_ID: u32 = 0x3B05C7E1;
const NVAPI_DISP_GET_VRR_INFO_ID: u32 = 0xDF8FDA57;
const NVAPI_DISP_GET_ADAPTIVE_SYNC_DATA_ID: u32 = 0xB73D1EE9;
const NVAPI_DISP_GET_GDI_PRIMARY_DISPLAY_ID: u32 = 0x1E9D8A31;

const NVAPI_OK: i32 = 0;
const NV_COLOR_CMD_GET: u8 = 1;
const NV_DYNAMIC_RANGE_VESA: u8 = 0;
const NV_DYNAMIC_RANGE_CEA: u8 = 1;

// NV_COLOR_DATA_V5 is the current public NVAPI color structure. V2 puts
// dynamicRange at byte 9; V5 adds the aligned bpc/policy/depth fields and
// puts dynamicRange at byte 10. V5 is 24 bytes with public-header packing.
const COLOR_DATA_V5_SIZE: usize = 24;
const COLOR_DATA_V5_VERSION: u32 = COLOR_DATA_V5_SIZE as u32 | (5 << 16);
// V5 nested data begins at byte 8: colorFormat=8, colorimetry=9,
// dynamicRange=10, bpc=12..15, policy=16..19, depth=20..23.
const COLOR_DATA_DYNAMIC_RANGE_OFFSET: usize = 10;

// NV_GPU_DISPLAYIDS_VER2 is MAKE_NVAPI_VERSION(NV_GPU_DISPLAYIDS, 3).
// The version number is 3 even though the type is called "VER2".
const DISPLAY_IDS_VERSION: u32 = 16 | (3 << 16);

// NV_GPU_DISPLAYIDS from nvapi.h. sizeof is 16:
// version (0), connectorType (4), displayId (8), flags bitfield (12).
const DISPLAY_IDS_SIZE: usize = 16;
const DISPLAY_IDS_IS_ACTIVE: u32 = 1 << 2;
const DISPLAY_IDS_IS_CONNECTED: u32 = 1 << 6;

// NV_MONITOR_CAPABILITIES_V1 from nvapi.h.
const NV_MONITOR_CAPS_TYPE_GENERIC: u32 = 0x1002;
const MONITOR_CAPS_STRUCT_SIZE: usize = 68;
const MONITOR_CAPABILITIES_VERSION: u32 = MONITOR_CAPS_STRUCT_SIZE as u32 | (1 << 16);
const MONITOR_CAPS_DATA_OFFSET: usize = 17;

// NV_GET_VRR_INFO_V1 from nvapi.h: version + flags + reservedEx[4].
const VRR_INFO_SIZE: usize = 24;
const VRR_INFO_VERSION: u32 = VRR_INFO_SIZE as u32 | (1 << 16);
// VRR_REQUESTED is the driver's configured/requested state. Do not use
// VRR_POSSIBLE as an enabled signal: it describes capability/possibility and
// stays set when the user disables G-SYNC/VRR in the NVIDIA driver settings.
const VRR_REQUESTED: u32 = 1 << 2;
const VRR_DISPLAY_MODE: u32 = 1 << 4;

// NV_GET_ADAPTIVE_SYNC_DATA_V1 from nvapi.h. Its fields are
// version, maxFrameInterval, flags, lastFlipRefreshCount, timestamp, and
// reservedEx[4] = 40 bytes total (not 48).
const ADAPTIVE_SYNC_DATA_SIZE: usize = 40;
const ADAPTIVE_SYNC_DATA_VERSION: u32 = ADAPTIVE_SYNC_DATA_SIZE as u32 | (1 << 16);
const ADAPTIVE_SYNC_DISABLED: u32 = 1 << 0;

type NvapiQueryInterface = unsafe extern "system" fn(id: u32) -> *const std::ffi::c_void;
type NvapiInitialize = unsafe extern "system" fn() -> i32;
type NvapiUnload = unsafe extern "system" fn() -> i32;
type NvapiGetGdiPrimaryDisplayId = unsafe extern "system" fn(display_id: *mut u32) -> i32;
type NvapiEnumPhysicalGpus =
    unsafe extern "system" fn(handles: *mut *const std::ffi::c_void, count: *mut u32) -> i32;
type NvapiGetConnectedDisplayIds = unsafe extern "system" fn(
    gpu: *const std::ffi::c_void,
    display_ids: *mut u8,
    count: *mut u32,
    flags: u32,
) -> i32;
type NvapiDispColorControl = unsafe extern "system" fn(display_id: u32, color_data: *mut u8) -> i32;
type NvapiDispGetMonitorCaps = unsafe extern "system" fn(display_id: u32, caps: *mut u8) -> i32;
type NvapiDispGetVrrInfo = unsafe extern "system" fn(display_id: u32, info: *mut u8) -> i32;
type NvapiDispGetAdaptiveSyncData =
    unsafe extern "system" fn(display_id: u32, data: *mut u8) -> i32;

struct NvapiLibrary {
    handle: windows::Win32::Foundation::HMODULE,
    initialized: bool,
    unload_succeeded: bool,
}

impl Drop for NvapiLibrary {
    fn drop(&mut self) {
        // Before NvAPI_Initialize succeeds, the module can be released
        // directly. After initialization, only release it when the paired
        // NvAPI_Unload succeeded; NVAPI_API_IN_USE means the process must keep
        // the module resident rather than invalidating driver state.
        if !self.initialized || self.unload_succeeded {
            unsafe {
                let released = FreeLibrary(self.handle).is_ok();
                if released {
                    log_vendor_lifecycle("nvapi64.dll reference released successfully");
                } else {
                    log_vendor_warn("nvapi64.dll FreeLibrary failed");
                }
            }
        } else {
            log_vendor_warn(
                "nvapi64.dll retained because NvAPI_Unload did not complete successfully",
            );
        }
    }
}

struct NvapiContext {
    query_interface: NvapiQueryInterface,
    gpu: *const std::ffi::c_void,
    // Keep the DLL loaded while all returned function pointers are used.
    dll: NvapiLibrary,
}

// All uses of the context (including the cached RTX HDR session) happen while
// VENDOR_QUERY_LOCK is held, so the raw query-interface pointer and GPU handle
// are never touched concurrently even when the context is shared across
// threads via the cache below. Safe to move between threads under that lock.
unsafe impl Send for NvapiContext {}
unsafe impl Sync for NvapiContext {}

impl Drop for NvapiContext {
    fn drop(&mut self) {
        unsafe {
            let unload_ptr = (self.query_interface)(NVAPI_UNLOAD_ID);
            if !unload_ptr.is_null() {
                let unload: NvapiUnload = std::mem::transmute(unload_ptr);
                let status = unload();
                self.dll.unload_succeeded = status == NVAPI_OK;
                log_vendor_lifecycle(&format!("NvAPI_Unload status={status}"));
            } else {
                self.dll.unload_succeeded = false;
                log_vendor_warn("NvAPI_Unload QueryInterface returned null");
            }
        }
        // `dll` then drops and only frees the module after a successful unload.
    }
}

unsafe fn unload_nvapi(query_interface: NvapiQueryInterface) -> bool {
    let unload_ptr = query_interface(NVAPI_UNLOAD_ID);
    if unload_ptr.is_null() {
        log_vendor_warn("NvAPI_Unload QueryInterface returned null during init cleanup");
        return false;
    }
    let unload: NvapiUnload = std::mem::transmute(unload_ptr);
    let status = unload();
    log_vendor_lifecycle(&format!("NvAPI_Unload init-cleanup status={status}"));
    status == NVAPI_OK
}

fn nvapi_init() -> Option<NvapiContext> {
    log_vendor("nvapi_init starting");
    unsafe {
        let dll = match LoadLibraryW(windows::core::w!("nvapi64.dll")) {
            Ok(module) => module,
            Err(error) => {
                log_vendor_warn(&format!("LoadLibraryW(nvapi64.dll) failed: {error:?}"));
                return None;
            }
        };
        log_vendor("nvapi64.dll loaded");

        let mut library = NvapiLibrary {
            handle: dll,
            initialized: false,
            unload_succeeded: false,
        };
        let query_ptr =
            match GetProcAddress(library.handle, windows::core::s!("nvapi_QueryInterface")) {
                Some(pointer) => pointer,
                None => {
                    log_vendor_warn("nvapi_QueryInterface export not found");
                    return None;
                }
            };
        let query_interface: NvapiQueryInterface = std::mem::transmute(query_ptr);

        let initialize_ptr = query_interface(NVAPI_INITIALIZE_ID);
        if initialize_ptr.is_null() {
            log_vendor_warn("NvAPI_Initialize QueryInterface returned null");
            return None;
        }
        let initialize: NvapiInitialize = std::mem::transmute(initialize_ptr);
        let initialize_status = initialize();
        log_vendor(&format!("NvAPI_Initialize status={initialize_status}"));
        if initialize_status != NVAPI_OK {
            return None;
        }
        library.initialized = true;

        let enum_ptr = query_interface(NVAPI_ENUM_PHYSICAL_GPUS_ID);
        if enum_ptr.is_null() {
            log_vendor_warn("NvAPI_EnumPhysicalGPUs QueryInterface returned null");
            library.unload_succeeded = unload_nvapi(query_interface);
            return None;
        }
        let enum_gpus: NvapiEnumPhysicalGpus = std::mem::transmute(enum_ptr);
        let mut gpu: *const std::ffi::c_void = std::ptr::null();
        let mut gpu_count = 1u32;
        let enum_status = enum_gpus(&mut gpu, &mut gpu_count);
        log_vendor(&format!(
            "NvAPI_EnumPhysicalGPUs status={enum_status} count={gpu_count} handle=0x{:X}",
            gpu as usize
        ));
        if enum_status != NVAPI_OK || gpu.is_null() {
            library.unload_succeeded = unload_nvapi(query_interface);
            return None;
        }

        Some(NvapiContext {
            query_interface,
            gpu,
            dll: library,
        })
    }
}

unsafe fn nvapi_function<T>(context: &NvapiContext, id: u32, name: &str) -> Option<T> {
    let pointer = (context.query_interface)(id);
    if pointer.is_null() {
        log_vendor_warn(&format!("{name}: QueryInterface(0x{id:08X}) returned null"));
        None
    } else {
        log_vendor(&format!("{name}: QueryInterface(0x{id:08X}) succeeded"));
        Some(std::mem::transmute_copy(&pointer))
    }
}

fn enumerate_active_display_id(context: &NvapiContext) -> Option<u32> {
    unsafe {
        // This is the authoritative GDI-primary display ID. It avoids the
        // driver-order ambiguity of GetConnectedDisplayIds on multi-monitor
        // systems. Older/Optimus paths may not expose it, so retain the
        // active-display fallback below.
        if let Some(get_primary) = nvapi_function::<NvapiGetGdiPrimaryDisplayId>(
            context,
            NVAPI_DISP_GET_GDI_PRIMARY_DISPLAY_ID,
            "NvAPI_DISP_GetGDIPrimaryDisplayId",
        ) {
            let mut display_id = 0u32;
            let status = get_primary(&mut display_id);
            log_vendor(&format!(
                "NvAPI_DISP_GetGDIPrimaryDisplayId status={status} id={display_id}"
            ));
            if status == NVAPI_OK && display_id != 0 {
                return Some(display_id);
            }
        }

        let get_ids = nvapi_function::<NvapiGetConnectedDisplayIds>(
            context,
            NVAPI_GPU_GET_CONNECTED_DISPLAY_IDS_ID,
            "NvAPI_GPU_GetConnectedDisplayIds",
        )?;
        const MAX_DISPLAY_IDS: usize = 16;
        let mut raw_ids = vec![0u8; MAX_DISPLAY_IDS * DISPLAY_IDS_SIZE];
        for index in 0..MAX_DISPLAY_IDS {
            let entry = raw_ids.as_mut_ptr().add(index * DISPLAY_IDS_SIZE);
            std::ptr::write(entry as *mut u32, DISPLAY_IDS_VERSION);
        }
        let mut count = MAX_DISPLAY_IDS as u32;
        let status = get_ids(context.gpu, raw_ids.as_mut_ptr(), &mut count, 0);
        log_vendor(&format!(
            "NvAPI_GPU_GetConnectedDisplayIds status={status} count={count}"
        ));
        if status != NVAPI_OK {
            return None;
        }

        let mut sole_active_display = None;
        let mut active_count = 0usize;
        for index in 0..(count as usize).min(MAX_DISPLAY_IDS) {
            let entry = raw_ids.as_ptr().add(index * DISPLAY_IDS_SIZE);
            let display_id = std::ptr::read(entry.add(8) as *const u32);
            let flags = std::ptr::read(entry.add(12) as *const u32);
            let active = flags & DISPLAY_IDS_IS_ACTIVE != 0;
            let connected = flags & DISPLAY_IDS_IS_CONNECTED != 0;
            log_vendor(&format!(
                "display id={display_id} active={active} connected={connected} flags=0x{flags:08X}"
            ));
            if active && connected {
                active_count += 1;
                sole_active_display = Some(display_id);
            }
        }
        if active_count == 1 {
            return sole_active_display;
        }
        if active_count > 1 {
            log_vendor_warn(
                "GDI-primary display lookup failed and multiple active displays were found; refusing an arbitrary display",
            );
        }
        None
    }
}

fn query_nvapi() -> VendorDisplayInfo {
    log_vendor("query_nvapi starting");
    // The app-wide vendor refresh runs this every 5 s. Reuse the cached
    // NVAPI context (kept alive for the RTX HDR DRS session) instead of
    // loading the DLL, initialising, querying, then unloading it on every
    // tick — that init/unload churn was a real stutter every few seconds.
    // Only fall back to a fresh context when the cache has no display ID
    // (e.g. right after a display reconfiguration). Boxed so the fresh
    // context lives for the whole query — the cached path borrows from the
    // static cache instead.
    let _owned_context;
    let (context, display_id) = match rtx_hdr_nvapi_with_display_id() {
        Some((context, Some(display_id))) => {
            log_vendor(&format!("reusing cached NVAPI context, display_id={display_id}"));
            (context, Some(display_id))
        }
        _ => match nvapi_init() {
            Some(context) => {
                let display_id = enumerate_active_display_id(&context);
                _owned_context = Some(Box::new(context));
                (unsafe { _owned_context.as_deref().unwrap_unchecked() }, display_id)
            }
            None => {
                log_vendor_warn("NVAPI initialization failed");
                return VendorDisplayInfo::default();
            }
        },
    };
    let Some(display_id) = display_id else {
        log_vendor_warn("no active NVIDIA display ID found");
        return VendorDisplayInfo::default();
    };
    log_vendor(&format!("using active NVIDIA display_id={display_id}"));

    let mut info = VendorDisplayInfo::default();

    // Dynamic range: public NvAPI_Disp_ColorControl with GET.
    unsafe {
        if let Some(color_control) = nvapi_function::<NvapiDispColorControl>(
            context,
            NVAPI_DISP_COLOR_CONTROL_ID,
            "NvAPI_Disp_ColorControl",
        ) {
            let mut color_data = [0u8; COLOR_DATA_V5_SIZE];
            std::ptr::write(color_data.as_mut_ptr() as *mut u32, COLOR_DATA_V5_VERSION);
            std::ptr::write(
                color_data.as_mut_ptr().add(4) as *mut u16,
                COLOR_DATA_V5_SIZE as u16,
            );
            // GET returns the currently applied output values. GET_DEFAULT
            // reports policy defaults, which can differ from the active
            // NVIDIA Control Panel range.
            color_data[6] = NV_COLOR_CMD_GET;
            let status = color_control(display_id, color_data.as_mut_ptr());
            log_vendor(&format!("NvAPI_Disp_ColorControl status={status}"));
            if status == NVAPI_OK {
                let range = color_data[COLOR_DATA_DYNAMIC_RANGE_OFFSET];
                info.dynamic_range = match range {
                    NV_DYNAMIC_RANGE_VESA => "Full (0–255)".to_string(),
                    NV_DYNAMIC_RANGE_CEA => "Limited (16–235)".to_string(),
                    other => {
                        log_vendor_warn(&format!("unknown NVAPI dynamic range={other}"));
                        String::new()
                    }
                };
                log_vendor(&format!("NVAPI dynamic range={range}"));
            }
        }
    }

    // VRR/G-SYNC capability and current state.
    unsafe {
        if let Some(get_caps) = nvapi_function::<NvapiDispGetMonitorCaps>(                        context,
                        NVAPI_DISP_GET_MONITOR_CAPABILITIES_ID,
                        "NvAPI_DISP_GetMonitorCapabilities",
                    ) {
            let mut caps = [0u8; MONITOR_CAPS_STRUCT_SIZE];
            std::ptr::write(caps.as_mut_ptr() as *mut u32, MONITOR_CAPABILITIES_VERSION);
            std::ptr::write(
                caps.as_mut_ptr().add(4) as *mut u16,
                MONITOR_CAPS_STRUCT_SIZE as u16,
            );
            std::ptr::write(
                caps.as_mut_ptr().add(8) as *mut u32,
                NV_MONITOR_CAPS_TYPE_GENERIC,
            );
            let status = get_caps(display_id, caps.as_mut_ptr());
            log_vendor(&format!(
                "NvAPI_DISP_GetMonitorCapabilities status={status}"
            ));
            if status == NVAPI_OK {
                let flags = caps[MONITOR_CAPS_DATA_OFFSET];
                let supports_vrr = flags & 0x01 != 0;
                let true_gsync = flags & 0x04 != 0;
                log_vendor(&format!(
                    "NVAPI monitor caps: supportVRR={supports_vrr} trueGsync={true_gsync} flags=0x{flags:08X}"
                ));

                if !supports_vrr {
                    info.vrr = "Not supported".to_string();
                } else {
                    let mut vrr_info = [0u8; VRR_INFO_SIZE];
                    std::ptr::write(vrr_info.as_mut_ptr() as *mut u32, VRR_INFO_VERSION);
                    if let Some(get_vrr) = nvapi_function::<NvapiDispGetVrrInfo>(                            context,
                            NVAPI_DISP_GET_VRR_INFO_ID,
                            "NvAPI_Disp_GetVRRInfo",
                        ) {
                        let vrr_status = get_vrr(display_id, vrr_info.as_mut_ptr());
                        let vrr_flags = std::ptr::read(vrr_info.as_ptr().add(4) as *const u32);
                        log_vendor(&format!(
                            "NvAPI_Disp_GetVRRInfo status={vrr_status} flags=0x{vrr_flags:08X}"
                        ));
                        if vrr_status == NVAPI_OK {
                            // The public header specifically defines
                            // bIsDisplayInVRRMode as the live state. The
                            // enabled/requested bits describe configuration,
                            // not proof that the panel is currently varying.
                            let active = vrr_flags & VRR_DISPLAY_MODE != 0;
                            info.vrr = if active {
                                if true_gsync {
                                    "Active (G-SYNC)"
                                } else {
                                    "Active (VRR)"
                                }
                            } else if vrr_flags & VRR_REQUESTED != 0 {
                                // Requested means the driver has VRR enabled
                                // as a configuration. It is deliberately the
                                // only non-live flag used here; capability
                                // bits remain set when the user disables VRR.
                                if true_gsync {
                                    "Supported (G-SYNC)"
                                } else {
                                    "Supported (VRR)"
                                }
                            } else {
                                // The monitor may support VRR, but the NVIDIA
                                // driver has it disabled.
                                if true_gsync {
                                    "Disabled (G-SYNC)"
                                } else {
                                    "Disabled (VRR)"
                                }
                            }
                            .to_string();
                        }
                    }

                    if info.vrr.is_empty() {
                        // AdaptiveSyncData does not directly report the live
                        // VRR state, but provides a useful fallback for whether
                        // the driver disabled adaptive sync on this display.
                        if let Some(get_adaptive) = nvapi_function::<NvapiDispGetAdaptiveSyncData>(
                            context,
                            NVAPI_DISP_GET_ADAPTIVE_SYNC_DATA_ID,
                            "NvAPI_DISP_GetAdaptiveSyncData",
                        ) {
                            let mut data = [0u8; ADAPTIVE_SYNC_DATA_SIZE];
                            std::ptr::write(
                                data.as_mut_ptr() as *mut u32,
                                ADAPTIVE_SYNC_DATA_VERSION,
                            );
                            let adaptive_status = get_adaptive(display_id, data.as_mut_ptr());
                            let adaptive_flags = std::ptr::read(data.as_ptr().add(8) as *const u32);
                            log_vendor(&format!(
                                "NvAPI_DISP_GetAdaptiveSyncData status={adaptive_status} flags=0x{adaptive_flags:08X}"
                            ));
                            if adaptive_status == NVAPI_OK {
                                // AdaptiveSyncData can confirm an explicit
                                // driver-disabled state, but an unset
                                // DISABLED bit is capability/configuration
                                // information rather than proof that VRR is
                                // enabled. Avoid reintroducing the old false
                                // positive when GetVRRInfo is unavailable.
                                info.vrr = if adaptive_flags & ADAPTIVE_SYNC_DISABLED != 0 {
                                    if true_gsync {
                                        "Disabled (G-SYNC)"
                                    } else {
                                        "Disabled (VRR)"
                                    }
                                } else {
                                    "Unknown (VRR)"
                                }
                                .to_string();
                            }
                        }
                    }
                }
            }
        }
    }

    log_vendor(&format!(
        "result: dynamic_range='{}', vrr='{}'",
        info.dynamic_range, info.vrr
    ));
    info
}

// ── RTX HDR (driver profile) ──────────────────────────────────────────────
// RTX HDR is a driver-profile setting (the "Global driver profile" rows that
// NVIDIA Profile Inspector shows under 06 - Rendering Features). There is no
// dedicated NVAPI call for it — it is written through the DRS (driver
// settings) API, exactly like the Control Panel does. We reuse the same
// interface IDs NVIDIA Profile Inspector uses (verified against its
// NvapiDrsWrapper.cs, which works on current drivers), and the setting IDs
// from its CustomSettingNames.xml:
//
//   RTX HDR - Enable       0x00DD48FB   (0 = off, 1 = on)
//   RTX HDR - Driver Flags 0x00432F84   (mode: 0 off, 2 VeryHigh debanding,
//                                       3 +indicator, 6 No debanding,
//                                       10 Medium debanding, 35 +indicator+debug)
//
// The Driver Flags value is the authoritative mode; the Enable master switch
// is kept in sync so the Control Panel agrees with what the app sets.
const NVAPI_DRS_CREATE_SESSION_ID: u32 = 0x0694D52E;
const NVAPI_DRS_DESTROY_SESSION_ID: u32 = 0xDAD9CFF8;
const NVAPI_DRS_LOAD_SETTINGS_ID: u32 = 0x375DBD6B;
const NVAPI_DRS_SAVE_SETTINGS_ID: u32 = 0xFCBC7E14;
const NVAPI_DRS_GET_CURRENT_GLOBAL_PROFILE_ID: u32 = 0x617BFF9F;
const NVAPI_DRS_GET_SETTING_ID: u32 = 0xEA99498D;
const NVAPI_DRS_SET_SETTING_ID: u32 = 0x8A2CF5F5;

// GetSetting returns this when the setting is not present in the profile
// (i.e. the user has never touched RTX HDR) — treated as "off".
const NVAPI_SETTING_NOT_FOUND: i32 = -160;

// Writing DRS settings (profiles/global profile) requires an elevated
// process; reads do not. The app normally runs non-elevated, so SetSetting
// returns this until the user restarts as Administrator. The frontend matches
// the marker below to offer that restart.
const NVAPI_INVALID_USER_PRIVILEGE: i32 = -137;
const ADMIN_REQUIRED_MARKER: &str = "ADMIN_REQUIRED:";

fn drs_write_error(operation: &str, status: i32) -> String {
    if status == NVAPI_INVALID_USER_PRIVILEGE {
        format!(
            "{ADMIN_REQUIRED_MARKER} {operation} needs administrator rights — restart OLED Helper as Administrator to change RTX HDR"
        )
    } else {
        format!("{operation} failed: {status}")
    }
}

const RTX_HDR_ENABLE_ID: u32 = 0x00DD48FB;
const RTX_HDR_DRIVER_FLAGS_ID: u32 = 0x00432F84;

// NVDRS_SETTING layout from NVIDIA's nvapi.h (_NVDRS_SETTING_V1, compiled
// with #pragma pack(4)):
//   version u32 | settingName NvAPI_UnicodeString (2048 WCHARs = 4096 bytes)
//   | settingId | settingType | settingLocation | isCurrentPredefined
//   | isPredefinedValid | union (predefined) | union (current)
// Each union is 4100 bytes (NVDRS_BINARY_SETTING = valueLength u32 +
// valueData[4096]) which is larger than the NvAPI_UnicodeString member, so
// the whole struct is 4 + 4096 + 20 + 4100 + 4100 = 12320 bytes. The DRS
// version constant embeds this size, and the driver validates it — passing
// the wrong size returns NVAPI_INCOMPATIBLE_STRUCT_VERSION (-9). Verified
// against Profile Inspector's C# NVDRS_SETTING (Marshal.SizeOf = 12320),
// which works on current drivers.
const NVDRS_SETTING_SIZE: u32 = 12320;
const NVDRS_SETTING_VER: u32 = (NVDRS_SETTING_SIZE & 0xFFFF) | (1 << 16);
const NVDRS_DWORD_TYPE: u32 = 0;
const NVDRS_CURRENT_PROFILE_LOCATION: u32 = 0;

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(non_snake_case)]
struct NvDrsSetting {
    version: u32,
    settingName: [u16; 2048],
    settingId: u32,
    settingType: u32,
    settingLocation: u32,
    isCurrentPredefined: u32,
    isPredefinedValid: u32,
    // predefined-value union: u32 slot + binary/string tail.
    u32PredefinedValue: u32,
    _predefined_tail: [u8; 4096],
    // current-value union: u32 slot (the only member we read/write) + tail.
    u32CurrentValue: u32,
    _current_tail: [u8; 4096],
}

// The big fixed-size arrays cannot derive Default, so build the zeroed
// struct manually with const-repeat expressions.
impl Default for NvDrsSetting {
    fn default() -> Self {
        Self {
            version: 0,
            settingName: [0u16; 2048],
            settingId: 0,
            settingType: 0,
            settingLocation: 0,
            isCurrentPredefined: 0,
            isPredefinedValid: 0,
            u32PredefinedValue: 0,
            _predefined_tail: [0u8; 4096],
            u32CurrentValue: 0,
            _current_tail: [0u8; 4096],
        }
    }
}

impl NvDrsSetting {
    fn dword(setting_id: u32, value: u32) -> Self {
        Self {
            version: NVDRS_SETTING_VER,
            settingId: setting_id,
            settingType: NVDRS_DWORD_TYPE,
            settingLocation: NVDRS_CURRENT_PROFILE_LOCATION,
            u32CurrentValue: value,
            ..Default::default()
        }
    }
}

// The DRS functions take opaque handle types (void*). Get/SetSetting use the
// "Ex" variants (extra trailing args) that modern drivers expose — the same
// signatures NVIDIA Profile Inspector's wrapper uses.
type NvapiDrsCreateSession = unsafe extern "system" fn(session: *mut *mut c_void) -> i32;
type NvapiDrsDestroySession = unsafe extern "system" fn(session: *mut c_void) -> i32;
type NvapiDrsLoadSettings = unsafe extern "system" fn(session: *mut c_void) -> i32;
type NvapiDrsSaveSettings = unsafe extern "system" fn(session: *mut c_void) -> i32;
type NvapiDrsGetCurrentGlobalProfile =
    unsafe extern "system" fn(session: *mut c_void, profile: *mut *mut c_void) -> i32;
type NvapiDrsGetSetting = unsafe extern "system" fn(
    session: *mut c_void,
    profile: *mut c_void,
    setting_id: u32,
    setting: *mut NvDrsSetting,
    unknown: *mut u32,
) -> i32;
type NvapiDrsSetSetting = unsafe extern "system" fn(
    session: *mut c_void,
    profile: *mut c_void,
    setting: *mut NvDrsSetting,
    unknown0: usize,
    unknown1: usize,
) -> i32;

// Loads the driver settings and returns the current global profile handle.
unsafe fn drs_global_profile(context: &NvapiContext, session: *mut c_void) -> Result<*mut c_void, String> {
    let load = nvapi_function::<NvapiDrsLoadSettings>(
        context,
        NVAPI_DRS_LOAD_SETTINGS_ID,
        "NvAPI_DRS_LoadSettings",
    )
    .ok_or_else(|| "NvAPI_DRS_LoadSettings is not available in this driver".to_string())?;
    if load(session) != NVAPI_OK {
        return Err("NvAPI_DRS_LoadSettings failed".into());
    }
    let get_global = nvapi_function::<NvapiDrsGetCurrentGlobalProfile>(
        context,
        NVAPI_DRS_GET_CURRENT_GLOBAL_PROFILE_ID,
        "NvAPI_DRS_GetCurrentGlobalProfile",
    )
    .ok_or_else(|| "NvAPI_DRS_GetCurrentGlobalProfile is not available in this driver".to_string())?;
    let mut profile = std::ptr::null_mut();
    if get_global(session, &mut profile) != NVAPI_OK || profile.is_null() {
        return Err("NvAPI_DRS_GetCurrentGlobalProfile failed".into());
    }
    Ok(profile)
}

// RTX HDR is polled every 500 ms while the Windows HDR tab is open. A fresh
// nvapi_init() plus a fresh DRS session per poll would load nvapi64.dll, run
// NvAPI_Initialize/EnumPhysicalGPUs, create a DRS session and re-read the
// entire driver settings tree, then destroy it all again (NvAPI_Unload + unload
// the DLL) twice a second — enough driver churn to stutter the UI. Cache the
// initialized NVAPI context AND the DRS session plus its global-profile handle
// so every poll is a single in-process NvAPI_DRS_GetSetting call. The context
// and session are rebuilt only after explicit invalidation, a context lifetime
// expiry, or a detected change to NVIDIA's DRS database file metadata. The
// cache is only ever touched while VENDOR_QUERY_LOCK is held, so sharing the
// context and session is safe.
struct CachedNvapiContext {
    // Boxed so the context's heap allocation (and thus a raw pointer to it)
    // is stable even while the MutexGuard is dropped. The box is only ever
    // replaced or removed while the cache lock is held, so a pointer taken
    // out of the guard remains valid for the process lifetime.
    context: Box<NvapiContext>,
    // Resolved once at init: the app-wide vendor refresh and the RTX HDR poll
    // reuse the display ID instead of re-enumerating it on every query. The
    // display ID stays valid as long as the GPU handle does, and both live as
    // long as the cached context does.
    display_id: Option<u32>,
    captured_at: Instant,
}

static RTX_HDR_NVAPI_CACHE: OnceLock<Mutex<Option<CachedNvapiContext>>> = OnceLock::new();
static NVAPI_SHUTDOWN_STARTED: AtomicBool = AtomicBool::new(false);
// This is a safety lifetime for the NVAPI context, not the RTX HDR setting
// refresh cadence. DRS metadata changes invalidate the setting session much
// sooner; display notifications invalidate both caches immediately.
const RTX_HDR_NVAPI_CACHE_TTL: Duration = Duration::from_secs(60);

// Returns a process-lifetime reference to the cached NVAPI context, so the
// 500 ms RTX HDR poll reuses the loaded DLL instead of re-initializing NVAPI
// (and unloading it again) on every read.
//
// SAFETY CONTRACT: the returned reference is only valid while the caller
// holds VENDOR_QUERY_LOCK — the cache box is replaced/dropped while that lock
// is held, so the heap allocation behind the reference can disappear the
// moment the lock is released. Both call sites (get/set_rtx_hdr_flags) hold
// the lock for the entire use; never store the reference or use it outside a
// VENDOR_QUERY_LOCK hold.
fn rtx_hdr_nvapi_context() -> Option<&'static NvapiContext> {
    let cache = RTX_HDR_NVAPI_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().unwrap_or_else(|error| error.into_inner());
    let fresh = guard
        .as_ref()
        .map(|cached| cached.captured_at.elapsed() < RTX_HDR_NVAPI_CACHE_TTL)
        .unwrap_or(false);
    if !fresh {
        // Re-init when the cached context is missing or stale. Drop the DRS
        // session first: its destroy fn-ptr belongs to the old context, so it
        // must be destroyed while that context is still loaded. The old
        // context then drops here (unloading NVAPI) while the lock is held,
        // so no other thread can be using it.
        if let Some(cache) = RTX_HDR_DRS_CACHE.get() {
            if let Ok(mut drs_guard) = cache.lock() {
                *drs_guard = None;
            }
        }
        if let Some(context) = nvapi_init() {
            let display_id = enumerate_active_display_id(&context);
            *guard = Some(CachedNvapiContext {
                context: Box::new(context),
                display_id,
                captured_at: Instant::now(),
            });
        } else if guard.is_none() {
            // nvapi_init failed and nothing is cached — report the failure
            // to the caller instead of hanging on to a stale context.
            return None;
        }
    }
    // The context lives in a stable heap allocation owned by the static
    // cache, so this reference outlives the guard. Safe because the box is
    // only dropped while the cache lock is held, and every caller holds that
    // lock for the whole use of the returned reference.
    guard
        .as_ref()
        .map(|cached| unsafe { &*(&*cached.context as *const NvapiContext) })
}

// Returns the cached NVAPI context plus its pre-resolved display ID (None if
// the ID could not be resolved). Unlike rtx_hdr_nvapi_context this does not
// initialise NVAPI on demand — the 5 s vendor refresh must not trigger a
// fresh init/unload cycle every tick; it reuses the cache and only re-inits
// when the cache is empty or stale.
fn rtx_hdr_nvapi_with_display_id() -> Option<(&'static NvapiContext, Option<u32>)> {
    let cache = RTX_HDR_NVAPI_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().unwrap_or_else(|error| error.into_inner());
    let fresh = guard
        .as_ref()
        .map(|cached| cached.captured_at.elapsed() < RTX_HDR_NVAPI_CACHE_TTL)
        .unwrap_or(false);
    if !fresh {
        // Same DRS-before-context ordering as rtx_hdr_nvapi_context: the DRS
        // session's destroy fn-ptr belongs to the old context.
        if let Some(cache) = RTX_HDR_DRS_CACHE.get() {
            if let Ok(mut drs_guard) = cache.lock() {
                *drs_guard = None;
            }
        }
        if let Some(context) = nvapi_init() {
            let display_id = enumerate_active_display_id(&context);
            *guard = Some(CachedNvapiContext {
                context: Box::new(context),
                display_id,
                captured_at: Instant::now(),
            });
        } else if guard.is_none() {
            return None;
        }
    }
    guard.as_ref().map(|cached| {
        let context = unsafe { &*(&*cached.context as *const NvapiContext) };
        (context, cached.display_id)
    })
}

// The DRS session and its global-profile handle are cached alongside the NVAPI
// context: creating a DRS session and calling NvAPI_DRS_LoadSettings rebuilds
// the driver's in-memory settings tree from disk — the expensive part of the
// old per-poll session lifecycle.
struct NvapiDrsSession {
    handle: *mut c_void,
    destroy: unsafe extern "system" fn(*mut c_void) -> i32,
}
// The session handle is only ever used while VENDOR_QUERY_LOCK is held (the
// same lock that guards the shared NVAPI context), so sharing it across
// threads through the static cache is safe.
unsafe impl Send for NvapiDrsSession {}
unsafe impl Sync for NvapiDrsSession {}
impl Drop for NvapiDrsSession {
    fn drop(&mut self) {
        unsafe {
            let status = (self.destroy)(self.handle);
            if status == NVAPI_OK {
                log_vendor_lifecycle("NvAPI_DRS_DestroySession completed successfully");
            } else {
                log_vendor_warn(&format!(
                    "NvAPI_DRS_DestroySession failed: {status}"
                ));
            }
        }
    }
}

struct CachedDrsSession {
    // Boxed so the entry's heap allocation is stable even while the
    // MutexGuard is dropped — the session + profile handles stay valid for
    // the process lifetime, only ever replaced while the cache lock is held.
    entry: Box<CachedDrsEntry>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct DrsFileStamp {
    length: u64,
    modified_ns: u128,
}

// NVIDIA writes the global driver profile to these files when Control Panel,
// Profile Inspector, or another DRS client changes a setting. Reading their
// metadata is cheap; parsing them with NvAPI_DRS_LoadSettings is not. The
// stamp lets the 500 ms status poll notice an external change without
// rebuilding the DRS session on a timer.
fn drs_database_stamp() -> [DrsFileStamp; 2] {
    let Some(program_data) = std::env::var_os("ProgramData") else {
        return [DrsFileStamp { length: 0, modified_ns: 0 }; 2];
    };
    let root = std::path::PathBuf::from(program_data)
        .join("NVIDIA Corporation")
        .join("Drs");
    ["nvdrsdb0.bin", "nvdrsdb1.bin"].map(|name| {
        let path = root.join(name);
        let Ok(metadata) = std::fs::metadata(path) else {
            return DrsFileStamp { length: 0, modified_ns: 0 };
        };
        let modified_ns = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        DrsFileStamp {
            length: metadata.len(),
            modified_ns,
        }
    })
}

struct CachedDrsEntry {
    session: NvapiDrsSession,
    profile: *mut c_void,
    // The resolved DRS function pointers are cached alongside the session so
    // the 500ms RTX HDR poll is a single direct driver call. Re-resolving via
    // nvapi_function() on every poll did an NvAPI_QueryInterface lookup AND
    // logged a line for each — per-tick churn that showed up in the runtime
    // log and IPC traffic.
    get_setting: NvapiDrsGetSetting,
    set_setting: NvapiDrsSetSetting,
    save_settings: NvapiDrsSaveSettings,
    database_stamp: [DrsFileStamp; 2],
}
// Like NvapiDrsSession, the cached profile handle + fn pointers are only ever
// used while VENDOR_QUERY_LOCK is held.
unsafe impl Send for CachedDrsEntry {}
unsafe impl Sync for CachedDrsEntry {}

static RTX_HDR_DRS_CACHE: OnceLock<Mutex<Option<CachedDrsSession>>> = OnceLock::new();

// Returns the cached DRS session + global profile, (re)creating them only when
// the session is absent or NVIDIA's DRS database metadata changed externally.
// Must be called while VENDOR_QUERY_LOCK is held. This avoids the old 5-second
// TTL rebuild, whose NvAPI_DRS_LoadSettings call caused the periodic hitch.
unsafe fn cached_drs_session(context: &NvapiContext) -> Option<&'static CachedDrsEntry> {
    let cache = RTX_HDR_DRS_CACHE.get_or_init(|| Mutex::new(None));
    let mut guard = cache.lock().unwrap_or_else(|error| error.into_inner());
    let current_stamp = drs_database_stamp();
    let fresh = guard
        .as_ref()
        .map(|cached| cached.entry.database_stamp == current_stamp)
        .unwrap_or(false);
    if !fresh {
        let create = nvapi_function::<NvapiDrsCreateSession>(
            context,
            NVAPI_DRS_CREATE_SESSION_ID,
            "NvAPI_DRS_CreateSession",
        )?;
        let mut session = std::ptr::null_mut();
        if create(&mut session) != NVAPI_OK || session.is_null() {
            return None;
        }
        let destroy = nvapi_function::<NvapiDrsDestroySession>(
            context,
            NVAPI_DRS_DESTROY_SESSION_ID,
            "NvAPI_DRS_DestroySession",
        )?;
        // LoadSettings re-reads the driver settings tree, then
        // GetCurrentGlobalProfile gives us the global profile handle the
        // app reads/writes. The profile handle is owned by the session and
        // stays valid as long as the session lives. Resolve the read/write
        // entry points once here; the poll reuses them directly.
        let profile = match drs_global_profile(context, session) {
            Ok(profile) => profile,
            Err(_) => {
                let status = destroy(session);
                if status == NVAPI_OK {
                    log_vendor_lifecycle("NvAPI_DRS_DestroySession init-cleanup completed successfully");
                } else {
                    log_vendor_warn(&format!(
                        "NvAPI_DRS_DestroySession init-cleanup failed: {status}"
                    ));
                }
                return None;
            }
        };
        let Some(get_setting) = nvapi_function::<NvapiDrsGetSetting>(
            context,
            NVAPI_DRS_GET_SETTING_ID,
            "NvAPI_DRS_GetSetting",
        ) else {
            let status = destroy(session);
            if status == NVAPI_OK {
                log_vendor("NvAPI_DRS_DestroySession init-cleanup completed successfully");
            } else {
                log_vendor_warn(&format!(
                    "NvAPI_DRS_DestroySession init-cleanup failed: {status}"
                ));
            }
            return None;
        };
        let Some(set_setting) = nvapi_function::<NvapiDrsSetSetting>(
            context,
            NVAPI_DRS_SET_SETTING_ID,
            "NvAPI_DRS_SetSetting",
        ) else {
            let status = destroy(session);
            if status == NVAPI_OK {
                log_vendor("NvAPI_DRS_DestroySession init-cleanup completed successfully");
            } else {
                log_vendor_warn(&format!(
                    "NvAPI_DRS_DestroySession init-cleanup failed: {status}"
                ));
            }
            return None;
        };
        let Some(save_settings) = nvapi_function::<NvapiDrsSaveSettings>(
            context,
            NVAPI_DRS_SAVE_SETTINGS_ID,
            "NvAPI_DRS_SaveSettings",
        ) else {
            let status = destroy(session);
            if status == NVAPI_OK {
                log_vendor("NvAPI_DRS_DestroySession init-cleanup completed successfully");
            } else {
                log_vendor_warn(&format!(
                    "NvAPI_DRS_DestroySession init-cleanup failed: {status}"
                ));
            }
            return None;
        };
        *guard = Some(CachedDrsSession {
            entry: Box::new(CachedDrsEntry {
                session: NvapiDrsSession { handle: session, destroy },
                profile,
                get_setting,
                set_setting,
                save_settings,
                database_stamp: current_stamp,
            }),
        });
    }
    guard.as_ref().map(|cached| {
        // The entry lives in a stable heap allocation owned by the static
        // cache, so this reference outlives the guard — same reasoning as
        // the context cache above.
        unsafe { &*(&*cached.entry as *const CachedDrsEntry) }
    })
}

unsafe fn with_cached_drs<T>(
    work: impl FnOnce(&CachedDrsEntry) -> Result<T, String>,
) -> Result<T, String> {
    let context = rtx_hdr_nvapi_context().ok_or_else(|| "NVAPI unavailable".to_string())?;
    let cached =
        unsafe { cached_drs_session(context) }.ok_or_else(|| "NvAPI_DRS session unavailable".to_string())?;
    work(cached)
}

/// Sets the global RTX HDR master switch in the driver profile.
pub fn enable_rtx_hdr_for_mode() -> Result<(), String> {
    // RTX HDR needs a non-zero Driver Flags value as well as the global
    // enable bit. Profile Inspector can leave the flags at zero after a
    // reset, which would make the UI say RTX HDR is active while the driver
    // applies no RTX HDR processing. Use NVIDIA's known performance default
    // only in that case; preserve an existing user-selected mode otherwise.
    let settings = get_rtx_hdr_settings()?;
    if settings.flags == 0 {
        set_rtx_hdr_flags(0x06)?;
    } else if !settings.enabled {
        set_rtx_hdr_enabled(true)?;
    }
    Ok(())
}

pub fn set_rtx_hdr_enabled(enabled: bool) -> Result<(), String> {
    let _query_guard = VENDOR_QUERY_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    unsafe {
        with_cached_drs(|cached| {
            let mut setting = NvDrsSetting::dword(RTX_HDR_ENABLE_ID, u32::from(enabled));
            let status = (cached.set_setting)(cached.session.handle, cached.profile, &mut setting, 0, 0);
            if status != NVAPI_OK {
                return Err(drs_write_error("NvAPI_DRS_SetSetting(Enable)", status));
            }
            if (cached.save_settings)(cached.session.handle) != NVAPI_OK {
                return Err("NvAPI_DRS_SaveSettings failed".into());
            }
            Ok(())
        })
    }
}

/// Sets the RTX HDR mode in the global driver profile. `flags` is the
/// "RTX HDR - Driver Flags" value (0 = off). Non-zero modes also enable the
/// master switch so selecting a mode remains a one-step operation.
pub fn set_rtx_hdr_flags(flags: u32) -> Result<(), String> {
    // Only the documented Driver Flags values exist.
    if !matches!(flags, 0x00 | 0x02 | 0x03 | 0x06 | 0x0A | 0x23) {
        return Err(format!("unknown RTX HDR mode 0x{flags:02X}"));
    }
    let _query_guard = VENDOR_QUERY_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    unsafe {
        with_cached_drs(|cached| {
            let mut flags_setting = NvDrsSetting::dword(RTX_HDR_DRIVER_FLAGS_ID, flags);
            let flags_status = (cached.set_setting)(cached.session.handle, cached.profile, &mut flags_setting, 0, 0);
            if flags_status != NVAPI_OK {
                return Err(drs_write_error("NvAPI_DRS_SetSetting(Driver Flags)", flags_status));
            }
            let mut enable_setting = NvDrsSetting::dword(
                RTX_HDR_ENABLE_ID,
                if flags == 0x00 { 0 } else { 1 },
            );
            let enable_status = (cached.set_setting)(cached.session.handle, cached.profile, &mut enable_setting, 0, 0);
            if enable_status != NVAPI_OK {
                return Err(drs_write_error("NvAPI_DRS_SetSetting(Enable)", enable_status));
            }
            if (cached.save_settings)(cached.session.handle) != NVAPI_OK {
                return Err("NvAPI_DRS_SaveSettings failed".into());
            }
            Ok(())
        })
    }
}

#[derive(serde::Serialize)]
pub struct RtxHdrSettings {
    pub enabled: bool,
    pub flags: u32,
}

// Reads both RTX HDR values under one vendor-query lock and one cached DRS
// session. The Windows HDR tab polls this every 500 ms; keeping the pair in a
// single command avoids two IPC calls and duplicate lock/context overhead.
pub fn get_rtx_hdr_settings() -> Result<RtxHdrSettings, String> {
    let _query_guard = VENDOR_QUERY_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    unsafe {
        with_cached_drs(|cached| {
            let read_dword = |setting_id| {
                let mut setting = NvDrsSetting::default();
                setting.version = NVDRS_SETTING_VER;
                let mut unknown = 0u32;
                let status = (cached.get_setting)(
                    cached.session.handle,
                    cached.profile,
                    setting_id,
                    &mut setting,
                    &mut unknown,
                );
                if status == NVAPI_OK {
                    Ok(setting.u32CurrentValue)
                } else if status == NVAPI_SETTING_NOT_FOUND {
                    Ok(0)
                } else {
                    Err(format!("NvAPI_DRS_GetSetting failed: {status}"))
                }
            };
            Ok(RtxHdrSettings {
                enabled: read_dword(RTX_HDR_ENABLE_ID)? != 0,
                flags: read_dword(RTX_HDR_DRIVER_FLAGS_ID)?,
            })
        })
    }
}

#[tauri::command]
pub fn get_rtx_hdr_settings_state() -> Result<RtxHdrSettings, String> {
    get_rtx_hdr_settings()
}

#[tauri::command]
pub fn set_rtx_hdr_state(flags: u32) -> Result<(), String> {
    set_rtx_hdr_flags(flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_ffi_layout_constants_are_sane() {
        assert_eq!(COLOR_DATA_DYNAMIC_RANGE_OFFSET, 10);
        assert_eq!(COLOR_DATA_V5_SIZE, 24);
        assert_eq!(MONITOR_CAPS_DATA_OFFSET, 17);
        assert_eq!(DISPLAY_IDS_SIZE, 16);
        assert_eq!(VRR_INFO_SIZE, 24);
        assert_eq!(ADAPTIVE_SYNC_DATA_SIZE, 40);

        // The DRS setting struct must stay at the driver-expected size so
        // the version constant (which embeds it) is accepted. 12320 matches
        // Profile Inspector's Marshal.SizeOf<NVDRS_SETTING>() — a wrong size
        // makes the driver return NVAPI_INCOMPATIBLE_STRUCT_VERSION (-9).
        assert_eq!(std::mem::size_of::<NvDrsSetting>(), 12320);
        assert_eq!(NVDRS_SETTING_VER, 0x00013020);

        // The value slots must sit exactly where the C unions do, or
        // Get/SetSetting would read/write the wrong member while the total
        // size (and thus the version constant) stays valid.
        assert_eq!(std::mem::offset_of!(NvDrsSetting, u32PredefinedValue), 4120);
        assert_eq!(std::mem::offset_of!(NvDrsSetting, u32CurrentValue), 8220);

        // These layouts are part of the public Intel C ABI. Keep the checks
        // here so a future edit to the hand-written bindings cannot silently
        // shift a field and make the driver write into the wrong location.
        assert_eq!(std::mem::size_of::<CtlInitArgs>(), 36);
        assert_eq!(std::mem::size_of::<CtlDisplaySettings>(), 148);
        assert_eq!(std::mem::size_of::<CtlArcMonitorParams>(), 24);
        assert_eq!(std::mem::size_of::<CtlArcProfileParams>(), 28);
    }
}
