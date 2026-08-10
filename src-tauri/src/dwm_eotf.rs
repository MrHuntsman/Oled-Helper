//! Read-only DWM EOTF compatibility probing.
//!
//! This module deliberately stops well before the dangerous part of
//! dwm_eotf: it does not suspend or terminate DWM, write process memory,
//! patch DXBC, or restart the compositor. It only finds dwm.exe/dwmcore.dll,
//! opens DWM with the least useful read access, and scans bounded readable
//! regions for the known sRGB transfer-function constants.
//!
//! An opt-in SeDebugPrivilege path can be requested to get past the
//! protected-process module-snapshot barrier; it still does not modify DWM.

use serde::Serialize;

#[derive(Clone, Serialize)]
pub struct DwmShaderDigestInfo {
    pub digest: String,
    pub file_size: u32,
    pub whitelisted: bool,
}

#[derive(Clone, Serialize)]
pub struct DwmShaderDigestDump {
    pub status: String,
    pub message: String,
    pub dwm_pid: u32,
    pub total: u32,
    pub whitelisted_hits: u32,
    pub digests: Vec<DwmShaderDigestInfo>,
}

#[derive(Clone, Serialize)]
pub struct DwmEotfScanResult {
    pub status: String,
    pub message: String,
    pub dwm_pid: u32,
    pub dwmcore_base: u64,
    pub dwmcore_size: u64,
    pub readable_regions: u32,
    pub scanned_bytes: u64,
    pub constant_hits: u32,
    pub constants_found: Vec<String>,
    pub diagnostic_code: u32,
    pub debug_privilege_used: bool,
    pub dxbc_containers: u32,
    pub whitelist_hits: u32,
    pub whitelist_matched: Vec<String>,
}

fn result(status: &str, message: impl Into<String>) -> DwmEotfScanResult {
    DwmEotfScanResult {
        status: status.to_string(),
        message: message.into(),
        dwm_pid: 0,
        dwmcore_base: 0,
        dwmcore_size: 0,
        readable_regions: 0,
        scanned_bytes: 0,
        constant_hits: 0,
        constants_found: Vec::new(),
        diagnostic_code: 0,
        debug_privilege_used: false,
        dxbc_containers: 0,
        whitelist_hits: 0,
        whitelist_matched: Vec::new(),
    }
}

#[cfg(not(target_os = "windows"))]
pub fn scan() -> DwmEotfScanResult {
    result("unsupported", "The read-only DWM probe is available on Windows only.")
}

#[cfg(not(target_os = "windows"))]
pub fn scan_with_debug_privilege() -> DwmEotfScanResult {
    result("unsupported", "The read-only DWM probe is available on Windows only.")
}

#[cfg(not(target_os = "windows"))]
pub fn scan_dwm_shaders() -> DwmEotfScanResult {
    result("unsupported", "The read-only DWM probe is available on Windows only.")
}

#[cfg(not(target_os = "windows"))]
pub fn scan_dwm_dry_run(_profile: &str) -> DwmEotfScanResult {
    result("unsupported", "The read-only DWM probe is available on Windows only.")
}

#[cfg(not(target_os = "windows"))]
pub fn apply_dwm_patch_v2(_profile: &str) -> DwmEotfScanResult {
    result("unsupported", "DWM patching is available on Windows only.")
}

#[cfg(not(target_os = "windows"))]
pub fn current_dwm_pid() -> Option<u32> {
    None
}

#[cfg(not(target_os = "windows"))]
pub fn dump_shader_digests() -> DwmShaderDigestDump {
    DwmShaderDigestDump {
        status: "unsupported".into(),
        message: "DWM shader digest dump is available on Windows only.".into(),
        dwm_pid: 0,
        total: 0,
        whitelisted_hits: 0,
        digests: Vec::new(),
    }
}

#[cfg(target_os = "windows")]
mod windows_scan {
    use super::*;
    use std::ffi::c_void;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
    use windows::Win32::System::Diagnostics::Debug::{
        ReadProcessMemory, WriteProcessMemory,
    };
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, Process32FirstW,
        Process32NextW, MODULEENTRY32W, PROCESSENTRY32W, TH32CS_SNAPMODULE,
        TH32CS_SNAPMODULE32, TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Memory::{
        VirtualProtectEx, VirtualQueryEx, MEMORY_BASIC_INFORMATION, MEM_COMMIT,
        PAGE_GUARD, PAGE_NOACCESS, PAGE_PROTECTION_FLAGS, PAGE_READWRITE,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, TerminateProcess,
        PROCESS_ALL_ACCESS, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
        PROCESS_VM_READ,
    };
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, SE_DEBUG_NAME, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::core::PCWSTR;

    const MAX_SCAN_BYTES: usize = 64 * 1024 * 1024;
    const MAX_READ_CHUNK: usize = 1024 * 1024;
    const PROCESS_NAME: &str = "dwm.exe";
    const MODULE_NAME: &str = "dwmcore.dll";

    // ── Known DWM shader whitelist from dwm_eotf_rs ─────────────────
    static DWM_EOTF_WHITELIST: [u128; 4] = [
        0x85d254ecdbd4d71dcdec559258d1e696,
        0x3caf9cdde6b655e3ddfba2c137b02621,
        0xdbadc38d66727c965df029e2ff26892c,
        0xf5a79888be546336d9b324afbbbf93f6,
    ];

    const DXBC_MAGIC: [u8; 4] = *b"DXBC";
    const DX_HEADER_SIZE: usize = 32;

    const KNOWN_CONSTANTS: [(&str, f32); 4] = [
        ("2.4", 2.4),
        ("0.04045", 0.04045),
        ("0.055", 0.055),
        ("0.94786733", 0.94786733),
    ];

    const DXBC_VEC3_PATTERNS: [(&str, [[f32; 4]; 1]); 4] = [
        ("2.4 (vec3)", [[2.4_f32, 2.4, 2.4, 0.0]]),
        ("0.04045 (vec3)", [[0.04045_f32, 0.04045, 0.04045, 0.0]]),
        ("0.055 (vec3)", [[0.055_f32, 0.055, 0.055, 0.0]]),
        ("0.94786733 (vec3)", [[0.94786733_f32, 0.94786733, 0.94786733, 0.0]]),
    ];

    // ── Gamma EOTF profiles ───────────────────────────────────────────
    // Each profile defines its target (gamma, threshold, slope, norm) and
    // the set of (old_val, new_val) replacement pairs that cover every
    // possible current state → this profile's target. Order matters for the
    // sRGB restore: the slope replacement must process before threshold
    // because pure-gamma shaders use [0,0,0,0] for both positions.

    pub const GAMMA_PROFILES: [(&str, &str); 4] = [
        ("srgb", "sRGB piecewise"),
        ("2.2", "Gamma 2.2"),
        ("2.4", "Gamma 2.4"),
        ("2.6", "Gamma 2.6"),
    ];

    /// Returns the (old, new) replacement pairs for a gamma profile.
    /// Each pair is a full vec3: [old, old, old, 0.0] → [new, new, new, 0.0].
    /// The patcher scans the shader for each old pattern in order and, if
    /// found and different from new, replaces it. Pairs that don't match
    /// the current shader state are harmless no-ops.
    fn gamma_replacements(profile: &str) -> Option<Vec<(f32, f32)>> {
        match profile {
            // Restore to original sRGB piecewise. Process slope (0.0→0.055)
            // BEFORE threshold (0.0→0.04045) so the two identical [0,0,0,0]
            // patterns in a pure-gamma shader are replaced correctly.
            "srgb" => Some(vec![
                (2.2, 2.4),       // gamma: any pure → 2.4
                (2.6, 2.4),
                (0.0, 0.055),     // slope: 0.0 → 0.055 (FIRST, before threshold)
                (0.0, 0.04045),   // threshold: remaining 0.0 → 0.04045
                (1.0, 0.94786733), // norm: 1.0 → 0.94786733
            ]),
            "2.2" => Some(vec![
                (2.4, 2.2),       // gamma: 2.4/2.6 → 2.2
                (2.6, 2.2),
                (0.04045, 0.0),   // threshold → 0.0
                (0.055, 0.0),     // slope → 0.0
                (0.94786733, 1.0), // norm → 1.0
            ]),
            "2.4" => Some(vec![
                (2.2, 2.4),       // gamma: 2.2/2.6 → 2.4
                (2.6, 2.4),
                (0.04045, 0.0),   // threshold → 0.0
                (0.055, 0.0),     // slope → 0.0
                (0.94786733, 1.0), // norm → 1.0
            ]),
            "2.6" => Some(vec![
                (2.2, 2.6),       // gamma: 2.2/2.4 → 2.6
                (2.4, 2.6),
                (0.04045, 0.0),   // threshold → 0.0
                (0.055, 0.0),     // slope → 0.0
                (0.94786733, 1.0), // norm → 1.0
            ]),
            _ => None,
        }
    }

