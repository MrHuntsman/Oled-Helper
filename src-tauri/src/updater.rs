// Portable self-updater.
//
// The app is distributed as a single portable exe (no installer), so the
// official tauri-plugin-updater does not apply: it installs via NSIS/MSI
// bundles. Updates instead work like this:
//
//  1. The release pipeline uploads the new exe plus a tiny `latest.json`
//     manifest (version, download url, sha256) to the GitHub release.
//  2. `check_for_update` fetches the manifest and compares versions, with
//     release notes pulled from the GitHub API for the update modal.
//  3. `download_update` streams the new exe to `<app>.new.exe` next to the
//     running binary (same filesystem, so the final rename is atomic),
//     emitting `update-progress` events for the progress bar, and verifies
//     the sha256 checksum from the manifest.
//  4. `apply_update` spawns that staged exe with
//     `--apply-update <old-exe-path> <old-pid>` and shuts the app down.
//  5. The staged process (see `handle_apply_update_mode`) waits for the old
//     process to exit, moves the old exe aside, renames itself into place,
//     launches the fresh app, and exits. Windows allows renaming the running
//     image (the loader opens it with FILE_SHARE_DELETE), which is the same
//     mechanism rustup/self-replace use.
//
// Checks fail silently by design: an offline machine or a missing release
// must never disturb the app, it just leaves the update indicator hidden.
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter};
use url::Url;

const MANIFEST_URL: &str =
    "https://github.com/MrHuntsman/Oled-Helper/releases/latest/download/latest.json";
// Used only for release notes/date in the update modal (best effort).
const RELEASE_API_URL: &str = "https://api.github.com/repos/MrHuntsman/Oled-Helper/releases/latest";
// Sanity cap on the staged download. A corrupt/bad manifest must never be able
// to point the updater at a multi-GB file and exhaust the disk next to the exe.
const MAX_UPDATE_SIZE: u64 = 512 * 1024 * 1024; // 512 MiB

// The only hosts that may serve the staged exe. The manifest itself is already
// fetched over HTTPS from the project's own release, but its download URL is
// handed back to the frontend and passed straight into download_update, and
// the sha256 that "verifies" the download comes from that same manifest. A
// manifest (or a caller) that points at any other host could therefore stage
// an arbitrary executable, so the host is pinned to the GitHub release
// endpoints the project actually publishes to.
const UPDATE_HOSTS: [&str; 3] = [
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

#[derive(Serialize, Clone)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub notes: String,
    pub date: String,
}

#[derive(Deserialize)]
struct Manifest {
    version: String,
    url: String,
    #[serde(default)]
    sha256: String,
}

#[derive(Deserialize)]
struct GithubRelease {
    #[serde(default)]
    body: String,
    #[serde(default)]
    published_at: String,
}

#[derive(Clone, Serialize)]
struct Progress {
    downloaded: u64,
    total: u64,
}

// Validate the download URL beyond its scheme: it must be an absolute HTTPS
// URL with no embedded credentials and a host from UPDATE_HOSTS, so a
// manipulated manifest cannot redirect the staged, checksum-verified
// executable download to an attacker controlled host.
fn validate_update_url(url: &str) -> Result<Url, String> {
    let parsed = Url::parse(url.trim())
        .map_err(|_| "the update manifest returned an invalid download address.".to_string())?;
    if parsed.scheme() != "https" {
        return Err("update downloads must use HTTPS; the update was aborted.".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("the update download address was rejected for safety.".into());
    }
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    if !UPDATE_HOSTS.iter().any(|allowed| host == *allowed) {
        return Err(
            "the update download address is not a recognized release host; the update was aborted."
                .into(),
        );
    }
    Ok(parsed)
}

// Compares semver-ish "x.y.z" strings (a leading "v" is tolerated). Returns
// true when `remote` is a strictly newer version than `current`.
fn version_greater(remote: &str, current: &str) -> bool {
    let parse = |version: &str| -> Vec<u64> {
        version
            .trim_start_matches('v')
            .split('.')
            .filter_map(|part| {
                let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
                digits.parse::<u64>().ok()
            })
            .collect()
    };
    let remote_parts = parse(remote);
    let current_parts = parse(current);
    for (remote_part, current_part) in remote_parts.iter().zip(current_parts.iter()) {
        if remote_part != current_part {
            return remote_part > current_part;
        }
    }
    remote_parts.len() > current_parts.len()
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("OLED-Helper-Updater")
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| format!("could not create HTTP client: {error}"))
}

// The staged copy is written next to the running exe so the final rename in
// apply-update mode stays on the same filesystem (cross-drive renames fail).
fn update_staging_path() -> Result<PathBuf, String> {
    let exe =
        std::env::current_exe().map_err(|error| format!("could not locate the app: {error}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "could not locate the app folder".to_string())?;
    let stem = exe
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("OledHelper");
    Ok(dir.join(format!("{stem}.new.exe")))
}

// Fetches the release manifest and reports whether a newer version exists.
// Never throws for network problems: a failed check just means "no update
// known", so startup checks stay silent.
#[tauri::command]
pub async fn check_for_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    let current = app.package_info().version.to_string();
    let client = http_client()?;
    let response = client.get(MANIFEST_URL).send().await.map_err(|error| {
        log::debug!("[updater] manifest fetch failed (silent): {error}");
        error.to_string()
    })?;
    if !response.status().is_success() {
        log::debug!(
            "[updater] manifest fetch returned HTTP {}",
            response.status()
        );
        return Err(format!(
            "manifest fetch returned HTTP {}",
            response.status()
        ));
    }
    let manifest: Manifest = response
        .json()
        .await
        .map_err(|error| format!("invalid update manifest: {error}"))?;
    if !version_greater(&manifest.version, &current) {
        return Ok(None);
    }
    // Release notes are a nice-to-have; never fail the check over them.
    let (notes, date) = match client.get(RELEASE_API_URL).send().await {
        Ok(response) if response.status().is_success() => {
            match response.json::<GithubRelease>().await {
                Ok(release) => (release.body, release.published_at),
                Err(_) => (String::new(), String::new()),
            }
        }
        _ => (String::new(), String::new()),
    };
    Ok(Some(UpdateInfo {
        version: manifest.version,
        url: manifest.url,
        sha256: manifest.sha256,
        notes,
        date,
    }))
}

// Streams the new exe to `<app>.new.exe`, emitting update-progress events,
// and verifies the sha256 from the manifest before handing off to
// apply_update.
#[tauri::command]
pub async fn download_update(
    app: AppHandle,
    url: String,
    sha256: String,
) -> Result<String, String> {
    // The manifest must carry a checksum. Without one, the staged exe could
    // be anything, so refuse instead of silently skipping verification.
    if sha256.trim().is_empty() {
        return Err(
            "the update manifest is missing its checksum; the update was aborted for safety."
                .into(),
        );
    }
    // Only an HTTPS URL on a known release host is acceptable for the download
    // itself. The URL is normally read back from the manifest, but the command
    // is otherwise unconstrained, so enforce both here rather than trusting
    // the caller.
    let download_url = validate_update_url(&url)?;
    let target = update_staging_path()?;
    let client = http_client()?;
    let response = client
        .get(download_url)
        .send()
        .await
        .map_err(|error| format!("could not download the update: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "update download failed (HTTP {}). Try again later.",
            response.status()
        ));
    }
    let total = response.content_length().unwrap_or(0);
    if total > MAX_UPDATE_SIZE {
        return Err("the update is larger than the allowed size; the update was aborted.".into());
    }
    let mut file = std::fs::File::create(&target).map_err(|error| {
        format!(
            "the app folder is not writable ({error}). Move OLED Helper to a writable folder to update it."
        )
    })?;
    let mut hasher = Sha256::new();
    let mut downloaded: u64 = 0;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("update download interrupted: {error}"))?;
        hasher.update(&chunk);
        file.write_all(&chunk)
            .map_err(|error| format!("could not write the update: {error}"))?;
        downloaded += chunk.len() as u64;
        if downloaded > MAX_UPDATE_SIZE {
            let _ = std::fs::remove_file(&target);
            return Err("the update exceeded the allowed size; the update was aborted.".into());
        }
        let _ = app.emit("update-progress", Progress { downloaded, total });
    }
    file.flush()
        .map_err(|error| format!("could not write the update: {error}"))?;
    drop(file);
    let digest = format!("{:x}", hasher.finalize());
    if !digest.eq_ignore_ascii_case(sha256.trim()) {
        let _ = std::fs::remove_file(&target);
        return Err(
            "update verification failed (checksum mismatch). The download may be corrupt; try again."
                .into(),
        );
    }
    Ok(target.to_string_lossy().into_owned())
}