    struct Handle(HANDLE);

    impl Handle {
        fn new(handle: HANDLE) -> Option<Self> {
            if handle.is_invalid() { None } else { Some(Self(handle)) }
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { let _ = CloseHandle(self.0); }
        }
    }

    fn wide_string(value: &[u16]) -> String {
        let end = value.iter().position(|ch| *ch == 0).unwrap_or(value.len());
        String::from_utf16_lossy(&value[..end])
    }

    fn find_dwm_pid() -> Option<u32> {
        unsafe {
            let snapshot = Handle::new(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?)?;
            let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            if Process32FirstW(snapshot.0, &mut entry).is_err() { return None; }
            loop {
                if wide_string(&entry.szExeFile).eq_ignore_ascii_case(PROCESS_NAME) { return Some(entry.th32ProcessID); }
                if Process32NextW(snapshot.0, &mut entry).is_err() { break; }
            }
        }
        None
    }

    fn win32_error_code(hresult: i32) -> u32 {
        let raw = hresult as u32;
        if raw & 0xFFFF_0000 == 0x8007_0000 { raw & 0xFFFF } else { raw }
    }

    /// Kills dwm.exe and waits for Windows to spawn a fresh instance, then
    /// returns the new PID. This mirrors dwm_eotf_rs' open_restarted flow:
    /// patching must happen against the restarted compositor before its GPU
    /// pipeline compiles the shaders, otherwise the in-memory write is never
    /// picked up by rendering (no flash, no visible change).
    fn restart_dwm() -> Result<u32, String> {
        let old_pid = find_dwm_pid().ok_or_else(|| "dwm.exe is not running.".to_string())?;
        let Ok(process) = (unsafe { OpenProcess(PROCESS_TERMINATE, false, old_pid) }) else {
            return Err(format!("Cannot open dwm.exe (PID {old_pid}) to terminate it."));
        };
        let Some(process) = Handle::new(process) else {
            return Err("Invalid dwm.exe process handle.".into());
        };
        log::info!("[dwm-eotf] restarting DWM (old PID {old_pid})...");
        let terminated = unsafe { TerminateProcess(process.0, 0) };
        if let Err(error) = terminated {
            return Err(format!("TerminateProcess failed: {error}"));
        }
        drop(process);

        // Windows restarts DWM automatically. Wait for a NEW PID (up to 15 s)
        // before continuing, exactly like wait_pid_by_name in dwm_eotf_rs.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if let Some(pid) = find_dwm_pid() {
                if pid != old_pid {
                    log::info!("[dwm-eotf] DWM restarted with new PID {pid}");
                    return Ok(pid);
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err("DWM did not restart within 15 seconds.".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    fn enable_debug_privilege() -> Result<bool, String> {
        unsafe {
            let mut token = HANDLE::default();
            let Ok(()) = OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut token,
            ) else {
                return Err("OpenProcessToken failed".into());
            };
            let _token_guard = Handle::new(token).ok_or("OpenProcessToken returned an invalid handle")?;
            let mut luid = LUID::default();
            LookupPrivilegeValueW(PCWSTR::null(), SE_DEBUG_NAME, &mut luid)
                .map_err(|e| format!("LookupPrivilegeValueW failed: {e}"))?;
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [windows::Win32::Security::LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            AdjustTokenPrivileges(
                token,
                false,
                Some(&tp),
                std::mem::size_of::<TOKEN_PRIVILEGES>() as u32,
                None,
                None,
            )
            .map_err(|e| format!("AdjustTokenPrivileges failed: {e}"))?;
        }
        Ok(true)
    }

    fn find_dwmcore(pid: u32) -> Result<Option<(usize, usize)>, u32> {
        unsafe {
            let flags = TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32;
            let snapshot = match CreateToolhelp32Snapshot(flags, pid) {
                Ok(handle) => Handle::new(handle).ok_or(6u32)?,
                Err(error) => return Err(win32_error_code(error.code().0)),
            };
            let mut entry = MODULEENTRY32W { dwSize: std::mem::size_of::<MODULEENTRY32W>() as u32, ..Default::default() };
            if let Err(error) = Module32FirstW(snapshot.0, &mut entry) {
                let code = win32_error_code(error.code().0);
                return if code == 18 { Ok(None) } else { Err(code) };
            }
            loop {
                if wide_string(&entry.szModule).eq_ignore_ascii_case(MODULE_NAME) {
                    return Ok(Some((entry.modBaseAddr as usize, entry.modBaseSize as usize)));
                }
                if let Err(error) = Module32NextW(snapshot.0, &mut entry) {
                    let code = error.code().0 as u32;
                    return if code == 18 { Ok(None) } else { Err(code) };
                }
            }
        }
    }

    fn module_access_message(code: u32) -> String {
        match code {
            5 => "Access denied (Win32 error 5). Windows blocked the read-only module snapshot for protected DWM; no memory was read or changed.".to_string(),
            299 => "Partial copy (Win32 error 299). Windows could not copy the 64-bit DWM module list into this process; no memory was read or changed.".to_string(),
            _ => format!("Windows could not enumerate DWM modules (Win32 error {code}). No memory was read or changed."),
        }
    }

    fn readable_committed(protect: u32) -> bool {
        let base = protect & 0xff;
        matches!(base, 0x02 | 0x04 | 0x08 | 0x10 | 0x20 | 0x40 | 0x80)
            && protect & (PAGE_GUARD.0 | PAGE_NOACCESS.0) == 0
    }

    fn find_pattern(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|window| window == needle)
    }

    /// byte-swaps a [u8;16] into a u128 for comparison against the whitelist.
    fn digest_u128(bytes: &[u8; 16]) -> u128 {
        let mut value = 0u128;
        for &byte in bytes.iter().rev() {
            value = (value << 8) | (byte as u128);
        }
        value
    }

    /// Reads `len` bytes from a DWM process at `address`, retrying across
    /// page boundaries. Returns the bytes actually read.
    fn read_dwm_bytes(process: &Handle, address: usize, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        let mut total = 0usize;
        while total < len {
            let mut chunk_read = 0usize;
            let chunk_addr = address.saturating_add(total);
            let chunk_len = (len - total).min(MAX_READ_CHUNK);
            if unsafe {
                ReadProcessMemory(
                    process.0,
                    chunk_addr as *const c_void,
                    out[total..].as_mut_ptr() as *mut c_void,
                    chunk_len,
                    Some(&mut chunk_read),
                )
            }
            .is_err()
                || chunk_read == 0
            {
                break;
            }
            total = total.saturating_add(chunk_read);
        }
        out.truncate(total);
        out
    }

    /// Phase 3a dry run: walks DWM virtual memory directly, finds whitelisted
    /// DXBC shaders at their real addresses, reads the full container, and
    /// simulates patching in a copy ,  never touches DWM.
    fn dry_run_patch(process: &Handle, base: usize, size: usize, profile: &str) -> (u32, u32, Vec<String>) {
        if gamma_replacements(profile).is_none() {
            return (0, 0, vec!["unknown gamma profile".into()]);
        }

        let mut shaders_found = 0u32;
        let mut would_patch = 0u32;
        let mut details = Vec::new();
        let mut covered_ranges: Vec<(usize, usize)> = Vec::new();
        let mut header_hits = 0u32;
        let mut captured_regions = 0u32;
        let mut captured_bytes = 0usize;
        let mut total_regions = 0u32;
        let mut unreadable_committed = 0u32;
        let mut protect_hist: std::collections::BTreeMap<u32, u32> = Default::default();
        let mut address = base;
        let end = base.saturating_add(size);

        while address < end {
            let mut info = MEMORY_BASIC_INFORMATION::default();
            let queried = unsafe {
                VirtualQueryEx(
                    process.0,
                    Some(address as *const c_void),
                    &mut info,
                    std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };
            if queried == 0 || info.RegionSize == 0 {
                break;
            }
            let region_start = info.BaseAddress as usize;
            let region_end = region_start.saturating_add(info.RegionSize);
            let clipped_start = region_start.max(base);
            let clipped_end = region_end.min(end);
            total_regions = total_regions.saturating_add(1);
            if info.State.0 == MEM_COMMIT.0 {
                *protect_hist.entry(info.Protect.0 & 0xff).or_insert(0) += 1;
                if !readable_committed(info.Protect.0) {
                    unreadable_committed = unreadable_committed.saturating_add(1);
                }
            }

            if info.State.0 == MEM_COMMIT.0
                && readable_committed(info.Protect.0)
                && clipped_start < clipped_end
            {
                let region_data = read_dwm_bytes(process, clipped_start, clipped_end - clipped_start);
                captured_regions = captured_regions.saturating_add(1);
                captured_bytes = captured_bytes.saturating_add(region_data.len());
                let mut i = 0usize;
                while i + DX_HEADER_SIZE <= region_data.len() {
                    if region_data[i..].starts_with(&DXBC_MAGIC) {
                        header_hits = header_hits.saturating_add(1);
                        let hash = digest_u128(
                            region_data[i + 4..i + 20]
                                .try_into()
                                .unwrap_or(&[0u8; 16]),
                        );
                        let file_size = u32::from_le_bytes(
                            region_data[i + 24..i + 28]
                                .try_into()
                                .unwrap_or([0u8; 4]),
                        ) as usize;
                        // Do not prefilter by the original digest here.
                        // After a gamma is applied, each shader has a repaired
                        // digest, so restore/switch dry runs must still pass
                        // the candidate through patch_recursive. That helper
                        // accepts only checksum-valid originals or containers
                        // that normalize back to the trusted whitelist.
                        let shader_va = clipped_start.saturating_add(i);
                        if covered_ranges.iter().any(|(start, end)| {
                            shader_va >= *start && shader_va < *end
                        }) {
                            i = i.saturating_add(DX_HEADER_SIZE);
                            continue;
                        }
                        let region_end_pos = (i + file_size).min(region_data.len());
                        let mut shader = if region_end_pos >= i + DX_HEADER_SIZE {
                            region_data[i..region_end_pos].to_vec()
                        } else {
                            Vec::new()
                        };
                        if shader.len() < file_size {
                            let fallback = read_dwm_bytes(process, shader_va, file_size);
                            if fallback.len() > shader.len() {
                                shader = fallback;
                            }
                        }

                        if shader.len() < file_size {
                            details.push(format!(
                                "{:032x} (body unreadable: VA=0x{shader_va:x} file={file_size} got={})",
                                hash,
                                shader.len(),
                            ));
                        } else {
                            // Use the exact same recursive, checksum-aware
                            // patch engine as the write path, but only on
                            // this private copy. This makes dry-run a true
                            // predictor of whether apply can patch the shader.
                            let (found, patched, patch_details) =
                                patch_recursive(&mut shader, profile);
                            log::debug!("[dwm-eotf] dry-run: VA=0x{shader_va:x} hash={hash:032x} file={file_size} found={found} patched={patched}");
                            if found > 0 {
                                shaders_found = shaders_found.saturating_add(found);
                                covered_ranges.push((
                                    shader_va,
                                    shader_va.saturating_add(file_size),
                                ));
                            }
                            if patched > 0 {
                                would_patch = would_patch.saturating_add(patched);
                                details.extend(patch_details);
                            } else if found > 0 {
                                let profile_label = GAMMA_PROFILES
                                    .iter()
                                    .find(|(key, _)| *key == profile)
                                    .map(|(_, label)| *label)
                                    .unwrap_or(profile);
                                details.push(format!(
                                    "{:032x} (already at {profile_label})",
                                    hash,
                                ));
                            }
                        }
                        // The header is enough to advance past this candidate;
                        // nested DXBC containers are handled by patch_recursive.
                        i = i.saturating_add(DX_HEADER_SIZE);
                    } else {
                        i = i.saturating_add(1);
                    }
                }
            }

            let next = region_end.max(address.saturating_add(0x1000));
            if next <= address {
                break;
            }
            address = next;
        }

        log::info!("[dwm-eotf] dry-run: total_regions={total_regions} captured={captured_regions} bytes={captured_bytes} unreadable_committed={unreadable_committed} header_hits={header_hits} shaders_found={shaders_found} would_patch={would_patch}");
        if !protect_hist.is_empty() {
            let hist: Vec<String> = protect_hist
                .iter()
                .map(|(protect, count)| format!("0x{protect:02X}x{count}"))
                .collect();
            log::info!("[dwm-eotf] dry-run: committed protection histogram: {}", hist.join(" "));
        }
        // Remember the capture stats for the result struct (the result fields
        // are populated in scan_dwm_dry_run after dry_run_patch returns).
        LAST_DRY_RUN_CAPTURE.with(|cell| {
            let mut guard = cell.borrow_mut();
            guard.0 = captured_regions;
            guard.1 = captured_bytes;
        });
        (shaders_found, would_patch, details)
    }

    // True capture stats from the last dry_run_patch invocation, surfaced in
    // the scan result so the UI shows what the scan actually walked.
    thread_local! {
        static LAST_DRY_RUN_CAPTURE: std::cell::RefCell<(u32, usize)> =
            const { std::cell::RefCell::new((0, 0)) };
    }

    fn dry_run_capture_stats() -> (u32, usize) {
        LAST_DRY_RUN_CAPTURE.with(|cell| *cell.borrow())
    }

    /// Reads all readable committed pages from dwmcore.dll into a single
    /// buffer, then parses DXBC containers and checks their digests against
    /// the known dwm_eotf_rs whitelist. Still read-only.
    fn scan_shaders(pid: u32, base: usize, size: usize) -> (u32, u32, Vec<String>) {
        let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ;
        let Ok(process) = (unsafe { OpenProcess(access, false, pid) }) else {
            return (0, 0, Vec::new());
        };
        let Some(process) = Handle::new(process) else {
            return (0, 0, Vec::new());
        };

        let mut memory = Vec::new();
        let mut address = base;
        let end = base.saturating_add(size);
        while address < end && memory.len() < MAX_SCAN_BYTES {
            let mut info = MEMORY_BASIC_INFORMATION::default();
            let queried = unsafe {
                VirtualQueryEx(process.0, Some(address as *const c_void), &mut info, std::mem::size_of::<MEMORY_BASIC_INFORMATION>())
            };
            if queried == 0 || info.RegionSize == 0 { break; }
            let region_start = info.BaseAddress as usize;
            let region_end = region_start.saturating_add(info.RegionSize);
            let clipped_start = region_start.max(base);
            let clipped_end = region_end.min(end);
            if info.State.0 == MEM_COMMIT.0 && readable_committed(info.Protect.0) && clipped_start < clipped_end {
                let length = (clipped_end - clipped_start).min(MAX_SCAN_BYTES - memory.len());
                let mut buffer = vec![0u8; length];
                let mut bytes_read = 0usize;
                let read_ok = unsafe {
                    ReadProcessMemory(process.0, clipped_start as *const c_void, buffer.as_mut_ptr() as *mut c_void, length, Some(&mut bytes_read))
                };
                if read_ok.is_ok() && bytes_read > 0 {
                    memory.extend_from_slice(&buffer[..bytes_read.min(buffer.len())]);
                }
            }
            let next = region_end.max(address.saturating_add(0x1000));
            if next <= address { break; }
            address = next;
        }

        let mut containers = 0u32;
        let mut hits = 0u32;
        let mut matched = Vec::new();
        let mut i = 0usize;
        while i + DX_HEADER_SIZE <= memory.len() {
            if memory[i..].starts_with(&DXBC_MAGIC) {
                containers = containers.saturating_add(1);
                let digest = &memory[i + 4..i + 20];
                let digest_bytes: &[u8; 16] = digest.try_into().unwrap_or(&[0u8; 16]);
                let hash = digest_u128(digest_bytes);
                let hex = format!("{:032x}", hash);
                if DWM_EOTF_WHITELIST.contains(&hash) {
                    hits = hits.saturating_add(1);
                    matched.push(hex);
                }
                let file_size_bytes = &memory[i + 24..i + 28];
                let _file_size = u32::from_le_bytes(file_size_bytes.try_into().unwrap_or([0u8; 4])) as usize;
                i = i.saturating_add(DX_HEADER_SIZE);
            } else {
                i = i.saturating_add(1);
            }
        }

        (containers, hits, matched)
    }

    pub fn scan() -> DwmEotfScanResult {
        scan_impl(false)
    }

    pub fn scan_with_debug_privilege() -> DwmEotfScanResult {
        scan_impl(true)
    }

    pub fn scan_dwm_shaders() -> DwmEotfScanResult {
        scan_impl(true)
    }

    /// Cheap PID lookup for the auto-reapply watcher: no module enumeration,
    /// no process open ,  just a Toolhelp process snapshot.
    pub fn current_dwm_pid() -> Option<u32> {
        find_dwm_pid()
    }

    /// Phase: whitelist growth. Walks the same readable dwmcore regions as
    /// the dry run, but reports EVERY DXBC container digest it finds (not
    /// just the four known ones), with its file size and whether it already
    /// matches the whitelist. This is how an unknown Windows build's shader
    /// digests get captured so the whitelist can be extended. Read-only.
    pub fn dump_shader_digests() -> DwmShaderDigestDump {
        let fail = |status: &str, message: String| DwmShaderDigestDump {
            status: status.to_string(),
            message,
            dwm_pid: 0,
            total: 0,
            whitelisted_hits: 0,
            digests: Vec::new(),
        };
        if let Err(err) = enable_debug_privilege() {
            return fail("privilege-failed", format!("Could not enable SeDebugPrivilege: {err}"));
        }
        let Some(pid) = find_dwm_pid() else {
            return fail("not-found", "Desktop Window Manager (dwm.exe) is not running.".into());
        };
        let (base, size) = match find_dwmcore(pid) {
            Ok(Some(module)) => module,
            Ok(None) => return fail("module-not-found", "dwmcore.dll was not listed in DWM's module snapshot.".into()),
            Err(code) => return fail("module-access", module_access_message(code)),
        };
        let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ;
        let Ok(process) = (unsafe { OpenProcess(access, false, pid) }) else {
            return fail("access-denied", "Windows did not allow a read-only DWM process handle.".into());
        };
        let Some(process) = Handle::new(process) else {
            return fail("open-failed", "Windows returned an invalid read-only DWM process handle.".into());
        };

        // Same region walk as the dry run: enumerate committed readable pages
        // inside dwmcore, read each fully, and scan for DXBC headers. Collect
        // every digest (deduplicated) with its container size.
        let mut digests: Vec<DwmShaderDigestInfo> = Vec::new();
        let mut seen: std::collections::HashSet<u128> = std::collections::HashSet::new();
        let mut address = base;
        let end = base.saturating_add(size);
        while address < end {
            let mut info = MEMORY_BASIC_INFORMATION::default();
            let queried = unsafe {
                VirtualQueryEx(process.0, Some(address as *const c_void), &mut info, std::mem::size_of::<MEMORY_BASIC_INFORMATION>())
            };
            if queried == 0 || info.RegionSize == 0 { break; }
            let region_start = info.BaseAddress as usize;
            let region_end = region_start.saturating_add(info.RegionSize);
            let clipped_start = region_start.max(base);
            let clipped_end = region_end.min(end);
            if info.State.0 == MEM_COMMIT.0 && readable_committed(info.Protect.0) && clipped_start < clipped_end {
                let region_data = read_dwm_bytes(&process, clipped_start, clipped_end - clipped_start);
                let mut i = 0usize;
                while i + DX_HEADER_SIZE <= region_data.len() {
                    if region_data[i..].starts_with(&DXBC_MAGIC) {
                        let hash = digest_u128(region_data[i + 4..i + 20].try_into().unwrap_or(&[0u8; 16]));
                        let file_size = u32::from_le_bytes(region_data[i + 24..i + 28].try_into().unwrap_or([0u8; 4]));
                        if seen.insert(hash) {
                            digests.push(DwmShaderDigestInfo {
                                digest: format!("{hash:032x}"),
                                file_size,
                                whitelisted: DWM_EOTF_WHITELIST.contains(&hash),
                            });
                        }
                        i = i.saturating_add(DX_HEADER_SIZE);
                    } else {
                        i = i.saturating_add(1);
                    }
                }
            }
            let next = region_end.max(address.saturating_add(0x1000));
            if next <= address { break; }
            address = next;
        }
        digests.sort_by(|a, b| b.whitelisted.cmp(&a.whitelisted).then_with(|| a.digest.cmp(&b.digest)));
        let whitelisted_hits = digests.iter().filter(|entry| entry.whitelisted).count() as u32;
        DwmShaderDigestDump {
            status: if digests.is_empty() { "no-shaders".into() } else { "ok".into() },
            message: if digests.is_empty() {
                "No DXBC containers found in dwmcore's readable pages.".into()
            } else {
                format!("Found {} unique DXBC container digests ({} already whitelisted).", digests.len(), whitelisted_hits)
            },
            dwm_pid: pid,
            total: digests.len() as u32,
            whitelisted_hits,
            digests,
        }
    }

    /// Phase 3a dry run: finds whitelisted shaders in DWM memory, builds
    /// replacement constants in a copy, and reports what WOULD change , 
    /// never writes to DWM, never suspends it.
    pub fn scan_dwm_dry_run(profile: &str) -> DwmEotfScanResult {
        if let Err(err) = enable_debug_privilege() {
            let mut out = result("privilege-failed", format!("Could not enable SeDebugPrivilege: {err}"));
            out.diagnostic_code = 1;
            return out;
        }
        let Some(pid) = find_dwm_pid() else { return result("not-found", "Desktop Window Manager (dwm.exe) is not running."); };
        let (base, size) = match find_dwmcore(pid) {
            Ok(Some(module)) => module,
            Ok(None) => {
                let mut out = result("module-not-found", "dwmcore.dll was not listed in DWM's module snapshot.");
                out.dwm_pid = pid;
                return out;
            }
            Err(code) => {
                let mut out = result("module-access", module_access_message(code));
                out.dwm_pid = pid;
                out.diagnostic_code = code;
                return out;
            }
        };
        let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ;
        let Ok(process) = (unsafe { OpenProcess(access, false, pid) }) else {
            return result("access-denied", "Windows did not allow a read-only DWM process handle.");
        };
        let Some(process) = Handle::new(process) else {
            return result("open-failed", "Windows returned an invalid read-only DWM process handle.");
        };

        let (shaders_found, would_patch, details) = dry_run_patch(&process, base, size, profile);

        // surface the true capture stats in the result so the UI reflects what
        // the scan actually walked (the legacy fields are otherwise always 0)
        let (cap_regions, cap_bytes) = dry_run_capture_stats();

        let msg = if would_patch == shaders_found && shaders_found > 0 {
            format!("Dry run complete: {would_patch}/{shaders_found} whitelisted shaders would be patched. No DWM memory was modified.")
        } else if would_patch > 0 {
            format!("Dry run: {would_patch}/{shaders_found} whitelisted shaders would be patched. {} were already in the expected layout.", shaders_found.saturating_sub(would_patch))
        } else {
            "Dry run: no whitelisted shaders needed patching.".to_string()
        };

        DwmEotfScanResult {
            status: if would_patch > 0 { "dry-run-ready".into() } else { "dry-run-skipped".into() },
            message: msg,
            dwm_pid: pid,
            dwmcore_base: base as u64,
            dwmcore_size: size as u64,
            readable_regions: cap_regions,
            scanned_bytes: cap_bytes as u64,
            constant_hits: 0,
            constants_found: Vec::new(),
            diagnostic_code: 0,
            debug_privilege_used: true,
            dxbc_containers: shaders_found,
            whitelist_hits: would_patch,
            whitelist_matched: details,
        }
    }

    // ── DXBC checksum + recursive patch helpers ──────────────────────
    // DXBC stores an MD5-compatible digest at offset 0x04. Microsoft's
    // CalculateDXBCChecksum hashes the container after offset 0x14 (magic
    // plus stored digest), using the DXBC-specific final block layout.
    fn md5_transform(state: &mut [u32; 4], block: &[u8; 64]) {
        const S: [[u32; 4]; 4] = [
            [7, 12, 17, 22], [5, 9, 14, 20], [4, 11, 16, 23], [6, 10, 15, 21],
        ];
        const K: [u32; 64] = [
            0xd76aa478,0xe8c7b756,0x242070db,0xc1bdceee,0xf57c0faf,0x4787c62a,0xa8304613,0xfd469501,
            0x698098d8,0x8b44f7af,0xffff5bb1,0x895cd7be,0x6b901122,0xfd987193,0xa679438e,0x49b40821,
            0xf61e2562,0xc040b340,0x265e5a51,0xe9b6c7aa,0xd62f105d,0x02441453,0xd8a1e681,0xe7d3fbc8,
            0x21e1cde6,0xc33707d6,0xf4d50d87,0x455a14ed,0xa9e3e905,0xfcefa3f8,0x676f02d9,0x8d2a4c8a,
            0xfffa3942,0x8771f681,0x6d9d6122,0xfde5380c,0xa4beea44,0x4bdecfa9,0xf6bb4b60,0xbebfbc70,
            0x289b7ec6,0xeaa127fa,0xd4ef3085,0x04881d05,0xd9d4d039,0xe6db99e5,0x1fa27cf8,0xc4ac5665,
            0xf4292244,0x432aff97,0xab9423a7,0xfc93a039,0x655b59c3,0x8f0ccc92,0xffeff47d,0x85845dd1,
            0x6fa87e4f,0xfe2ce6e0,0xa3014314,0x4e0811a1,0xf7537e82,0xbd3af235,0x2ad7d2bb,0xeb86d391,
        ];
        let mut words = [0u32; 16];
        for (i, word) in words.iter_mut().enumerate() {
            *word = u32::from_le_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
        }
        let mut a = state[0]; let mut b = state[1]; let mut c = state[2]; let mut d = state[3];
        for i in 0..64 {
            let (f, g) = if i < 16 { ((b & c) | ((!b) & d), i) }
                else if i < 32 { ((d & b) | ((!d) & c), (5 * i + 1) % 16) }
                else if i < 48 { (b ^ c ^ d, (3 * i + 5) % 16) }
                else { (c ^ (b | !d), (7 * i) % 16) };
            let rot = S[i / 16][i % 4];
            let value = a.wrapping_add(f).wrapping_add(K[i]).wrapping_add(words[g]);
            a = d; d = c; c = b; b = b.wrapping_add(value.rotate_left(rot));
        }
        state[0] = state[0].wrapping_add(a); state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c); state[3] = state[3].wrapping_add(d);
    }

    fn dxbc_checksum(data: &[u8]) -> [u8; 16] {
        let mut state = [0x67452301u32, 0xefcdab89, 0x98badcfe, 0x10325476];
        if data.len() <= 0x14 {
            let mut out = [0u8; 16];
            for (i, word) in state.iter().enumerate() {
                out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
            }
            return out;
        }
        let payload = &data[0x14..];
        let full = payload.len() & !63;
        for chunk in payload[..full].chunks_exact(64) {
            md5_transform(&mut state, chunk.try_into().unwrap());
        }
        let rem = &payload[full..];
        let bits = (payload.len() as u32).wrapping_mul(8);
        if rem.len() < 56 {
            let mut block = [0u8; 64];
            block[..4].copy_from_slice(&bits.to_le_bytes());
            block[4..4 + rem.len()].copy_from_slice(rem);
            block[4 + rem.len()] = 0x80;
            block[60..64].copy_from_slice(&((bits >> 2) | 1).to_le_bytes());
            md5_transform(&mut state, &block);
        } else {
            // Match CalculateDXBCChecksum exactly when the remainder is at
            // least 56 bytes: first consume the remainder plus the complete
            // padding block, then transform a second synthetic block whose
            // first/final DWORDs carry the DXBC bit-count markers.
            let mut padded = [0u8; 64];
            padded[..rem.len()].copy_from_slice(rem);
            padded[rem.len()] = 0x80;
            md5_transform(&mut state, &padded);

            let mut second = [0u8; 64];
            second[..4].copy_from_slice(&bits.to_le_bytes());
            second[60..64].copy_from_slice(&((bits >> 2) | 1).to_le_bytes());
            md5_transform(&mut state, &second);
        }
        let mut out = [0u8; 16];
        for (i, word) in state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    fn repair_dxbc_checksum(data: &mut [u8]) {
        if data.len() >= DX_HEADER_SIZE {
            let checksum = dxbc_checksum(data);
            data[4..20].copy_from_slice(&checksum);
        }
    }

    fn vec3_count(data: &[u8], value: f32) -> usize {
        let pat = [value, value, value, 0.0_f32];
        let bytes: &[u8] = bytemuck::bytes_of(&pat);
        data.windows(bytes.len()).filter(|window| *window == bytes).count()
    }

    fn replace_vec3(data: &mut [u8], old: f32, new: f32) -> u32 {
        let old_pat = [old, old, old, 0.0_f32];
        let new_pat = [new, new, new, 0.0_f32];
        let old_bytes: &[u8] = bytemuck::bytes_of(&old_pat);
        let new_bytes: &[u8] = bytemuck::bytes_of(&new_pat);
        let mut count = 0u32;
        let mut pos = 0usize;
        while pos + old_bytes.len() <= data.len() {
            if &data[pos..pos + old_bytes.len()] == old_bytes && old_bytes != new_bytes {
                data[pos..pos + new_bytes.len()].copy_from_slice(new_bytes);
                count = count.saturating_add(1);
                pos += new_bytes.len();
            } else { pos += 1; }
        }
        count
    }

    fn normalized_srgb(data: &[u8]) -> Option<Vec<u8>> {
        if data.len() < DX_HEADER_SIZE { return None; }
        // Try the untouched sRGB form first.
        let mut direct = data.to_vec();
        repair_dxbc_checksum(&mut direct);
        let direct_digest: [u8; 16] = direct[4..20].try_into().ok()?;
        if DWM_EOTF_WHITELIST.contains(&digest_u128(&direct_digest)) {
            return Some(direct);
        }

        // A pure-gamma patch collapses the two sRGB constants to identical
        // zero vectors. Try both possible assignments; only the checksum
        // whitelist decides which one is the real target shader.
        for gamma in [2.2_f32, 2.4, 2.6] {
            if vec3_count(data, gamma) == 0 || vec3_count(data, 0.0) < 2 || vec3_count(data, 1.0) == 0 { continue; }
            for &(first, second) in &[(0.04045_f32, 0.055_f32), (0.055_f32, 0.04045_f32)] {
                let mut candidate = data.to_vec();
                replace_vec3(&mut candidate, gamma, 2.4);
                replace_vec3_once(&mut candidate, 0.0, first);
                replace_vec3_once(&mut candidate, 0.0, second);
                replace_vec3(&mut candidate, 1.0, 0.94786733);
                repair_dxbc_checksum(&mut candidate);
                let digest: [u8; 16] = candidate[4..20].try_into().ok()?;
                if DWM_EOTF_WHITELIST.contains(&digest_u128(&digest)) { return Some(candidate); }
            }
        }
        None
    }

    fn patch_one_dxbc(data: &mut [u8], profile: &str, hash: u128) -> u32 {
        // Accept a live shader directly only when both its stored digest and
        // the calculated DXBC checksum agree. The scan's whitelist identifies
        // the expected shader, while this second check prevents a stale or
        // corrupted container from being treated as trusted. Patched shaders
        // have a new digest, so normalized_srgb reconstructs their original
        // whitelisted form before applying the next profile.
        let stored_checksum_valid = data.len() >= DX_HEADER_SIZE
            && dxbc_checksum(data) == data[4..20];
        let Some(mut canonical) = (if DWM_EOTF_WHITELIST.contains(&hash)
            && stored_checksum_valid
        {
            Some(data.to_vec())
        } else {
            normalized_srgb(data)
        }) else { return 0; };
        let original = data.to_vec();
        match profile {
            "srgb" => {}
            "2.2" | "2.4" | "2.6" => {
                let gamma: f32 = profile.parse().unwrap_or(2.2);
                replace_vec3(&mut canonical, 2.4, gamma);
                replace_vec3(&mut canonical, 0.04045, 0.0);
                replace_vec3(&mut canonical, 0.055, 0.0);
                replace_vec3(&mut canonical, 0.94786733, 1.0);
            }
            _ => return 0,
        }
        repair_dxbc_checksum(&mut canonical);
        if canonical == original { return 0; }
        data.copy_from_slice(&canonical);
        1
    }

    fn replace_vec3_once(data: &mut [u8], old: f32, new: f32) -> u32 {
        let old_pat = [old, old, old, 0.0_f32];
        let new_pat = [new, new, new, 0.0_f32];
        let old_bytes: &[u8] = bytemuck::bytes_of(&old_pat);
        if let Some(pos) = data.windows(old_bytes.len()).position(|w| w == old_bytes) {
            data[pos..pos + old_bytes.len()].copy_from_slice(bytemuck::bytes_of(&new_pat));
            1
        } else { 0 }
    }

    fn patch_recursive(raw: &mut [u8], profile: &str) -> (u32, u32, Vec<String>) {
        let mut found = 0u32; let mut patched = 0u32; let mut details = Vec::new(); let mut start = 0usize;
        while start + DX_HEADER_SIZE <= raw.len() {
            if !raw[start..].starts_with(&DXBC_MAGIC) { start += 1; continue; }
            let file_size = u32::from_le_bytes(raw[start + 24..start + 28].try_into().unwrap()) as usize;
            let Some(end) = start.checked_add(file_size) else { start += 1; continue; };
            if file_size < DX_HEADER_SIZE || end > raw.len() { start += 1; continue; }
            let (sub_found, sub_patched, sub_details) = patch_recursive(&mut raw[start + DX_HEADER_SIZE..end], profile);
            if sub_found > 0 {
                found += sub_found; patched += sub_patched; details.extend(sub_details);
                if sub_patched > 0 { repair_dxbc_checksum(&mut raw[start..end]); }
            } else {
                let digest: [u8; 16] = raw[start + 4..start + 20].try_into().unwrap();
                let hash = digest_u128(&digest);
                let changed = patch_one_dxbc(&mut raw[start..end], profile, hash);
                if DWM_EOTF_WHITELIST.contains(&hash) || changed > 0 { found += 1; }
                if changed > 0 { patched += 1; details.push(format!("{:032x} ({changed} replacements)", hash)); }
            }
            start = end;
        }
        (found, patched, details)
    }

    // Native process-level suspension used by dwm_eotf_rs. Suspending the
    // process atomically is important: a thread-by-thread snapshot can miss
    // a render thread or let DWM observe a partially patched shader.
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtSuspendProcess(process: *mut c_void) -> i32;
        fn NtResumeProcess(process: *mut c_void) -> i32;
    }

    /// Phase 3b implementation aligned with dwm_eotf_rs: suspend the entire
    /// process, snapshot the complete READONLY shader regions, recursively
    /// patch nested DXBC containers, repair each modified DXBC checksum, write
    /// every modified region back, and resume the process.
    struct ProcessSuspendGuard {
        process: *mut c_void,
        resumed: bool,
    }

    impl ProcessSuspendGuard {
        fn resume(&mut self) -> i32 {
            if self.resumed { return 0; }
            let status = unsafe { NtResumeProcess(self.process) };
            // Keep the guard armed when the first resume attempt fails so its
            // Drop implementation gets one more chance to recover DWM.
            if status == 0 {
                self.resumed = true;
            }
            status
        }
    }

    impl Drop for ProcessSuspendGuard {
        fn drop(&mut self) {
            if !self.resumed {
                let status = unsafe { NtResumeProcess(self.process) };
                if status != 0 {
                    log::error!("[dwm-eotf] emergency NtResumeProcess failed: NTSTATUS 0x{status:08X}");
                }
            }
        }
    }

    pub fn apply_dwm_patch_v2(profile: &str) -> DwmEotfScanResult {
        let Some(_) = gamma_replacements(profile) else {
            return result("invalid-profile", format!("Unknown gamma profile '{profile}'. Use srgb, 2.2, 2.4, or 2.6."));
        };
        if let Err(err) = enable_debug_privilege() {
            let mut out = result("privilege-failed", format!("SeDebugPrivilege: {err}"));
            out.diagnostic_code = 1;
            return out;
        }
        // Kill DWM and patch the freshly restarted instance. Patching the
        // long-running compositor writes memory that its GPU pipeline never
        // recompiles ,  the upstream tool restarts DWM first so the patched
        // bytecode is loaded before shader compilation happens.
        let pid = match restart_dwm() {
            Ok(pid) => pid,
            Err(err) => return result("restart-failed", format!("DWM restart failed: {err}")),
        };
        // dwmcore.dll is not resident the instant the new DWM process appears;
        // poll for the module like wait_module_by_name_and_pid does upstream.
        let module_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let (base, size) = loop {
            match find_dwmcore(pid) {
                Ok(Some(module)) => break module,
                Ok(None) | Err(_) => {
                    if std::time::Instant::now() >= module_deadline {
                        return result("module-not-found", "dwmcore.dll not found in restarted DWM.");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        };
        let Ok(process) = (unsafe { OpenProcess(PROCESS_ALL_ACCESS, false, pid) }) else {
            return result("access-denied", "Cannot open restarted DWM with PROCESS_ALL_ACCESS.");
        };
        let Some(process) = Handle::new(process) else {
            return result("open-failed", "Invalid restarted DWM process handle.");
        };

        let process_ptr = process.0.0 as *mut c_void;
        let suspend_status = unsafe { NtSuspendProcess(process_ptr) };
        if suspend_status != 0 {
            return result("suspend-failed", format!("NtSuspendProcess failed with NTSTATUS 0x{suspend_status:08X}."));
        }
        let mut suspend_guard = ProcessSuspendGuard { process: process_ptr, resumed: false };

        struct Region {
            address: usize,
            size: usize,
            offset: usize,
        }
        log::info!("[dwm-eotf] apply v2: pid={pid} dwmcore base=0x{base:x} size={size} profile={profile}");
        let mut regions = Vec::new();
        let mut memory = Vec::new();
        let mut address = base;
        let end = base.saturating_add(size);
        let mut skipped_unreadable = 0u32;
        let mut total_regions_seen = 0u32;
        while address < end && memory.len() < MAX_SCAN_BYTES {
            let mut info = MEMORY_BASIC_INFORMATION::default();
            let queried = unsafe {
                VirtualQueryEx(process.0, Some(address as *const c_void), &mut info, std::mem::size_of::<MEMORY_BASIC_INFORMATION>())
            };
            if queried == 0 || info.RegionSize == 0 { break; }
            let region_start = info.BaseAddress as usize;
            let region_end = region_start.saturating_add(info.RegionSize);
            let clipped_start = region_start.max(base);
            let clipped_end = region_end.min(end);
            // Keep the apply path's memory predicate identical to the dry
            // run. DWM shader containers may live in executable-readable
            // pages (for example PAGE_EXECUTE_READ), not only PAGE_READONLY.
            // The whitelist and checksum validation below still prevent
            // unrelated memory from being modified.
            total_regions_seen = total_regions_seen.saturating_add(1);
            let is_readable = info.State.0 == MEM_COMMIT.0 && readable_committed(info.Protect.0);
            if is_readable && clipped_start < clipped_end {
                let region_size = (clipped_end - clipped_start).min(MAX_SCAN_BYTES - memory.len());
                let bytes = read_dwm_bytes(&process, clipped_start, region_size);
                if bytes.len() == region_size {
                    let offset = memory.len();
                    memory.extend_from_slice(&bytes);
                    regions.push(Region { address: clipped_start, size: region_size, offset });
                } else {
                    skipped_unreadable = skipped_unreadable.saturating_add(1);
                    log::info!("[dwm-eotf] apply v2: partial read region 0x{clipped_start:x} len={region_size} got={}", bytes.len());
                }
            } else if !is_readable && info.State.0 == MEM_COMMIT.0 {
                log::info!("[dwm-eotf] apply v2: skip unreadable committed region 0x{clipped_start:x} len={} protect=0x{:x}", clipped_end - clipped_start, info.Protect.0);
            }
            let next = region_end.max(address.saturating_add(0x1000));
            if next <= address { break; }
            address = next;
        }
        log::info!("[dwm-eotf] apply v2: regions_seen={total_regions_seen} captured={} memory_bytes={} skipped_unreadable={skipped_unreadable}", regions.len(), memory.len());

        let mut header_hits = 0u32;
        let mut shader_count = 0u32;
        let mut patched_count = 0u32;
        let mut details = Vec::new();
        let mut patches: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut covered_ranges: Vec<(usize, usize)> = Vec::new();

        // Scan headers from the captured regions, but always read each full
        // DXBC body from its real VA. A shader can cross a VirtualQueryEx
        // region boundary; patching the region slice alone truncates it and
        // creates the dry-run-ready/apply-skipped mismatch.
        for region in &regions {
            let slice = &memory[region.offset..region.offset + region.size];
            let mut i = 0usize;
            while i + DX_HEADER_SIZE <= slice.len() {
                if !slice[i..].starts_with(&DXBC_MAGIC) {
                    i = i.saturating_add(1);
                    continue;
                }
                header_hits = header_hits.saturating_add(1);
                let file_size = u32::from_le_bytes(
                    slice[i + 24..i + 28].try_into().unwrap_or([0u8; 4]),
                ) as usize;
                if !(DX_HEADER_SIZE..=MAX_SCAN_BYTES).contains(&file_size) {
                    log::info!("[dwm-eotf] apply v2: bad file_size={file_size} at VA=0x{:x}", region.address.saturating_add(i));
                    i = i.saturating_add(DX_HEADER_SIZE);
                    continue;
                }
                let shader_va = region.address.saturating_add(i);
                if covered_ranges.iter().any(|(start, end)| {
                    shader_va >= *start && shader_va < *end
                }) {
                    i = i.saturating_add(DX_HEADER_SIZE);
                    continue;
                }
                let mut shader = read_dwm_bytes(&process, shader_va, file_size);
                if shader.len() != file_size {
                    log::info!("[dwm-eotf] apply v2: body read short VA=0x{shader_va:x} file={file_size} got={}", shader.len());
                    i = i.saturating_add(DX_HEADER_SIZE);
                    continue;
                }
                let (found, patched, found_details) = patch_recursive(&mut shader, profile);
                log::info!("[dwm-eotf] apply v2: candidate VA=0x{shader_va:x} file={file_size} found={found} patched={patched}");
                shader_count = shader_count.saturating_add(found);
                patched_count = patched_count.saturating_add(patched);
                details.extend(found_details);
                if found > 0 {
                    covered_ranges.push((shader_va, shader_va.saturating_add(file_size)));
                }
                if patched > 0 {
                    patches.push((shader_va, shader));
                }
                i = i.saturating_add(DX_HEADER_SIZE);
            }
        }
        log::info!("[dwm-eotf] apply v2: header_hits={header_hits} shaders_found={shader_count} patched={patched_count}");

        let mut write_failures = 0u32;
        for (shader_va, shader) in patches {
            let file_size = shader.len();
            let mut offset = 0usize;
            let mut shader_failed = false;
            while offset < file_size {
                let segment_va = shader_va.saturating_add(offset);
                let mut info = MEMORY_BASIC_INFORMATION::default();
                let queried = unsafe {
                    VirtualQueryEx(
                        process.0,
                        Some(segment_va as *const c_void),
                        &mut info,
                        std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
                    )
                };
                if queried == 0 || info.RegionSize == 0 {
                    shader_failed = true;
                    break;
                }
                let region_end = (info.BaseAddress as usize).saturating_add(info.RegionSize);
                let segment_end = region_end.min(shader_va.saturating_add(file_size));
                if segment_end <= segment_va
                    || info.State.0 != MEM_COMMIT.0
                    || !readable_committed(info.Protect.0)
                {
                    shader_failed = true;
                    break;
                }
                let segment_len = segment_end - segment_va;
                let mut old_protect = PAGE_PROTECTION_FLAGS::default();
                let protected = unsafe {
                    VirtualProtectEx(
                        process.0,
                        segment_va as *const c_void,
                        segment_len,
                        PAGE_READWRITE,
                        &mut old_protect,
                    )
                };
                if protected.is_err() {
                    shader_failed = true;
                    break;
                }
                let mut written = 0usize;
                let write_ok = unsafe {
                    WriteProcessMemory(
                        process.0,
                        segment_va as *const c_void,
                        shader[offset..offset + segment_len].as_ptr() as *const c_void,
                        segment_len,
                        Some(&mut written),
                    )
                };
                let mut restored = PAGE_PROTECTION_FLAGS::default();
                let restored_ok = unsafe {
                    VirtualProtectEx(
                        process.0,
                        segment_va as *const c_void,
                        segment_len,
                        old_protect,
                        &mut restored,
                    )
                };
                if write_ok.is_err() || written != segment_len || restored_ok.is_err() {
                    shader_failed = true;
                    break;
                }
                offset = offset.saturating_add(segment_len);
            }
            if shader_failed {
                write_failures = write_failures.saturating_add(1);
            }
        }

        let resume_status = suspend_guard.resume();
        if resume_status != 0 {
            return DwmEotfScanResult {
                status: "resume-failed".into(),
                message: format!("DWM was patched but NtResumeProcess failed with NTSTATUS 0x{resume_status:08X}. Restart DWM immediately."),
                dwm_pid: pid,
                dwmcore_base: base as u64,
                dwmcore_size: size as u64,
                readable_regions: regions.len() as u32,
                scanned_bytes: memory.len() as u64,
                constant_hits: 0,
                constants_found: Vec::new(),
                diagnostic_code: resume_status as u32,
                debug_privilege_used: true,
                dxbc_containers: shader_count,
                whitelist_hits: patched_count,
                whitelist_matched: details,
            };
        }

        let status = if write_failures > 0 { "patch-partial" } else if patched_count > 0 { "patched" } else { "patch-skipped" };
        let message = if write_failures > 0 {
            format!("Patched {patched_count} shader(s), but {write_failures} memory-region write(s) failed. DWM was resumed; restart DWM to restore a clean state.")
        } else if patched_count > 0 {
            format!("Patched {patched_count} of {shader_count} nested shader(s) with {profile} EOTF. DXBC checksums repaired; DWM was suspended at process level and resumed.")
        } else {
            "DWM was restarted and scanned, but no whitelisted shaders needed patching (or none were found in readable memory). The fresh compositor is running unpatched with its original sRGB EOTF.".to_string()
        };
        DwmEotfScanResult {
            status: status.into(),
            message,
            dwm_pid: pid,
            dwmcore_base: base as u64,
            dwmcore_size: size as u64,
            readable_regions: regions.len() as u32,
            scanned_bytes: memory.len() as u64,
            constant_hits: 0,
            constants_found: Vec::new(),
            diagnostic_code: 0,
            debug_privilege_used: true,
            dxbc_containers: shader_count,
            whitelist_hits: patched_count,
            whitelist_matched: details,
        }
    }

    fn scan_impl(try_debug_privilege: bool) -> DwmEotfScanResult {
        if try_debug_privilege {
            if let Err(err) = enable_debug_privilege() {
                let mut out = result("privilege-failed", format!("Could not enable SeDebugPrivilege: {err}"));
                out.diagnostic_code = 1;
                return out;
            }
        }
        let Some(pid) = find_dwm_pid() else { return result("not-found", "Desktop Window Manager (dwm.exe) is not running."); };
        let (base, size) = match find_dwmcore(pid) {
            Ok(Some(module)) => module,
            Ok(None) => {
                let mut out = result("module-not-found", "dwmcore.dll was not listed in DWM's module snapshot.");
                out.dwm_pid = pid;
                return out;
            }
            Err(code) => {
                let mut out = result("module-access", module_access_message(code));
                out.dwm_pid = pid;
                out.diagnostic_code = code;
                return out;
            }
        };
        let access = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ;
        let Ok(process) = (unsafe { OpenProcess(access, false, pid) }) else {
            let mut out = result("access-denied", "Windows did not allow a read-only DWM process handle.");
            out.dwm_pid = pid;
            out.dwmcore_base = base as u64;
            out.dwmcore_size = size as u64;
            return out;
        };
        let Some(process) = Handle::new(process) else {
            let mut out = result("open-failed", "Windows returned an invalid read-only DWM process handle.");
            out.dwm_pid = pid;
            out.dwmcore_base = base as u64;
            out.dwmcore_size = size as u64;
            return out;
        };
        let patterns_f32: Vec<(&str, Vec<u8>)> = KNOWN_CONSTANTS.iter()
            .map(|(name, value)| (*name, value.to_le_bytes().to_vec()))
            .collect();
        let patterns_vec3: Vec<(&str, Vec<u8>)> = DXBC_VEC3_PATTERNS.iter()
            .map(|(name, block)| (*name, bytemuck::bytes_of(&block[0]).to_vec()))
            .collect();
        let all_patterns: Vec<(&str, Vec<u8>)> = patterns_f32.into_iter().chain(patterns_vec3).collect();
        let mut found = vec![false; all_patterns.len()];
        let mut readable_regions = 0u32;
        let mut scanned_bytes = 0usize;
        let mut address = base;
        let end = base.saturating_add(size);
        while address < end && scanned_bytes < MAX_SCAN_BYTES {
            let mut info = MEMORY_BASIC_INFORMATION::default();
            let queried = unsafe { VirtualQueryEx(process.0, Some(address as *const c_void), &mut info, std::mem::size_of::<MEMORY_BASIC_INFORMATION>()) };
            if queried == 0 || info.RegionSize == 0 { break; }
            let region_start = info.BaseAddress as usize;
            let region_end = region_start.saturating_add(info.RegionSize);
            let clipped_start = region_start.max(base);
            let clipped_end = region_end.min(end);
            if info.State.0 == MEM_COMMIT.0 && readable_committed(info.Protect.0) && clipped_start < clipped_end {
                readable_regions = readable_regions.saturating_add(1);
                let mut cursor = clipped_start;
                let mut carry = Vec::new();
                while cursor < clipped_end && scanned_bytes < MAX_SCAN_BYTES {
                    let length = (clipped_end - cursor).min(MAX_READ_CHUNK).min(MAX_SCAN_BYTES - scanned_bytes);
                    let mut buffer = vec![0u8; length];
                    let mut bytes_read = 0usize;
                    let read_ok = unsafe { ReadProcessMemory(process.0, cursor as *const c_void, buffer.as_mut_ptr() as *mut c_void, length, Some(&mut bytes_read)).is_ok() };
                    if read_ok && bytes_read > 0 {
                        let bytes = &buffer[..bytes_read.min(buffer.len())];
                        let mut search = Vec::with_capacity(carry.len() + bytes.len());
                        search.extend_from_slice(&carry);
                        search.extend_from_slice(bytes);
                        for (index, (_, pattern)) in all_patterns.iter().enumerate() {
                            if !found[index] && find_pattern(&search, pattern) { found[index] = true; }
                        }
                        carry.clear();
                        carry.extend_from_slice(&search[search.len().saturating_sub(15)..]);
                        scanned_bytes = scanned_bytes.saturating_add(bytes.len());
                        cursor = cursor.saturating_add(bytes.len());
                    } else {
                        carry.clear();
                        cursor = cursor.saturating_add(length.max(1));
                        scanned_bytes = scanned_bytes.saturating_add(length.min(MAX_SCAN_BYTES - scanned_bytes));
                    }
                }
            }
            let next = region_end.max(address.saturating_add(0x1000));
            if next <= address { break; }
            address = next;
        }
        let constants_found: Vec<String> = all_patterns.iter().enumerate().filter_map(|(index, (name, _))| found[index].then_some((*name).to_string())).collect();
        let constant_hits = constants_found.len() as u32;
        let f32_hits = KNOWN_CONSTANTS.iter().filter(|(name, _)| { let idx = all_patterns.iter().position(|(n, _)| *n == *name).unwrap_or(0); found[idx] }).count() as u32;
        let vec3_hits = DXBC_VEC3_PATTERNS.iter().filter(|(name, _)| { let idx = all_patterns.iter().position(|(n, _)| *n == *name).unwrap_or(0); idx >= KNOWN_CONSTANTS.len() && found[idx] }).count() as u32;
        let all_known = KNOWN_CONSTANTS.len() as u32;
        let all_vec3 = DXBC_VEC3_PATTERNS.len() as u32;
        let (dxbc_containers, whitelist_hits, whitelist_matched) = scan_shaders(pid, base, size);

        let mut msg = if f32_hits == all_known {
            "All known sRGB constants were found in readable DWM memory. This is only a compatibility signal; no shader was identified or modified.".to_string()
        } else {
            format!("Found {f32_hits}/{all_known} f32 constants and {vec3_hits}/{all_vec3} DXBC vec3 patterns in the scanned pages.")
        };
        let mut status = if f32_hits == all_known { "constants-found".to_string() } else { "scanned".to_string() };
        if whitelist_hits == DWM_EOTF_WHITELIST.len() as u32 {
            status = "whitelist-match".into();
            msg = format!("{msg} All {whitelist_hits} known dwm_eotf_rs shader digests matched: full compatibility confirmed.");
        } else if whitelist_hits > 0 {
            status = "whitelist-partial".into();
            msg = format!("{msg} {whitelist_hits}/{} whitelist digests matched.", DWM_EOTF_WHITELIST.len());
        }

        let mut out = DwmEotfScanResult {
            status,
            message: msg,
            dwm_pid: pid,
            dwmcore_base: base as u64,
            dwmcore_size: size as u64,
            readable_regions,
            scanned_bytes: scanned_bytes as u64,
            constant_hits,
            constants_found,
            diagnostic_code: 0,
            debug_privilege_used: try_debug_privilege,
            dxbc_containers,
            whitelist_hits,
            whitelist_matched,
        };
        if scanned_bytes >= MAX_SCAN_BYTES { out.message.push_str(" The scan stopped at its safety limit."); }
        out
    }
}

#[cfg(target_os = "windows")]
pub use windows_scan::{
    apply_dwm_patch_v2, current_dwm_pid, dump_shader_digests, scan, scan_dwm_dry_run,
    scan_dwm_shaders, scan_with_debug_privilege,
};