// Hands off to the staged exe (--apply-update) and shuts the app down. The
// staged process performs the file swap and relaunch once this process exits.
#[tauri::command]
pub fn apply_update(app: AppHandle) -> Result<(), String> {
    let staging = update_staging_path()?;
    if !staging.exists() {
        return Err("the downloaded update is missing. Download it again.".into());
    }
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let pid = std::process::id().to_string();
    if let Err(error) = std::process::Command::new(&staging)
        .arg("--apply-update")
        .arg(&exe)
        .arg(&pid)
        .spawn()
    {
        return Err(format!("could not start the update process: {error}"));
    }
    crate::taskbar_dimmer::shutdown();
    crate::shutdown_gamma_worker(&app);
    app.exit(0);
    Ok(())
}

// The `--apply-update <old-exe-path> <old-pid>` entry point. Runs before any
// Tauri/WebView2 initialization, so the staged copy can swap files while the
// old process is gone. Returns true when the flag was consumed (caller must
// exit). Keeps the old exe intact on any failure so the app is never left
// missing; the staged copy stays behind for a manual retry.
pub fn handle_apply_update_mode() -> bool {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let Some(pos) = args.iter().position(|arg| arg == "--apply-update") else {
        return false;
    };
    let Some(target) = args.get(pos + 1).map(PathBuf::from) else {
        return false;
    };
    let Some(parent_pid) = args
        .get(pos + 2)
        .and_then(|arg| arg.to_str())
        .and_then(|text| text.parse::<u32>().ok())
    else {
        return false;
    };

    // The old process is shutting down; wait for it to fully exit so the exe
    // file is unlocked and WebView2's profile is released before swapping. A
    // failed wait is a safe abort: the old instance is likely still running,
    // so leave its exe untouched and just exit.
    #[cfg(target_os = "windows")]
    if !crate::process::wait_for_restart_parent(parent_pid) {
        log::error!("[updater] old instance did not exit cleanly; aborting update");
        std::process::exit(1);
    }

    // Original launch arguments minus the apply flag and its two values, so a
    // relaunched exe starts the same way the user launched the app.
    let mut launch_args: Vec<std::ffi::OsString> = Vec::new();
    let mut index = 1;
    while index < args.len() {
        if args[index] == "--apply-update" {
            index += 3;
            continue;
        }
        launch_args.push(args[index].clone());
        index += 1;
    }
    // Strip every one-shot handoff flag forwarded from an earlier UAC
    // relaunch (see process::strip_handoff_args): the relaunched app must not
    // wait on the original launcher's pid, which may long since have been
    // reused by another process, nor re-apply stale intent (tab, RTX HDR,
    // screen dimmer) the user has since changed.
    launch_args = crate::process::strip_handoff_args(launch_args);
    // Every failure path after the old process has exited relaunches the app
    // (with the old exe restored where needed), so a failed update never
    // leaves the user with nothing running.
    fn relaunch(exe: &std::path::Path, launch_args: &[std::ffi::OsString]) {
        if let Err(error) = std::process::Command::new(exe).args(launch_args).spawn() {
            log::error!("[updater] could not relaunch the app after a failed update: {error}");
        }
    }

    let stem = target
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("OledHelper");
    let old_backup = target.with_file_name(format!("{stem}.old"));

    // 1. Move the old (exited) exe aside.
    if let Err(error) = std::fs::rename(&target, &old_backup) {
        log::error!("[updater] could not move the old app aside: {error}");
        relaunch(&target, &launch_args);
        std::process::exit(1);
    }
    // 2. Move ourselves (the staged copy, running from its temp name) into
    //    the app's place. Renaming the running image is allowed on Windows.
    let self_exe = std::env::current_exe().unwrap_or_default();
    if let Err(_error) = std::fs::rename(&self_exe, &target) {
        // Fallback: copy ourselves over and leave the staging copy behind.
        if let Err(copy_error) = std::fs::copy(&self_exe, &target) {
            log::error!("[updater] could not install the new app: {copy_error}");
            let _ = std::fs::rename(&old_backup, &target);
            relaunch(&target, &launch_args);
            std::process::exit(1);
        }
    }
    // 3. Launch the fresh app. On failure, roll back to the old exe and
    //    relaunch that instead, so the user always gets a running app.
    if let Err(error) = std::process::Command::new(&target)
        .args(&launch_args)
        .spawn()
    {
        log::error!("[updater] update installed but could not launch the new app: {error}");
        let _ = std::fs::remove_file(&target);
        let _ = std::fs::rename(&old_backup, &target);
        relaunch(&target, &launch_args);
        std::process::exit(1);
    }
    // 4. Best-effort cleanup of the old exe (and the staging copy if the
    //    rename fallback left it behind). AV can briefly hold a handle to
    //    the just-exited old process, so retry a few times.
    for _ in 0..10 {
        if std::fs::remove_file(&old_backup).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let _ = std::fs::remove_file(&self_exe);
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_downloads_are_limited_to_https_release_hosts() {
        for accepted in [
            "https://github.com/MrHuntsman/Oled-Helper/releases/download/v1.0.3/OledHelper.exe",
            "https://objects.githubusercontent.com/github-production-release-asset/1234",
            "https://release-assets.githubusercontent.com/github-production-release-asset/1234",
            "  https://github.com/MrHuntsman/Oled-Helper/releases/latest/download/OledHelper.exe  ",
        ] {
            assert!(
                validate_update_url(accepted).is_ok(),
                "expected release URL to be accepted: {accepted}"
            );
        }
        for rejected in [
            "http://github.com/MrHuntsman/Oled-Helper/releases/download/v1.0.3/OledHelper.exe",
            "https://github.com.evil.example/MrHuntsman/Oled-Helper/releases/download/v1.0.3/OledHelper.exe",
            "https://evil.example/MrHuntsman/Oled-Helper/releases/download/v1.0.3/OledHelper.exe",
            "https://user:pass@github.com/MrHuntsman/Oled-Helper/releases/download/v1.0.3/OledHelper.exe",
            "file:///C:/Windows/System32/calc.exe",
            "/C:/Windows/System32/calc.exe",
            "",
        ] {
            assert!(
                validate_update_url(rejected).is_err(),
                "expected URL to be rejected: {rejected}"
            );
        }
    }

    #[test]
    fn newer_semver_is_detected_with_optional_v_prefix() {
        assert!(version_greater("1.0.3", "1.0.2"));
        assert!(version_greater("v1.1.0", "1.0.9"));
        assert!(version_greater("2.0.0", "1.9.9"));
        assert!(!version_greater("1.0.2", "1.0.2"));
        assert!(!version_greater("1.0.1", "1.0.2"));
        assert!(!version_greater("1.0.2", "v1.0.2"));
    }
}
