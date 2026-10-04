// LG webOS TV controller.
//
// The worker owns the WebSocket and every network wait. Tauri commands submit
// work to the serialized worker and await a reply when the caller needs the
// completed result, so the main window is never blocked by an offline TV. The
// webOS client key stays inside this module and is never serialized into
// AppConfig or emitted to the frontend.

use crate::config::TvDeviceConfig;
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use native_tls::TlsConnector;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{
    connect_async, connect_async_tls_with_config, Connector, MaybeTlsStream, WebSocketStream,
};

const SECURE_PORT: u16 = 3001;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PAIRING_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CREDENTIAL_BLOB_SIZE: usize = 5 * 512;
const RECONNECT_DELAYS: [Duration; 4] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
];
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);
const SSDP_MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1900;
const SSDP_SEARCH_TARGET: &str = "urn:lge-com:service:webos-second-screen:1";
const MAX_DISCOVERED_TVS: usize = 16;
const DEFAULT_TV_DISCOVERY_NAME: &str = "LG webOS TV";
const PICTURE_MODE_CONTEXT_KEY: &str = "pictureMode";
const PICTURE_MODE_LUNA_URI: &str = "luna://com.webos.settingsservice/setSystemSettings";
const PICTURE_SETTINGS_CONTEXT: &str = "picture";
const TV_RESTART_QUICK_START_KEY: &str = "quickStartMode";
const TV_RESTART_WOL_KEY: &str = "wolwowlOnOff";

// Picture settings use the same direct and notification alert fallback chain
// as picture mode because this firmware refuses the direct setter for paired
// third-party clients. The backlight key maps to the user facing OLED Pixel
// Brightness control. LG labels the picture category's numeric color setting
// as Color Depth, while webOS stores it under the key color. The frontend
// renders this array across two columns, so each left column control is
// followed by its matching right column control.
const PICTURE_SETTING_SPECS: &[TvPictureSettingSpec] = &[
    TvPictureSettingSpec {
        id: "backlight",
        label: "OLED Pixel Brightness",
        key: "backlight",
        kind: TvPictureSettingKind::Number,
        min: 0,
        max: 100,
    },
    // webOS stores the color temperature as a signed offset, so negative
    // values are valid here.
    TvPictureSettingSpec {
        id: "colorTemperature",
        label: "Color Temperature",
        key: "colorTemperature",
        kind: TvPictureSettingKind::Number,
        min: -50,
        max: 50,
    },
    TvPictureSettingSpec {
        id: "contrast",
        label: "Contrast",
        key: "contrast",
        kind: TvPictureSettingKind::Number,
        min: 0,
        max: 100,
    },
    TvPictureSettingSpec {
        id: "color",
        label: "Color Depth",
        key: "color",
        kind: TvPictureSettingKind::Number,
        min: 0,
        max: 100,
    },
    TvPictureSettingSpec {
        id: "brightness",
        label: "Black Level",
        key: "brightness",
        kind: TvPictureSettingKind::Number,
        min: 0,
        max: 100,
    },
    TvPictureSettingSpec {
        id: "colorGamut",
        label: "Color Gamut",
        key: "colorGamut",
        kind: TvPictureSettingKind::ColorGamut,
        min: 0,
        max: 0,
    },
    TvPictureSettingSpec {
        id: "hdrDynamicToneMapping",
        label: "Dynamic Tone Mapping",
        key: "hdrDynamicToneMapping",
        kind: TvPictureSettingKind::Dtm,
        min: 0,
        max: 0,
    },
];

const DTM_SETTING_VALUES: &[&str] = &["off", "on", "HGIG"];
const COLOR_GAMUT_SETTING_VALUES: &[&str] = &["auto", "native", "dynamic", "adobeRGB"];
const PICTURE_MODE_SETTLE_DELAY: Duration = Duration::from_millis(750);
const TV_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const PICTURE_MODE_READBACK_ATTEMPTS: usize = 5;
const PICTURE_MODE_READBACK_DELAY: Duration = Duration::from_millis(300);
const PICTURE_ALERT_ENTER_DELAY: Duration = Duration::from_millis(150);
const PICTURE_ALERT_ENTER_SETTLE: Duration = Duration::from_millis(200);
const POINTER_SOCKET_TIMEOUT: Duration = Duration::from_secs(2);
const TV_RESTART_POWER_OFF_DELAY: Duration = Duration::from_secs(5);
const TV_RESTART_WOL_PACKET_COUNT: usize = 3;
const TV_RESTART_WOL_PACKET_GAP: Duration = Duration::from_millis(100);
const TV_LOG_VALUE_LIMIT: usize = 160;
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(0x4f4c4400);

fn reconnect_delay(attempt_index: usize) -> Duration {
    RECONNECT_DELAYS
        .get(attempt_index)
        .copied()
        .unwrap_or_else(|| {
            *RECONNECT_DELAYS
                .last()
                .expect("reconnect delays are configured")
        })
}

#[cfg(target_os = "windows")]
fn credential_target(device_id: &str) -> String {
    let sanitized: String = device_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    format!("OLEDHelper:LGWebOS:{sanitized}")
}

#[cfg(target_os = "windows")]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(target_os = "windows")]
pub fn store_client_key(device_id: &str, client_key: &str) -> Result<(), String> {
    use std::ptr;
    use windows::core::PWSTR;
    use windows::Win32::Security::Credentials::{
        CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    if client_key.is_empty() {
        return Err("The TV returned an empty pairing key".to_string());
    }
    let mut target = wide(&credential_target(device_id));
    let mut user_name = wide("OLED Helper");
    let key_size = client_key.encode_utf16().count() * 2;
    if key_size > MAX_CREDENTIAL_BLOB_SIZE {
        return Err("The TV pairing key is too large for Windows Credential Manager".to_string());
    }
    let mut key_bytes = [0u8; MAX_CREDENTIAL_BLOB_SIZE];
    for (index, unit) in client_key.encode_utf16().enumerate() {
        let start = index * 2;
        key_bytes[start..start + 2].copy_from_slice(&unit.to_ne_bytes());
    }
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(target.as_mut_ptr()),
        CredentialBlobSize: key_size as u32,
        CredentialBlob: key_bytes.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        Attributes: ptr::null_mut(),
        UserName: PWSTR(user_name.as_mut_ptr()),
        ..Default::default()
    };
    let result = unsafe { CredWriteW(&credential, 0) };
    key_bytes.fill(0);
    result.map_err(|error| {
        format!("Windows Credential Manager could not store the TV pairing: {error}")
    })
}

#[cfg(target_os = "windows")]
pub fn load_client_key(device_id: &str) -> Result<Option<String>, String> {
    use std::{ptr, slice};
    use windows::core::PWSTR;
    use windows::Win32::Security::Credentials::{
        CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC,
    };

    let mut target = wide(&credential_target(device_id));
    let mut credential: *mut CREDENTIALW = ptr::null_mut();
    if let Err(error) = unsafe {
        CredReadW(
            PWSTR(target.as_mut_ptr()),
            CRED_TYPE_GENERIC,
            None,
            &mut credential,
        )
    } {
        if error.code() == windows::Win32::Foundation::ERROR_NOT_FOUND.to_hresult() {
            return Ok(None);
        }
        return Err(format!(
            "Windows Credential Manager could not read the TV pairing: {error}"
        ));
    }
    if credential.is_null() {
        return Ok(None);
    }
    let result = (|| {
        let credential = unsafe { &*credential };
        if credential.CredentialBlob.is_null() || credential.CredentialBlobSize == 0 {
            return Err("Windows Credential Manager returned an empty TV pairing".to_string());
        }
        if credential.CredentialBlobSize as usize > MAX_CREDENTIAL_BLOB_SIZE {
            return Err("Windows Credential Manager returned an oversized TV pairing".to_string());
        }
        let bytes = unsafe {
            slice::from_raw_parts(
                credential.CredentialBlob,
                credential.CredentialBlobSize as usize,
            )
        };
        if bytes.len() % 2 != 0 {
            return Err("Windows Credential Manager returned an invalid TV pairing".to_string());
        }
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16(&units)
            .map_err(|_| "Windows Credential Manager returned an invalid TV pairing".to_string())
    })();
    unsafe {
        CredFree(credential.cast());
    }
    result.map(Some)
}

#[cfg(target_os = "windows")]
pub fn delete_credential(device_id: &str) -> Result<(), String> {
    use windows::core::PWSTR;
    use windows::Win32::Security::Credentials::{CredDeleteW, CRED_TYPE_GENERIC};

    let mut target = wide(&credential_target(device_id));
    if let Err(error) = unsafe { CredDeleteW(PWSTR(target.as_mut_ptr()), CRED_TYPE_GENERIC, None) }
    {
        if error.code() == windows::Win32::Foundation::ERROR_NOT_FOUND.to_hresult() {
            return Ok(());
        }
        return Err(format!(
            "Windows Credential Manager could not remove the TV pairing: {error}"
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn store_client_key(_device_id: &str, _client_key: &str) -> Result<(), String> {
    Err("Persistent TV pairing is only available on Windows".to_string())
}

#[cfg(not(target_os = "windows"))]
pub fn load_client_key(_device_id: &str) -> Result<Option<String>, String> {
    Ok(None)
}

#[cfg(not(target_os = "windows"))]
pub fn delete_credential(_device_id: &str) -> Result<(), String> {
    Ok(())
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TvStateSnapshot {
    pub state: String,
    pub device_id: String,
    pub name: String,
    pub host: String,
    pub endpoint: String,
    pub paired: bool,
    pub credential_saved: bool,
    pub pending: bool,
    pub volume: Option<u32>,
    pub muted: Option<bool>,
    pub input_id: String,
    pub input_label: String,
    pub picture_mode: Option<String>,
    pub picture_modes: Vec<TvPictureModeOption>,
    pub picture_settings: Vec<TvPictureSettingState>,
    pub picture_settings_error: String,
    pub picture_error: String,
    pub picture_write_error: String,
    pub error: String,
}

impl Default for TvStateSnapshot {
    fn default() -> Self {
        Self {
            state: "disconnected".to_string(),
            device_id: String::new(),
            name: String::new(),
            host: String::new(),
            endpoint: String::new(),
            paired: false,
            credential_saved: false,
            pending: false,
            volume: None,
            muted: None,
            input_id: String::new(),
            input_label: String::new(),
            picture_mode: None,
            picture_modes: Vec::new(),
            picture_settings: Vec::new(),
            picture_settings_error: String::new(),
            picture_error: String::new(),
            picture_write_error: String::new(),
            error: String::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TvDiscoveryCandidate {
    pub device_id: String,
    pub name: String,
    pub host: String,
    pub model: String,
    pub model_number: String,
    pub software_version: String,
    pub screen_type: String,
    pub resolution: String,
    pub width_px: u32,
    pub height_px: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TvDiscoverySnapshot {
    pub state: String,
    pub candidates: Vec<TvDiscoveryCandidate>,
    pub error: String,
}

impl Default for TvDiscoverySnapshot {
    fn default() -> Self {
        Self {
            state: "idle".to_string(),
            candidates: Vec::new(),
            error: String::new(),
        }
    }
}

pub struct TvControllerState {
    snapshot: Mutex<TvStateSnapshot>,
    discovery: Mutex<TvDiscoverySnapshot>,
    sender: Mutex<Option<mpsc::Sender<WorkerCommand>>>,
}

impl Default for TvControllerState {
    fn default() -> Self {
        Self {
            snapshot: Mutex::new(TvStateSnapshot::default()),
            discovery: Mutex::new(TvDiscoverySnapshot::default()),
            sender: Mutex::new(None),
        }
    }
}

enum WorkerCommand {
    Discover,
    Pair {
        host: String,
        name: String,
    },
    ReconnectSaved {
        device: TvDeviceConfig,
        client_key: Option<String>,
        credential_saved: bool,
    },
    Forget {
        reply: Option<oneshot::Sender<Result<(), String>>>,
    },
    Reset,
    Disconnect,
    AdjustVolume(i32),
    ToggleMute,
    Restart {
        reply: oneshot::Sender<Result<(), String>>,
    },
    RefreshPictureContext {
        reply: oneshot::Sender<Result<TvStateSnapshot, String>>,
    },
    SetPictureMode {
        mode: String,
        reply: Option<oneshot::Sender<Result<TvStateSnapshot, String>>>,
    },
    SetPictureSetting {
        setting_id: String,
        value: String,
        // Optional, like SetPictureMode: a hotkey binding queues the write
        // with nobody waiting on a reply.
        reply: Option<oneshot::Sender<Result<TvStateSnapshot, String>>>,
    },
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TvPictureModeOption {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TvPictureSettingKind {
    Number,
    Dtm,
    ColorGamut,
}

impl TvPictureSettingKind {
    fn as_str(&self) -> &'static str {
        match self {
            TvPictureSettingKind::Number => "number",
            TvPictureSettingKind::Dtm => "dtm",
            TvPictureSettingKind::ColorGamut => "colorGamut",
        }
    }
}

pub struct TvPictureSettingSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub key: &'static str,
    pub kind: TvPictureSettingKind,
    pub min: i64,
    pub max: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TvPictureSettingState {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub min: i64,
    pub max: i64,
    pub value: Option<String>,
    pub supported: bool,
}

const PICTURE_MODE_OPTIONS: &[(&str, &str)] = &[
    ("vivid", "Vivid"),
    ("normal", "Standard"),
    ("cinema", "Cinema"),
    ("game", "Game"),
    ("sports", "Sports"),
    ("filmMaker", "Filmmaker Mode"),
    ("hdrVivid", "HDR Vivid"),
    ("hdrStandard", "HDR Standard"),
    ("hdrCinema", "HDR Cinema"),
    ("hdrCinemaBright", "HDR Cinema Home"),
    ("hdrGame", "HDR Game"),
    ("hdrFilmMaker", "HDR Filmmaker Mode"),
    ("dolbyHdrVivid", "Dolby Vision Vivid"),
    ("dolbyHdrStandard", "Dolby Vision Standard"),
    ("dolbyHdrCinema", "Dolby Vision Cinema"),
    ("dolbyHdrCinemaBright", "Dolby Vision Cinema Home"),
    ("dolbyHdrGame", "Dolby Vision Game"),
];

const HDR_PICTURE_MODE_IDS: &[&str] = &[
    "hdrVivid",
    "hdrStandard",
    "hdrCinema",
    "hdrCinemaBright",
    "hdrGame",
    "hdrFilmMaker",
];

const DOLBY_HDR_PICTURE_MODE_IDS: &[&str] = &[
    "dolbyHdrVivid",
    "dolbyHdrStandard",
    "dolbyHdrCinema",
    "dolbyHdrCinemaBright",
    "dolbyHdrGame",
];

#[derive(Clone, Default)]
struct PictureContext {
    input_id: String,
    input_label: String,
    picture_mode: Option<String>,
    picture_modes: Vec<TvPictureModeOption>,
    picture_settings: Vec<TvPictureSettingState>,
    picture_settings_error: String,
    error: String,
}

struct ConnectedTv {
    socket: Socket,
    // Kept private and in Rust memory. The key is never exposed by a state
    // snapshot, event, log record, or AppConfig field.
    client_key: Option<String>,
    credential_saved: bool,
    host: String,
    name: String,
    device_id: String,
    endpoint: String,
    // SHA-256 of the TV's TLS leaf certificate, learned during the explicit
    // first pairing and persisted with the pairing. Every later connection
    // (reconnect, recovery, restart, and the remote input socket) must present
    // the same certificate, so a device that replaces the TV at that address
    // is rejected instead of receiving the pairing key and the user's control
    // requests.
    certificate_fingerprint: String,
    picture_context: PictureContext,
}

pub fn shutdown(app: &AppHandle) {
    let state = app.state::<TvControllerState>();
    let sender = state
        .sender
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(sender) = sender.as_ref() {
        let _ = sender.try_send(WorkerCommand::Shutdown);
    }
}

pub fn restore_saved(app: &AppHandle, device: Option<TvDeviceConfig>) -> Result<(), String> {
    let Some(device) = device else {
        return Ok(());
    };
    if !device.paired || !device.auto_reconnect {
        publish(
            app,
            saved_device_snapshot(
                &device,
                if device.paired { "disconnected" } else { "pairing_required" },
            ),
        );
        return Ok(());
    }
    publish(app, saved_device_snapshot(&device, "connecting"));
    let sender = ensure_worker(app)?;
    sender
        .try_send(WorkerCommand::ReconnectSaved {
            device,
            client_key: None,
            credential_saved: true,
        })
        .map_err(|_| "TV controller is shutting down".to_string())
}

pub async fn forget_and_wait(app: &AppHandle) -> Result<(), String> {
    let sender = ensure_worker(app)?;
    let (reply, receiver) = oneshot::channel();
    sender
        .try_send(WorkerCommand::Forget { reply: Some(reply) })
        .map_err(|_| "TV controller is shutting down".to_string())?;
    receiver
        .await
        .map_err(|_| "TV controller stopped before forgetting the TV".to_string())?
}

pub fn queue_reset(app: &AppHandle) -> Result<(), String> {
    let sender = ensure_worker(app)?;
    sender
        .try_send(WorkerCommand::Reset)
        .map_err(|_| "TV controller is shutting down".to_string())
}

pub fn queue_action(app: &AppHandle, action: &str) -> Result<(), String> {
    let command = match action {
        "decrease_tv_volume" => WorkerCommand::AdjustVolume(-1),
        "increase_tv_volume" => WorkerCommand::AdjustVolume(1),
        "toggle_tv_mute" => WorkerCommand::ToggleMute,
        _ => return Err(format!("unknown TV action '{action}'")),
    };
    let sender = ensure_worker(app)?;
    sender
        .try_send(command)
        .map_err(|_| "TV controller is shutting down".to_string())
}

pub fn queue_picture_mode(app: &AppHandle, mode: &str) -> Result<(), String> {
    let mode = validate_picture_mode(mode.trim())?;
    let sender = ensure_worker(app)?;
    sender
        .try_send(WorkerCommand::SetPictureMode { mode, reply: None })
        .map_err(|_| "TV controller is shutting down".to_string())
}

/// Queues a picture setting write with no caller waiting on the result, the
/// tray side of a picture setting hotkey. The setting id and value are
/// validated here, so a stale binding cannot reach the TV with garbage.
pub fn queue_picture_setting(app: &AppHandle, setting_id: &str, value: &str) -> Result<(), String> {
    let spec = picture_setting_spec(setting_id.trim())?;
    let value = validate_picture_setting_value(spec, value)?;
    let sender = ensure_worker(app)?;
    sender
        .try_send(WorkerCommand::SetPictureSetting {
            setting_id: spec.id.to_string(),
            value,
            reply: None,
        })
        .map_err(|_| "TV controller is shutting down".to_string())
}

fn ensure_worker(app: &AppHandle) -> Result<mpsc::Sender<WorkerCommand>, String> {
    let state = app.state::<TvControllerState>();
    let mut sender = state
        .sender
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(existing) = sender.as_ref() {
        return Ok(existing.clone());
    }

    let (tx, rx) = mpsc::channel(32);
    let worker_app = app.clone();
    thread::Builder::new()
        .name("tv-controller".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    log::error!("[tv] could not start worker runtime: {error}");
                    return;
                }
            };
            runtime.block_on(worker_loop(worker_app, rx));
        })
        .map_err(|error| format!("could not start TV controller worker: {error}"))?;
    *sender = Some(tx.clone());
    Ok(tx)
}

async fn worker_loop(app: AppHandle, mut receiver: mpsc::Receiver<WorkerCommand>) {
    let mut connection: Option<ConnectedTv> = None;
    let mut pending_command: Option<WorkerCommand> = None;
    let mut resume_reconnect: Option<ReconnectRequest> = None;
    let mut restore_quick_start_after_reconnect = false;
    'worker_loop: loop {
        let command = match pending_command.take() {
            Some(command) => command,
            None if resume_reconnect.is_some() => {
                let reconnect = resume_reconnect
                    .take()
                    .expect("resume request was checked");
                WorkerCommand::ReconnectSaved {
                    device: reconnect.device,
                    client_key: Some(reconnect.client_key),
                    credential_saved: reconnect.credential_saved,
                }
            }
            None if connection.is_some() => {
                tokio::select! {
                    command = receiver.recv() => match command {
                        Some(command) => command,
                        None => break 'worker_loop,
                    },
                    _ = tokio::time::sleep(TV_KEEPALIVE_INTERVAL) => {
                        let Some(mut current) = connection.take() else {
                            continue 'worker_loop;
                        };
                        match read_volume(&mut current.socket).await {
                            Ok(_) => {
                                connection = Some(current);
                            }
                            Err(error) => {
                                log::warn!(
                                    "[tv] idle keepalive failed detail={}",
                                    sanitized_error_detail(&error)
                                );
                                if current.client_key.is_some() {
                                    let (recovered, interrupted, stop) =
                                        recover_lost_connection(&app, current, &mut receiver).await;
                                    if stop {
                                        break 'worker_loop;
                                    }
                                    if let Some((next, resume)) = interrupted {
                                        pending_command = Some(next);
                                        resume_reconnect = resume;
                                    }
                                    connection = recovered;
                                } else {
                                    publish(
                                        &app,
                                        TvStateSnapshot {
                                            state: "offline".to_string(),
                                            error: stable_error(&error),
                                            ..TvStateSnapshot::default()
                                        },
                                    );
                                }
                            }
                        }
                        continue 'worker_loop;
                    }
                }
            }
            None => match receiver.recv().await {
                Some(command) => command,
                None => break 'worker_loop,
            },
        };
        match command {
            WorkerCommand::Discover => {
                // Discovery is deliberately cancellable. Pair, disconnect, and
                // shutdown commands can interrupt the short SSDP wait without
                // blocking the rest of the controller.
                let result = tokio::select! {
                    result = discover_ssdp() => Some(result),
                    next = receiver.recv() => {
                        pending_command = Some(next.unwrap_or(WorkerCommand::Shutdown));
                        publish_discovery(&app, TvDiscoverySnapshot {
                            state: "canceled".to_string(),
                            ..TvDiscoverySnapshot::default()
                        });
                        None
                    }
                };
                let Some(result) = result else {
                    continue;
                };
                match result {
                    Ok(candidates) => {
                        publish_discovery(
                            &app,
                            TvDiscoverySnapshot {
                                state: "complete".to_string(),
                                candidates,
                                error: String::new(),
                            },
                        );
                    }
                    Err(error) => {
                        publish_discovery(
                            &app,
                            TvDiscoverySnapshot {
                                state: "error".to_string(),
                                candidates: Vec::new(),
                                error,
                            },
                        );
                    }
                }
            }
            WorkerCommand::Pair { host, name } => {
                connection = None;
                // An explicit Pair is the trust decision. The certificate the
                // TV presents during this handshake is the one that gets
                // pinned, so a first pairing cannot be hijacked afterwards and
                // re-pairing is the way to adopt a new certificate.
                let result = tokio::select! {
                    result = connect_and_register(&app, host, name, None, false, "connecting", "") => Some(result),
                    next = receiver.recv() => {
                        pending_command = Some(next.unwrap_or(WorkerCommand::Shutdown));
                        None
                    }
                };
                let Some(result) = result else {
                    continue;
                };
                match result {
                    Ok(mut connected) => {
                        let persistence_error = persist_connection(&app, &mut connected);
                        let restore_error = if restore_quick_start_after_reconnect {
                            match restore_tv_quick_start(&mut connected).await {
                                Ok(()) => {
                                    restore_quick_start_after_reconnect = false;
                                    log::info!("[tv] Quick Start+ restored after pairing");
                                    None
                                }
                                Err(error) => {
                                    log::warn!(
                                        "[tv] Quick Start+ could not be restored after pairing detail={}",
                                        sanitized_error_detail(&error)
                                    );
                                    Some(
                                        "The TV connected, but Quick Start+ could not be restored"
                                            .to_string(),
                                    )
                                }
                            }
                        } else {
                            None
                        };
                        let volume = read_volume(&mut connected.socket).await.ok();
                        let muted = read_mute(&mut connected.socket).await.ok();
                        connected
                            .refresh_picture_context("initial_picture_context")
                            .await;
                        publish(
                            &app,
                            connected.snapshot(volume, muted, persistence_error.or(restore_error)),
                        );
                        connection = Some(connected);
                    }
                    Err(failure) => {
                        let state = if failure.authentication_rejected {
                            "authentication_rejected"
                        } else {
                            "offline"
                        };
                        publish(&app, failure.snapshot(state));
                    }
                }
            }
            WorkerCommand::ReconnectSaved {
                device,
                client_key: in_memory_key,
                credential_saved: in_memory_credential_saved,
            } => {
                connection = None;
                let (client_key, credential_saved) = if let Some(key) = in_memory_key {
                    (key, in_memory_credential_saved)
                } else {
                    if !device.paired || !device.auto_reconnect {
                        publish(
                            &app,
                            saved_device_snapshot(
                                &device,
                                if device.paired { "disconnected" } else { "pairing_required" },
                            ),
                        );
                        continue;
                    }
                    match load_client_key(&device.device_id) {
                        Ok(Some(key)) => (key, true),
                        Ok(None) => {
                            let mut device = device;
                            device.paired = false;
                            device.auto_reconnect = false;
                            let _ = crate::config::update_tv_device(&app, Some(device.clone()));
                            publish(&app, saved_device_snapshot(&device, "pairing_required"));
                            continue;
                        }
                        Err(error) => {
                            log::warn!("[tv] could not load the saved TV pairing: {error}");
                            let mut device = device;
                            device.paired = false;
                            device.auto_reconnect = false;
                            let _ = crate::config::update_tv_device(&app, Some(device.clone()));
                            publish(&app, saved_device_snapshot(&device, "pairing_required"));
                            continue;
                        }
                    }
                };
                match reconnect_saved(&app, device, client_key, credential_saved, &mut receiver).await {
                    Ok(mut connected) => {
                        let restore_error = if restore_quick_start_after_reconnect {
                            match restore_tv_quick_start(&mut connected).await {
                                Ok(()) => {
                                    restore_quick_start_after_reconnect = false;
                                    log::info!("[tv] Quick Start+ restored after reconnect");
                                    None
                                }
                                Err(error) => {
                                    log::warn!(
                                        "[tv] Quick Start+ could not be restored after reconnect detail={}",
                                        sanitized_error_detail(&error)
                                    );
                                    Some("The TV reconnected, but Quick Start+ could not be restored".to_string())
                                }
                            }
                        } else {
                            None
                        };
                        let volume = read_volume(&mut connected.socket).await.ok();
                        let muted = read_mute(&mut connected.socket).await.ok();
                        connected
                            .refresh_picture_context("reconnect_picture_context")
                            .await;
                        let mut snapshot = connected.snapshot(volume, muted, restore_error);
                        snapshot.picture_write_error = String::new();
                        publish(&app, snapshot);
                        connection = Some(connected);
                    }
                    Err(ReconnectStop::Command { next, resume }) => {
                        // Reprocess the interrupting command through the normal
                        // worker path. Ordinary actions do not abandon retries,
                        // while commands that replace or disable the pairing do.
                        pending_command = Some(next);
                        resume_reconnect = resume;
                    }
                    Err(ReconnectStop::PairingRequired) => {
                        if let Some(mut device) = crate::config::saved_tv_device() {
                            device.paired = false;
                            device.auto_reconnect = false;
                            let _ = crate::config::update_tv_device(&app, Some(device.clone()));
                            publish(&app, saved_device_snapshot(&device, "pairing_required"));
                        }
                    }
                    Err(ReconnectStop::Shutdown) => break,
                }
            }
            WorkerCommand::Forget { reply } => {
                connection = None;
                let result = forget_from_worker(&app);
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                } else if let Err(error) = result {
                    log::warn!("[tv] forget failed: {error}");
                }
            }
            WorkerCommand::Reset => {
                connection = None;
                publish(&app, TvStateSnapshot::default());
                publish_discovery(&app, TvDiscoverySnapshot::default());
            }
            WorkerCommand::Disconnect => {
                connection = None;
                if let Some(mut device) = crate::config::saved_tv_device() {
                    device.auto_reconnect = false;
                    let _ = crate::config::update_tv_device(&app, Some(device.clone()));
                    publish(&app, saved_device_snapshot(&device, "disconnected"));
                } else {
                    publish(&app, TvStateSnapshot::default());
                }
            }
            WorkerCommand::AdjustVolume(delta) => {
                let Some(mut current) = connection.take() else {
                    publish(
                        &app,
                        TvStateSnapshot {
                            state: "offline".to_string(),
                            pending: false,
                            error: "Connect and pair with the TV first".to_string(),
                            ..TvStateSnapshot::default()
                        },
                    );
                    continue;
                };
                let mut pending = current.snapshot(None, None, None);
                pending.pending = true;
                publish(&app, pending);
                let uri = if delta < 0 {
                    "ss://audio/volumeDown"
                } else {
                    "ss://audio/volumeUp"
                };
                let result =
                    send_request(&mut current.socket, uri, json!({}), "adjust_volume").await;
                let request_error = result.err().map(|error| stable_error(&error));
                let request_failed = request_error.is_some();
                let volume = if request_failed {
                    None
                } else {
                    read_volume(&mut current.socket).await.ok()
                };
                let muted = if request_failed {
                    None
                } else {
                    read_mute(&mut current.socket).await.ok()
                };
                if request_failed && current.client_key.is_some() {
                    let (recovered, interrupted, stop) =
                        recover_lost_connection(&app, current, &mut receiver).await;
                    if stop {
                        break;
                    }
                    if let Some((next, resume)) = interrupted {
                        pending_command = Some(next);
                        resume_reconnect = resume;
                        continue;
                    }
                    connection = recovered;
                } else {
                    publish(&app, current.snapshot(volume, muted, request_error));
                    connection = Some(current);
                }
            }
            WorkerCommand::ToggleMute => {
                let Some(mut current) = connection.take() else {
                    publish(
                        &app,
                        TvStateSnapshot {
                            state: "offline".to_string(),
                            pending: false,
                            error: "Connect and pair with the TV first".to_string(),
                            ..TvStateSnapshot::default()
                        },
                    );
                    continue;
                };
                let mut pending = current.snapshot(None, None, None);
                pending.pending = true;
                publish(&app, pending);
                let result = async {
                    let response = send_request(
                        &mut current.socket,
                        "ss://audio/getMute",
                        json!({}),
                        "read_mute_for_toggle",
                    )
                    .await?;
                    let muted = parse_bool(&response, "muted").unwrap_or(false);
                    send_request(
                        &mut current.socket,
                        "ss://audio/setMute",
                        json!({ "muted": !muted }),
                        "toggle_mute",
                    )
                    .await
                }
                .await;
                let request_error = result.err().map(|error| stable_error(&error));
                let request_failed = request_error.is_some();
                let muted = if request_failed {
                    None
                } else {
                    read_mute(&mut current.socket).await.ok()
                };
                let volume = if request_failed {
                    None
                } else {
                    read_volume(&mut current.socket).await.ok()
                };
                if request_failed && current.client_key.is_some() {
                    let (recovered, interrupted, stop) =
                        recover_lost_connection(&app, current, &mut receiver).await;
                    if stop {
                        break;
                    }
                    if let Some((next, resume)) = interrupted {
                        pending_command = Some(next);
                        resume_reconnect = resume;
                        continue;
                    }
                    connection = recovered;
                } else {
                    publish(&app, current.snapshot(volume, muted, request_error));
                    connection = Some(current);
                }
            }
            WorkerCommand::Restart { reply } => {
                let Some(mut current) = connection.take() else {
                    let error = "Connect and pair with the TV first".to_string();
                    publish(
                        &app,
                        TvStateSnapshot {
                            state: "offline".to_string(),
                            pending: false,
                            error: error.clone(),
                            ..TvStateSnapshot::default()
                        },
                    );
                    let _ = reply.send(Err(error));
                    continue;
                };
                let Some(client_key) = current.client_key.clone() else {
                    let error = "Pair and save this TV before restarting it".to_string();
                    publish(&app, current.snapshot(None, None, Some(error.clone())));
                    let _ = reply.send(Err(error));
                    connection = Some(current);
                    continue;
                };
                let credential_saved = current.credential_saved;
                let mut device = device_from_connection(&current);
                device.paired = credential_saved;
                device.auto_reconnect = true;
                let quick_start = match read_tv_system_setting(
                    &mut current.socket,
                    "option",
                    TV_RESTART_QUICK_START_KEY,
                    "read_quick_start_for_restart",
                )
                .await
                {
                    Ok(value) => value,
                    Err(error) => {
                        let error =
                            format!("Could not read Quick Start+ before restarting: {error}");
                        publish(&app, current.snapshot(None, None, Some(error.clone())));
                        let _ = reply.send(Err(error));
                        connection = Some(current);
                        continue;
                    }
                };
                let quick_start_enabled = match parse_tv_switch(&quick_start, "Quick Start+") {
                    Ok(value) => value,
                    Err(error) => {
                        publish(&app, current.snapshot(None, None, Some(error.clone())));
                        let _ = reply.send(Err(error));
                        connection = Some(current);
                        continue;
                    }
                };
                let wake_on_lan = match read_tv_system_setting(
                    &mut current.socket,
                    "network",
                    TV_RESTART_WOL_KEY,
                    "read_wake_on_lan_for_restart",
                )
                .await
                {
                    Ok(value) => value,
                    Err(error) => {
                        let error =
                            format!("Could not read Wake on LAN before restarting: {error}");
                        publish(&app, current.snapshot(None, None, Some(error.clone())));
                        let _ = reply.send(Err(error));
                        connection = Some(current);
                        continue;
                    }
                };
                let wake_on_lan_enabled = match parse_tv_switch(&wake_on_lan, "Wake on LAN") {
                    Ok(value) => value,
                    Err(error) => {
                        publish(&app, current.snapshot(None, None, Some(error.clone())));
                        let _ = reply.send(Err(error));
                        connection = Some(current);
                        continue;
                    }
                };
                if !wake_on_lan_enabled {
                    let error = "The TV reports Wake on LAN as disabled. On C4, enable All settings > General > Devices > External Devices > TV On With Mobile > Turn on via Wi-Fi, then retry Restart".to_string();
                    publish(&app, current.snapshot(None, None, Some(error.clone())));
                    let _ = reply.send(Err(error));
                    connection = Some(current);
                    continue;
                }
                let mac_addresses = match read_tv_network_addresses(&mut current.socket).await {
                    Ok(addresses) => addresses,
                    Err(error) => {
                        let error =
                            format!("Could not read the TV network address for Restart: {error}");
                        publish(&app, current.snapshot(None, None, Some(error.clone())));
                        let _ = reply.send(Err(error));
                        connection = Some(current);
                        continue;
                    }
                };
                if quick_start_enabled {
                    if let Err(error) =
                        set_tv_quick_start(&mut current, false, "TV restart preparation").await
                    {
                        let error = format!("Could not disable Quick Start+ for Restart: {error}");
                        publish(&app, current.snapshot(None, None, Some(error.clone())));
                        let _ = reply.send(Err(error));
                        connection = Some(current);
                        continue;
                    }
                    restore_quick_start_after_reconnect = true;
                    log::info!("[tv] Quick Start+ disabled for restart");
                }
                publish(&app, saved_device_snapshot(&device, "reconnecting"));
                if let Err(error) = request_tv_turn_off(&mut current.socket).await {
                    if restore_quick_start_after_reconnect {
                        if restore_tv_quick_start(&mut current).await.is_ok() {
                            restore_quick_start_after_reconnect = false;
                        }
                    }
                    let error = format!("Could not send the TV power off request: {error}");
                    publish(&app, current.snapshot(None, None, Some(error.clone())));
                    let _ = reply.send(Err(error));
                    connection = Some(current);
                    continue;
                }
                log::info!("[tv] restart power off request sent");
                drop(current);
                tokio::time::sleep(TV_RESTART_POWER_OFF_DELAY).await;
                if let Err(error) = send_wake_on_lan(&mac_addresses).await {
                    let error = format!(
                        "The TV was powered off, but OLED Helper could not send its wake packet: {error}. Quick Start+ will be restored after the TV reconnects"
                    );
                    log::warn!(
                        "[tv] restart wake packet failed detail={}",
                        sanitized_error_detail(&error)
                    );
                    publish(
                        &app,
                        saved_device_snapshot_with_error(&device, "offline", error.clone()),
                    );
                    let _ = reply.send(Err(error));
                    continue;
                }
                log::info!("[tv] restart wake packet sent");
                let _ = reply.send(Ok(()));
                match reconnect_saved(&app, device, client_key, credential_saved, &mut receiver).await {
                    Ok(mut reconnected) => {
                        let restore_error = if restore_quick_start_after_reconnect {
                            match restore_tv_quick_start(&mut reconnected).await {
                                Ok(()) => {
                                    restore_quick_start_after_reconnect = false;
                                    log::info!("[tv] Quick Start+ restored after restart");
                                    None
                                }
                                Err(error) => {
                                    log::warn!(
                                        "[tv] Quick Start+ could not be restored after restart detail={}",
                                        sanitized_error_detail(&error)
                                    );
                                    Some(
                                        "The TV restarted, but Quick Start+ could not be restored"
                                            .to_string(),
                                    )
                                }
                            }
                        } else {
                            None
                        };
                        let volume = read_volume(&mut reconnected.socket).await.ok();
                        let muted = read_mute(&mut reconnected.socket).await.ok();
                        reconnected
                            .refresh_picture_context("restart_picture_context")
                            .await;
                        publish(&app, reconnected.snapshot(volume, muted, restore_error));
                        connection = Some(reconnected);
                    }
                    Err(ReconnectStop::Command { next, resume }) => {
                        pending_command = Some(next);
                        resume_reconnect = resume;
                    }
                    Err(ReconnectStop::PairingRequired) => {
                        if let Some(mut saved) = crate::config::saved_tv_device() {
                            saved.paired = false;
                            saved.auto_reconnect = false;
                            let _ = crate::config::update_tv_device(&app, Some(saved.clone()));
                            publish(&app, saved_device_snapshot(&saved, "pairing_required"));
                        }
                    }
                    Err(ReconnectStop::Shutdown) => break,
                }
            }
            WorkerCommand::RefreshPictureContext { reply } => {
                let Some(mut current) = connection.take() else {
                    let snapshot = TvStateSnapshot {
                        state: "offline".to_string(),
                        pending: false,
                        error: "Connect and pair with the TV first".to_string(),
                        ..TvStateSnapshot::default()
                    };
                    save_snapshot(&app, &snapshot);
                    let _ = reply.send(Ok(snapshot));
                    continue;
                };
                current
                    .refresh_picture_context("automatic_picture_context_refresh")
                    .await;
                // The refresh swallows per key failures into the context
                // errors, so a socket the TV dropped is only visible there.
                // One cheap read tells a slow TV from a dead one: alive it
                // answers in milliseconds, dead it fails and triggers the
                // reconnect below.
                if is_picture_transport_failure(&current.picture_context.error)
                    && current.client_key.is_some()
                {
                    log::warn!(
                        "[tv] status refresh found the connection dead detail={}",
                        sanitized_error_detail(&current.picture_context.error)
                    );
                    let (recovered, interrupted, stop) =
                        recover_lost_connection(&app, current, &mut receiver).await;
                    if stop {
                        break;
                    }
                    if let Some((next, resume)) = interrupted {
                        pending_command = Some(next);
                        resume_reconnect = resume;
                        let _ = reply.send(Err(
                            "The TV connection was lost while refreshing the status".to_string(),
                        ));
                        continue;
                    }
                    match recovered {
                            Some(current) => {
                                // Recovery already refreshed the picture
                                // context, so reuse it for this poll response.
                                let (volume, muted) = cached_volume_mute(&app);
                                let cached_write_error = app
                                    .state::<TvControllerState>()
                                    .snapshot
                                    .lock()
                                    .ok()
                                    .map(|snapshot| snapshot.picture_write_error.clone())
                                    .unwrap_or_default();
                                let mut snapshot = current.snapshot(volume, muted, None);
                                snapshot.picture_write_error = cached_write_error;
                                save_snapshot(&app, &snapshot);
                                let _ = reply.send(Ok(snapshot));
                                connection = Some(current);
                            }
                            None => {
                                let _ = reply.send(Err(
                                    "The TV connection was lost and could not be restored yet"
                                        .to_string(),
                                ));
                            }
                    }
                    continue;
                }
                let (volume, muted) = cached_volume_mute(&app);
                let cached_write_error = app
                    .state::<TvControllerState>()
                    .snapshot
                    .lock()
                    .ok()
                    .map(|snapshot| snapshot.picture_write_error.clone())
                    .unwrap_or_default();
                let mut snapshot = current.snapshot(volume, muted, None);
                snapshot.picture_write_error = cached_write_error;
                // Polling returns through the command reply and updates the
                // backend cache only. It must not publish a global TV event,
                // because that would wake the app shell and its menu effects.
                save_snapshot(&app, &snapshot);
                let _ = reply.send(Ok(snapshot));
                connection = Some(current);
            }
            WorkerCommand::SetPictureMode { mode, reply } => {
                let Some(mut current) = connection.take() else {
                    let error = "Connect and pair with the TV first".to_string();
                    publish(
                        &app,
                        TvStateSnapshot {
                            state: "offline".to_string(),
                            pending: false,
                            error: error.clone(),
                            ..TvStateSnapshot::default()
                        },
                    );
                    if let Some(reply) = reply {
                        let _ = reply.send(Err(error));
                    }
                    continue;
                };
                let mut pending = current.snapshot(None, None, None);
                pending.pending = true;
                pending.picture_write_error = String::new();
                // Keep the shell asleep during a mode write, the same as the
                // setting write and the status poll.
                save_snapshot(&app, &pending);
                current
                    .refresh_picture_context("pre_write_picture_context")
                    .await;
                let write_started = Instant::now();
                let input_before_write = current.picture_context.input_id.clone();
                let mode_before_write = current.picture_context.picture_mode.clone();
                log::info!(
                    "[tv] picture mode write requested mode={} input_id={} current_mode={}",
                    sanitized_log_value(&mode),
                    sanitized_log_value(&input_before_write),
                    mode_before_write
                        .as_deref()
                        .map(sanitized_log_value)
                        .unwrap_or_else(|| "unavailable".to_string())
                );
                let write_result = if picture_mode_allowed_for_context(
                    &mode,
                    current.picture_context.picture_mode.as_deref(),
                ) {
                    let endpoint = current.endpoint.clone();
                    let certificate_fingerprint = current.certificate_fingerprint.clone();
                    set_picture_mode(
                        &mut current.socket,
                        &mode,
                        &endpoint,
                        &certificate_fingerprint,
                    )
                    .await
                } else {
                    let error =
                        "Choose a picture mode for the current TV picture context".to_string();
                    log::warn!(
                        "[tv] picture mode write blocked mode={} current_mode={}",
                        sanitized_log_value(&mode),
                        current
                            .picture_context
                            .picture_mode
                            .as_deref()
                            .map(sanitized_log_value)
                            .unwrap_or_else(|| "unavailable".to_string())
                    );
                    Err(error)
                };
                match &write_result {
                    Ok(()) => log::info!(
                        "[tv] picture mode write request completed mode={} elapsed_ms={}",
                        sanitized_log_value(&mode),
                        elapsed_ms(write_started)
                    ),
                    Err(error) => log::warn!(
                        "[tv] picture mode write request failed mode={} elapsed_ms={} detail={}",
                        sanitized_log_value(&mode),
                        elapsed_ms(write_started),
                        sanitized_error_detail(error)
                    ),
                }
                // A mode write can be the request that finds the socket the
                // TV already dropped (a 5s request timeout with no answer is
                // the tell, a live TV answers in milliseconds). Recover the
                // connection here so the arm below does not walk readbacks
                // into a dead socket, and requeue the write so it lands on
                // the fresh connection.
                if write_result
                    .as_ref()
                    .err()
                    .is_some_and(|error| is_picture_transport_failure(error))
                    && current.client_key.is_some()
                {
                        let (recovered, interrupted, stop) =
                            recover_lost_connection(&app, current, &mut receiver).await;
                        if stop {
                            break;
                        }
                        if let Some((next, resume)) = interrupted {
                            pending_command = Some(next);
                            resume_reconnect = resume;
                            continue;
                        }
                        if let Some(recovered) = recovered {
                            log::info!(
                                "[tv] picture mode write requeued after connection recovery mode={}",
                                sanitized_log_value(&mode)
                            );
                            connection = Some(recovered);
                            pending_command = Some(WorkerCommand::SetPictureMode { mode, reply });
                        } else if let Some(reply) = reply {
                            let _ = reply.send(Err(
                                "The TV connection was lost and could not be restored yet"
                                    .to_string(),
                            ));
                        }
                        continue;
                }
                if write_result.is_ok() {
                    tokio::time::sleep(PICTURE_MODE_SETTLE_DELAY).await;
                }
                current
                    .refresh_picture_context("post_write_picture_context")
                    .await;
                if write_result.is_ok() {
                    current.wait_for_picture_mode(&mode).await;
                }
                let (volume, muted) = cached_volume_mute(&app);
                let write_error = match &write_result {
                    Ok(()) => picture_mode_write_error(&current.picture_context, &mode),
                    Err(error) => Some(error.clone()),
                };
                if let Some(error) = write_error.as_deref() {
                    log::warn!(
                        "[tv] picture mode readback mismatch mode={} detail={}",
                        sanitized_log_value(&mode),
                        sanitized_error_detail(error)
                    );
                } else if write_result.is_ok() {
                    log::info!(
                        "[tv] picture mode readback matched mode={} input_id={}",
                        sanitized_log_value(&mode),
                        sanitized_log_value(&current.picture_context.input_id)
                    );
                }
                let caller_result = match &write_error {
                    Some(error) => Err(error.clone()),
                    None => Ok(()),
                };
                let mut snapshot = current.snapshot(volume, muted, None);
                snapshot.picture_write_error = write_error.unwrap_or_default();
                // A command reply carries the post write snapshot so the TV tab
                // can merge the fresh mode and setting values into its local
                // state without a global event waking the app shell.
                save_snapshot(&app, &snapshot);
                if let Some(reply) = reply {
                    let _ = reply.send(caller_result.map(|()| snapshot));
                }
                connection = Some(current);
            }
            WorkerCommand::SetPictureSetting {
                setting_id,
                value,
                reply,
            } => {
                let Some(mut current) = connection.take() else {
                    let error = "Connect and pair with the TV first".to_string();
                    publish(
                        &app,
                        TvStateSnapshot {
                            state: "offline".to_string(),
                            pending: false,
                            error: error.clone(),
                            ..TvStateSnapshot::default()
                        },
                    );
                    if let Some(reply) = reply {
                        let _ = reply.send(Err(error));
                    }
                    continue;
                };
                let spec = match picture_setting_spec(&setting_id) {
                    Ok(spec) => spec,
                    Err(error) => {
                        if let Some(reply) = reply {
                            let _ = reply.send(Err(error));
                        }
                        connection = Some(current);
                        continue;
                    }
                };
                // Dynamic Tone Mapping exists in HDR picture modes only. The TV
                // tab hides its control outside them, so a hotkey bound to one
                // of its values has to be refused the same way here instead of
                // writing a value the TV will not honor.
                if spec.kind == TvPictureSettingKind::Dtm
                    && !current
                        .picture_context
                        .picture_mode
                        .as_deref()
                        .is_some_and(is_hdr_picture_mode_id)
                {
                    let error =
                        "Dynamic Tone Mapping is only available in HDR picture modes".to_string();
                    log::info!("[tv] picture setting write skipped: {error}");
                    if let Some(reply) = reply {
                        let _ = reply.send(Err(error));
                    }
                    connection = Some(current);
                    continue;
                }
                let safe_setting = sanitized_log_value(&setting_id);
                let safe_value = sanitized_log_value(&value);
                let mut pending = current.snapshot(None, None, None);
                pending.pending = true;
                save_snapshot(&app, &pending);
                log::info!(
                    "[tv] picture setting write requested setting={} value={}",
                    safe_setting,
                    safe_value
                );
                let write_started = Instant::now();
                let endpoint = current.endpoint.clone();
                let certificate_fingerprint = current.certificate_fingerprint.clone();
                let write_result = set_picture_setting(
                    &mut current.socket,
                    spec,
                    &value,
                    &endpoint,
                    &certificate_fingerprint,
                )
                .await;
                match &write_result {
                    Ok(()) => log::info!(
                        "[tv] picture setting write request completed setting={} elapsed_ms={}",
                        safe_setting,
                        elapsed_ms(write_started)
                    ),
                    Err(error) => log::warn!(
                        "[tv] picture setting write request failed setting={} elapsed_ms={} detail={}",
                        safe_setting,
                        elapsed_ms(write_started),
                        sanitized_error_detail(error)
                    ),
                }
                // Same dead socket check as the mode write: a 5s request
                // timeout means the TV stopped answering, so recover and
                // requeue once instead of walking the readbacks into a dead
                // connection and reporting a misleading mismatch.
                if write_result
                    .as_ref()
                    .err()
                    .is_some_and(|error| is_picture_transport_failure(error))
                    && current.client_key.is_some()
                {
                        let (recovered, interrupted, stop) =
                            recover_lost_connection(&app, current, &mut receiver).await;
                        if stop {
                            break;
                        }
                        if let Some((next, resume)) = interrupted {
                            pending_command = Some(next);
                            resume_reconnect = resume;
                            continue;
                        }
                        if let Some(recovered) = recovered {
                            log::info!(
                                "[tv] picture setting write requeued after connection recovery setting={} value={}",
                                safe_setting,
                                safe_value
                            );
                            connection = Some(recovered);
                            pending_command = Some(WorkerCommand::SetPictureSetting {
                                setting_id,
                                value,
                                reply,
                            });
                        } else if let Some(reply) = reply {
                            let _ = reply.send(Err(
                                "The TV connection was lost and could not be restored yet"
                                    .to_string(),
                            ));
                        }
                        continue;
                }
                if write_result.is_ok() {
                    tokio::time::sleep(PICTURE_MODE_SETTLE_DELAY).await;
                }
                current
                    .refresh_picture_context("post_setting_picture_context")
                    .await;
                let (volume, muted) = cached_volume_mute(&app);
                // A setting write is verified by comparing what the TV now
                // reports for that key against the requested value. Some keys
                // are clamped or remapped by firmware, so the reported value
                // is surfaced instead of silently assumed.
                let reported_value = current
                    .picture_context
                    .picture_settings
                    .iter()
                    .find(|setting| setting.id == spec.id)
                    .and_then(|setting| setting.value.clone());
                let write_error = match &write_result {
                    Ok(()) => match reported_value.as_deref() {
                        None => Some(
                            "The TV did not report a value for the changed setting".to_string(),
                        ),
                        Some(actual) if actual != value.trim() => Some(format!(
                            "Requested {} {}, but the TV reports {}",
                            spec.label,
                            value.trim(),
                            actual
                        )),
                        Some(_) => None,
                    },
                    Err(error) => Some(error.clone()),
                };
                if let Some(error) = write_error.as_deref() {
                    log::warn!(
                        "[tv] picture setting readback mismatch setting={} detail={}",
                        safe_setting,
                        sanitized_error_detail(error)
                    );
                } else {
                    log::info!(
                        "[tv] picture setting readback matched setting={} value={}",
                        safe_setting,
                        safe_value
                    );
                }
                let caller_result = match &write_error {
                    Some(error) => Err(error.clone()),
                    None => Ok(()),
                };
                let mut snapshot = current.snapshot(volume, muted, None);
                snapshot.picture_write_error = write_error.unwrap_or_default();
                // Like polling, a setting write reports through the command
                // reply and the backend cache only. A global event would wake
                // the app shell and can close menus while the user works.
                save_snapshot(&app, &snapshot);
                if let Some(reply) = reply {
                    let _ = reply.send(caller_result.map(|()| snapshot));
                }
                connection = Some(current);
            }
            WorkerCommand::Shutdown => break,
        }
    }
}

fn publish_discovery(app: &AppHandle, snapshot: TvDiscoverySnapshot) {
    let state = app.state::<TvControllerState>();
    if let Ok(mut current) = state.discovery.lock() {
        *current = snapshot.clone();
    }
    let _ = app.emit("tv-discovery-changed", snapshot);
}

// The local IPv4 address Windows would use for ordinary outbound traffic.
// An SSDP search has to be sent from the address of the LAN the TV is on. A
// wildcard bind (0.0.0.0) does not guarantee that: when a machine also has a
// Mobile Hotspot, a Wi-Fi Direct virtual adapter, or a VPN up, Windows resolves
// the multicast group against whichever of those adapters it prefers, so the
// search leaves from an address the TV has no route back to and the TV never
// answers. Connecting a UDP socket only picks the route, it sends no packet,
// so this probe is free and side effect free.
fn primary_outbound_ipv4() -> Option<Ipv4Addr> {
    let probe = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    probe.connect((Ipv4Addr::new(1, 1, 1, 1), 9)).ok()?;
    match probe.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if is_usable_local_discovery_ip(ip) => Some(ip),
        _ => None,
    }
}

// Addresses a discovery socket may search from, best first: the LAN address
// first, then the wildcard as a fallback so a machine whose outbound address
// is not the one the TV sits on still searches the way it always did.
fn discovery_bind_targets() -> Vec<Ipv4Addr> {
    let mut targets = Vec::new();
    if let Some(ip) = primary_outbound_ipv4() {
        targets.push(ip);
    }
    if !targets.contains(&Ipv4Addr::UNSPECIFIED) {
        targets.push(Ipv4Addr::UNSPECIFIED);
    }
    targets
}

fn is_usable_local_discovery_ip(ip: Ipv4Addr) -> bool {
    !ip.is_loopback()
        && !ip.is_unspecified()
        && !ip.is_multicast()
        && !ip.is_link_local()
        && ip != Ipv4Addr::BROADCAST
}

async fn discover_ssdp() -> Result<Vec<TvDiscoveryCandidate>, String> {
    let targets = discovery_bind_targets();
    let searches = targets.iter().copied().map(ssdp_search_on).collect::<Vec<_>>();
    let results = futures_util::future::join_all(searches).await;
    let mut devices: BTreeMap<String, TvDiscoveryCandidate> = BTreeMap::new();
    let mut responses = 0usize;
    let mut accepted = 0usize;
    let mut last_error: Option<String> = None;
    for (source, result) in targets.iter().copied().zip(results) {
        match result {
            Ok(found) => {
                responses += found.responses;
                accepted += found.accepted;
                for candidate in found.devices.into_values() {
                    if devices.len() < MAX_DISCOVERED_TVS
                        || devices.contains_key(&candidate.device_id)
                    {
                        merge_discovery_candidate(&mut devices, candidate);
                    }
                }
            }
            Err(error) => {
                log::debug!("[tv] ssdp search from {source} failed detail={error}");
                last_error = Some(error);
            }
        }
    }
    if devices.is_empty() && last_error.is_some() {
        return Err(last_error.expect("error was recorded"));
    }
    log::debug!(
        "[tv] ssdp search finished responses={responses} accepted={accepted} devices={}",
        devices.len()
    );

    let mut candidates: Vec<_> = devices.into_values().collect();
    candidates.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| left.host.cmp(&right.host))
    });
    Ok(candidates)
}

struct SsdpSearchResult {
    devices: BTreeMap<String, TvDiscoveryCandidate>,
    responses: usize,
    accepted: usize,
}

async fn ssdp_search_on(bind_ip: Ipv4Addr) -> Result<SsdpSearchResult, String> {
    let socket = UdpSocket::bind((bind_ip, 0))
        .await
        .map_err(|error| format!("Could not open the local network discovery socket: {error}"))?;
    let request = format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: {SSDP_MULTICAST_ADDR}:{SSDP_PORT}\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {SSDP_SEARCH_TARGET}\r\n\r\n"
    );
    // SSDP runs over UDP, which silently drops packets. The specification
    // expects clients to repeat the search, so send it a few times across
    // the wait window instead of once.
    let search_attempts = 3;
    let attempt_gap = DISCOVERY_TIMEOUT / (search_attempts as u32 + 1);
    for attempt in 0..search_attempts {
        socket
            .send_to(request.as_bytes(), (SSDP_MULTICAST_ADDR, SSDP_PORT))
            .await
            .map_err(|error| format!("Could not search the local network for TVs: {error}"))?;
        if attempt + 1 < search_attempts {
            tokio::time::sleep(attempt_gap).await;
        }
    }

    let deadline = tokio::time::Instant::now() + DISCOVERY_TIMEOUT;
    let mut buffer = [0u8; 8192];
    let mut devices: BTreeMap<String, TvDiscoveryCandidate> = BTreeMap::new();
    let mut responses = 0usize;
    let mut accepted = 0usize;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let received = tokio::time::timeout(remaining, socket.recv_from(&mut buffer)).await;
        let (length, source) = match received {
            Ok(Ok(value)) => value,
            Ok(Err(_)) | Err(_) => break,
        };
        responses += 1;
        if let Some((mut candidate, location)) = parse_ssdp_response(&buffer[..length], source) {
            accepted += 1;
            fetch_discovery_details(&location, &mut candidate).await;
            if devices.len() < MAX_DISCOVERED_TVS || devices.contains_key(&candidate.device_id) {
                merge_discovery_candidate(&mut devices, candidate);
            }
        }
    }
    Ok(SsdpSearchResult {
        devices,
        responses,
        accepted,
    })
}

fn merge_discovery_candidate(
    devices: &mut BTreeMap<String, TvDiscoveryCandidate>,
    candidate: TvDiscoveryCandidate,
) {
    let device_id = candidate.device_id.clone();
    let should_replace = devices
        .get(&device_id)
        .map(|existing| {
            (existing.name == DEFAULT_TV_DISCOVERY_NAME
                && candidate.name != DEFAULT_TV_DISCOVERY_NAME)
                || (existing.model.is_empty() && !candidate.model.is_empty())
        })
        .unwrap_or(true);
    if should_replace {
        devices.insert(device_id, candidate);
    }
}

impl Default for TvDiscoveryCandidate {
    fn default() -> Self {
        Self {
            device_id: String::new(),
            name: String::new(),
            host: String::new(),
            model: String::new(),
            model_number: String::new(),
            software_version: String::new(),
            screen_type: String::new(),
            resolution: String::new(),
            width_px: 0,
            height_px: 0,
        }
    }
}

fn parse_ssdp_response(
    response: &[u8],
    source: SocketAddr,
) -> Option<(TvDiscoveryCandidate, String)> {
    let source_ip = match source.ip() {
        IpAddr::V4(ip) if is_valid_discovery_ip(ip) => ip,
        _ => return None,
    };
    let text = std::str::from_utf8(response).ok()?;
    let mut lines = text.lines();
    let status = lines.next()?.trim();
    if !status.starts_with("HTTP/1.") || !status.ends_with(" 200 OK") {
        return None;
    }

    let mut headers = BTreeMap::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once(':')?;
        if key.split_whitespace().count() != 1 {
            return None;
        }
        headers.insert(key.to_ascii_lowercase(), value.trim().to_string());
    }

    let search_target = headers.get("st")?;
    if !search_target.eq_ignore_ascii_case(SSDP_SEARCH_TARGET) {
        return None;
    }
    if headers
        .get("usn")
        .map(|value| value.is_empty())
        .unwrap_or(true)
    {
        return None;
    }
    let location = headers.get("location")?.clone();
    if parse_ssdp_location_host(&location)? != source_ip {
        return None;
    }

    let model =
        clean_discovery_value(headers.get("modelname").map(String::as_str)).unwrap_or_default();
    let name = clean_discovery_value(headers.get("friendlyname").map(String::as_str))
        .or_else(|| clean_discovery_value(headers.get("name").map(String::as_str)))
        .or_else(|| (!model.is_empty()).then(|| model.clone()))
        .unwrap_or_else(|| DEFAULT_TV_DISCOVERY_NAME.to_string());
    Some((
        TvDiscoveryCandidate {
            device_id: format!("lg-{source_ip}"),
            name,
            host: source_ip.to_string(),
            model,
            ..TvDiscoveryCandidate::default()
        },
        location,
    ))
}

// LG's SSDP headers usually omit the model, so the device description XML
// at the response's LOCATION URL is fetched for the full details: friendly
// name, model name and number, screen type, resolution, and software
// version. A short timeout keeps the scan responsive; a TV that does not
// serve the description simply keeps its header only values.
async fn fetch_discovery_details(location: &str, candidate: &mut TvDiscoveryCandidate) {
    let request = reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .build();
    let client = match request {
        Ok(client) => client,
        Err(_) => return,
    };
    let response = match client.get(location).send().await {
        Ok(response) => response,
        Err(_) => return,
    };
    let text = match response.text().await {
        Ok(text) => text,
        Err(_) => return,
    };
    for (tag, field) in [
        ("friendlyName", &mut candidate.name),
        ("modelName", &mut candidate.model),
        ("modelNumber", &mut candidate.model_number),
        ("softwareVersion", &mut candidate.software_version),
        ("screenType", &mut candidate.screen_type),
        ("resolution", &mut candidate.resolution),
    ] {
        if let Some(value) =
            xml_tag_value(&text, tag).and_then(|found| clean_discovery_value(Some(found.as_str())))
        {
            *field = value;
        }
    }
    if let Some((width, height)) = xml_tag_pair(&text, "width", "height") {
        candidate.width_px = width;
        candidate.height_px = height;
    }
    if candidate.name.is_empty() {
        candidate.name = DEFAULT_TV_DISCOVERY_NAME.to_string();
    }
}

fn xml_tag_value(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.to_ascii_lowercase().find(&open.to_ascii_lowercase())?;
    let value_start = start + open.len();
    let remainder = &text[value_start..];
    let end = remainder
        .to_ascii_lowercase()
        .find(&close.to_ascii_lowercase())?;
    Some(remainder[..end].trim().to_string())
}

fn xml_tag_pair(text: &str, first_tag: &str, second_tag: &str) -> Option<(u32, u32)> {
    let first = xml_tag_value(text, first_tag)?.parse().ok()?;
    let second = xml_tag_value(text, second_tag)?.parse().ok()?;
    Some((first, second))
}

fn parse_ssdp_location_host(location: &str) -> Option<Ipv4Addr> {
    let (scheme, rest) = location.split_once("://")?;
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return None;
    }
    let authority = rest
        .split(|character| matches!(character, '/' | '?' | '#'))
        .next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port),
        None => (authority, "80"),
    };
    let port = port.parse::<u16>().ok()?;
    if host.is_empty() || port == 0 {
        return None;
    }
    host.parse::<Ipv4Addr>().ok()
}

fn is_valid_discovery_ip(ip: Ipv4Addr) -> bool {
    !ip.is_unspecified() && !ip.is_loopback() && !ip.is_multicast() && ip != Ipv4Addr::BROADCAST
}

fn clean_discovery_value(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    let filtered: String = value
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    let filtered = filtered.trim();
    if filtered.is_empty() {
        None
    } else {
        Some(filtered.chars().take(96).collect())
    }
}

struct ConnectFailure {
    message: String,
    device_id: String,
    host: String,
    name: String,
    endpoint: String,
    authentication_rejected: bool,
    // The peer presented a certificate that is not the pinned one. Retrying
    // cannot fix it (a different device answered), so the reconnect loop
    // stops and asks for a new pairing instead of backing off forever.
    certificate_pin_rejected: bool,
}

impl ConnectFailure {
    fn new(
        message: impl Into<String>,
        host: impl Into<String>,
        name: impl Into<String>,
        endpoint: impl Into<String>,
        authentication_rejected: bool,
    ) -> Self {
        Self::with_pin_rejection(
            message,
            host,
            name,
            endpoint,
            authentication_rejected,
            false,
        )
    }

    fn with_pin_rejection(
        message: impl Into<String>,
        host: impl Into<String>,
        name: impl Into<String>,
        endpoint: impl Into<String>,
        authentication_rejected: bool,
        certificate_pin_rejected: bool,
    ) -> Self {
        let message = message.into();
        let host = host.into();
        let endpoint = endpoint.into();
        let device_id = host
            .parse::<IpAddr>()
            .map(|host| format!("lg-{host}"))
            .unwrap_or_default();
        log::warn!(
            "[tv] connection failed host={} endpoint={} authentication_rejected={} certificate_pin_rejected={} detail={}",
            sanitized_log_value(&host),
            sanitized_log_value(&endpoint),
            authentication_rejected,
            certificate_pin_rejected,
            sanitized_error_detail(&message)
        );
        Self {
            message,
            device_id,
            host,
            name: name.into(),
            endpoint,
            authentication_rejected,
            certificate_pin_rejected,
        }
    }

    fn snapshot(&self, state: &str) -> TvStateSnapshot {
        TvStateSnapshot {
            state: state.to_string(),
            device_id: self.device_id.clone(),
            name: self.name.clone(),
            host: self.host.clone(),
            endpoint: self.endpoint.clone(),
            paired: false,
            credential_saved: false,
            pending: false,
            error: self.message.clone(),
            ..TvStateSnapshot::default()
        }
    }
}

struct ReconnectRequest {
    device: TvDeviceConfig,
    client_key: String,
    credential_saved: bool,
}

enum ReconnectStop {
    Command {
        next: WorkerCommand,
        resume: Option<ReconnectRequest>,
    },
    PairingRequired,
    Shutdown,
}

// The leaf certificate the TV just presented, as a lowercase hex SHA-256 of
// its DER bytes. Any other digest choice is a config format change, so the
// value is stored with an explicit prefix that keeps old and new pins
// distinguishable and lets a future scheme be added without ambiguity.
fn peer_certificate_fingerprint(socket: &Socket) -> Result<String, String> {
    let MaybeTlsStream::NativeTls(stream) = socket.get_ref() else {
        return Err("The TV connection is not using TLS".to_string());
    };
    let certificate = stream
        .get_ref()
        .peer_certificate()
        .map_err(|error| format!("The TV certificate could not be read: {error}"))?
        .ok_or_else(|| "The TV did not present a certificate".to_string())?;
    let der = certificate
        .to_der()
        .map_err(|error| format!("The TV certificate could not be read: {error}"))?;
    Ok(format!("sha256:{:x}", Sha256::digest(der)))
}

// Normalize a stored fingerprint before comparing: trim, lowercase, drop the
// separators a user or an older build may have written. An empty stored value
// means the pairing predates pinning, which is treated as unpinned (see
// expected_certificate_fingerprint).
fn normalized_fingerprint(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace() && *character != ':')
        .flat_map(char::to_lowercase)
        .collect()
}

// A saved TV can only be pinned once it has a stored fingerprint. Empty means
// a pairing saved before pinning existed, which reconnects treat as unpinned
// (and report as needing a fresh pairing) rather than trusting a new identity
// silently.
fn expected_certificate_fingerprint(device: &TvDeviceConfig) -> String {
    normalized_fingerprint(&device.certificate_fingerprint)
}

fn fingerprints_match(presented: &str, expected: &str) -> bool {
    let expected = normalized_fingerprint(expected);
    !expected.is_empty() && normalized_fingerprint(presented) == expected
}

fn command_cancels_reconnect(command: &WorkerCommand) -> bool {
    matches!(
        command,
        WorkerCommand::Pair { .. }
            | WorkerCommand::Forget { .. }
            | WorkerCommand::Reset
            | WorkerCommand::Disconnect
            | WorkerCommand::Shutdown
    )
}

fn reject_command_while_reconnecting(app: &AppHandle, command: WorkerCommand) {
    let error = "The TV is reconnecting. Try again when it is connected".to_string();
    match command {
        WorkerCommand::Discover => publish_discovery(
            app,
            TvDiscoverySnapshot {
                state: "error".to_string(),
                candidates: Vec::new(),
                error,
            },
        ),
        WorkerCommand::Restart { reply } => {
            let _ = reply.send(Err(error));
        }
        WorkerCommand::RefreshPictureContext { reply } => {
            let _ = reply.send(Err(error));
        }
        WorkerCommand::SetPictureMode { reply, .. }
        | WorkerCommand::SetPictureSetting { reply, .. } => {
            if let Some(reply) = reply {
                let _ = reply.send(Err(error));
            }
        }
        WorkerCommand::AdjustVolume(_) | WorkerCommand::ToggleMute => {
            log::debug!("[tv] volume hotkey skipped while reconnecting");
        }
        WorkerCommand::ReconnectSaved { .. } => {
            log::debug!("[tv] duplicate reconnect request ignored during recovery");
        }
        WorkerCommand::Pair { .. }
        | WorkerCommand::Forget { .. }
        | WorkerCommand::Reset
        | WorkerCommand::Disconnect
        | WorkerCommand::Shutdown => {
            unreachable!("cancellation commands are handled before rejection")
        }
    }
}

async fn connect_and_register(
    app: &AppHandle,
    host: String,
    name: String,
    client_key: Option<&str>,
    credential_saved: bool,
    mode_state: &str,
    expected_fingerprint: &str,
) -> Result<ConnectedTv, ConnectFailure> {
    let parsed = match validate_host(&host) {
        Ok(host) => host,
        Err(error) => return Err(ConnectFailure::new(error, host, name, String::new(), false)),
    };
    let endpoint = format!("wss://{parsed}:{SECURE_PORT}");
    log::info!(
        "[tv] connection starting host={} endpoint={} mode={} saved_pairing_present={}",
        parsed,
        endpoint,
        mode_state,
        client_key.is_some()
    );
    publish(
        app,
        TvStateSnapshot {
            state: mode_state.to_string(),
            name: name.clone(),
            host: parsed.to_string(),
            endpoint: endpoint.clone(),
            paired: client_key.is_some(),
            credential_saved: client_key.is_some(),
            pending: true,
            ..TvStateSnapshot::default()
        },
    );
    let request = match format!("{endpoint}/").into_client_request() {
        Ok(request) => request,
        Err(_) => {
            return Err(ConnectFailure::new(
                "Could not build the TV connection address",
                parsed.to_string(),
                name,
                endpoint,
                false,
            ));
        }
    };
    // LG TVs ship a self signed certificate, so chain and hostname validation
    // cannot succeed. Both exceptions are scoped to this one connector, which
    // is only ever used with the validated numeric host and fixed secure port
    // above, and the presented certificate is pinned below, so the accepted
    // identity is the exact certificate the user paired with rather than any
    // certificate any device on the network can present. The connector is
    // never installed into a process-wide TLS configuration.
    let tls = match TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()
    {
        Ok(tls) => tls,
        Err(_error) => {
            return Err(ConnectFailure::new(
                "Could not initialize the TV secure connection",
                parsed.to_string(),
                name,
                endpoint,
                false,
            ));
        }
    };
    let connect = tokio::time::timeout(
        CONNECT_TIMEOUT,
        connect_async_tls_with_config(request, None, false, Some(Connector::NativeTls(tls))),
    )
    .await;
    let (mut socket, _) = match connect {
        Ok(Ok(socket)) => {
            log::info!("[tv] secure websocket opened host={parsed}");
            socket
        }
        Ok(Err(_)) => {
            return Err(ConnectFailure::new(
                "Could not connect to the TV on the secure port",
                parsed.to_string(),
                name,
                endpoint,
                false,
            ));
        }
        Err(_) => {
            return Err(ConnectFailure::new(
                "The TV did not answer the secure connection in time",
                parsed.to_string(),
                name,
                endpoint,
                false,
            ));
        }
    };

    // Pin check before anything else goes over this socket: the pairing key,
    // the registration manifest, and every later picture-setting write. A
    // mismatch means something other than the paired TV answered at that
    // address, so the connection is dropped and the pairing is not used.
    let certificate_fingerprint = match peer_certificate_fingerprint(&socket) {
        Ok(fingerprint) => fingerprint,
        Err(error) => {
            return Err(ConnectFailure::new(
                error,
                parsed.to_string(),
                name,
                endpoint,
                false,
            ));
        }
    };
    if !expected_fingerprint.is_empty()
        && !fingerprints_match(&certificate_fingerprint, expected_fingerprint)
    {
        log::warn!("[tv] TLS certificate pin mismatch host={parsed}, refusing the connection");
        return Err(ConnectFailure::with_pin_rejection(
            "This address is not the paired TV: its secure certificate does not match. Forget the pairing and pair again with your TV",
            parsed.to_string(),
            name,
            endpoint,
            false,
            true,
        ));
    }
    log::info!(
        "[tv] TLS certificate accepted host={parsed} pinned={}",
        !expected_fingerprint.is_empty()
    );

    // webOS 26 requires the normal hello and pre-registration system-info
    // exchange before it will accept a register message. Older firmware also
    // accepts this sequence, so it is the common path for the first probe.
    log::debug!("[tv] hello exchange starting host={parsed}");
    perform_hello(&mut socket).await.map_err(|error| {
        ConnectFailure::new(
            stable_error(&error),
            parsed.to_string(),
            name.clone(),
            endpoint.clone(),
            false,
        )
    })?;
    log::debug!("[tv] pre registration system info exchange starting host={parsed}");
    get_pre_registration_system_info(&mut socket)
        .await
        .map_err(|error| {
            ConnectFailure::new(
                stable_error(&error),
                parsed.to_string(),
                name.clone(),
                endpoint.clone(),
                false,
            )
        })?;

    if client_key.is_none() {
        publish(
            app,
            TvStateSnapshot {
                state: "awaiting_approval".to_string(),
                name: name.clone(),
                host: parsed.to_string(),
                endpoint: endpoint.clone(),
                pending: true,
                ..TvStateSnapshot::default()
            },
        );
    }
    log::info!(
        "[tv] registration exchange starting host={} saved_pairing_present={}",
        parsed,
        client_key.is_some()
    );
    let response = match register_once(&mut socket, registration_payload(&name, client_key)).await {
        Ok(response) => response,
        Err(error)
            if error
                .to_ascii_lowercase()
                .contains("blacklisted certificate") =>
        {
            match register_once(
                &mut socket,
                unsigned_registration_payload(&name, client_key),
            )
            .await
            {
                Ok(response) => response,
                Err(error) => {
                    let message = stable_error(&error);
                    let authentication_rejected =
                        client_key.is_some() && is_authentication_error(&error);
                    return Err(ConnectFailure::new(
                        message,
                        parsed.to_string(),
                        name,
                        endpoint,
                        authentication_rejected,
                    ));
                }
            }
        }
        Err(error) => {
            let message = stable_error(&error);
            let authentication_rejected = client_key.is_some() && is_authentication_error(&error);
            return Err(ConnectFailure::new(
                message,
                parsed.to_string(),
                name,
                endpoint,
                authentication_rejected,
            ));
        }
    };
    let (paired, returned_key) = match parse_pairing_response(&response) {
        Ok(result) => result,
        Err(error) => {
            let authentication_rejected = is_authentication_error(&error);
            return Err(ConnectFailure::new(
                error,
                parsed.to_string(),
                name,
                endpoint,
                authentication_rejected,
            ));
        }
    };
    if !paired {
        log::warn!("[tv] registration was not approved host={parsed}");
        return Err(ConnectFailure::new(
            "The TV did not approve this pairing request",
            parsed.to_string(),
            name,
            endpoint,
            true,
        ));
    }

    log::info!(
        "[tv] registration accepted host={} credential_present={}",
        parsed,
        returned_key.is_some() || client_key.is_some()
    );
    Ok(ConnectedTv {
        socket,
        client_key: returned_key.or_else(|| client_key.map(str::to_string)),
        credential_saved,
        host: parsed.to_string(),
        name,
        device_id: format!("lg-{parsed}"),
        endpoint,
        certificate_fingerprint,
        picture_context: PictureContext::default(),
    })
}

fn is_authentication_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    [
        "client-key",
        "unauthorized",
        "forbidden",
        "401",
        "403",
        "reject",
        "invalid key",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

async fn reconnect_saved(
    app: &AppHandle,
    device: TvDeviceConfig,
    client_key: String,
    credential_saved: bool,
    receiver: &mut mpsc::Receiver<WorkerCommand>,
) -> Result<ConnectedTv, ReconnectStop> {
    let mut delay_index = 0;
    // A saved pairing without a stored fingerprint predates pinning, so there
    // is no identity to verify. Reconnecting it would silently adopt whatever
    // certificate answers, so the user is asked to pair again instead.
    let expected_fingerprint = expected_certificate_fingerprint(&device);
    if expected_fingerprint.is_empty() {
        publish(
            app,
            saved_device_snapshot_with_error(
                &device,
                "pairing_required",
                "This saved pairing has no saved TV certificate. Pair again on the TV to authorize OLED Helper"
                    .to_string(),
            ),
        );
        return Err(ReconnectStop::PairingRequired);
    }
    loop {
        let attempt = connect_and_register(
            app,
            device.host.clone(),
            device.name.clone(),
            Some(&client_key),
            credential_saved,
            "reconnecting",
            &expected_fingerprint,
        );
        tokio::pin!(attempt);
        let result = loop {
            tokio::select! {
                result = &mut attempt => break result,
                command = receiver.recv() => match command {
                    Some(command) if command_cancels_reconnect(&command) => {
                        return Err(ReconnectStop::Command { next: command, resume: None });
                    }
                    Some(command) => reject_command_while_reconnecting(app, command),
                    None => return Err(ReconnectStop::Shutdown),
                }
            }
        };
        let mut pairing_rejected = false;
        match result {
            Ok(mut connected) => {
                // The IP-derived id is only the original pairing's credential
                // namespace, not the TV's current network identity.
                connected.device_id = device.device_id.clone();
                return Ok(connected);
            }
            Err(failure) if failure.authentication_rejected => {
                pairing_rejected = true;
                publish(
                    app,
                    saved_device_snapshot_with_error(&device, "reconnecting", failure.message),
                );
            }
            // An old DHCP address may now belong to a different device. Search
            // for the saved certificate before concluding that the TV needs a
            // new pairing.
            Err(failure) if failure.certificate_pin_rejected => {
                publish(
                    app,
                    saved_device_snapshot_with_error(&device, "reconnecting", failure.message),
                );
            }
            Err(failure) => {
                publish(
                    app,
                    saved_device_snapshot_with_error(&device, "reconnecting", failure.message),
                );
            }
        }

        // Search after the first failed connection and periodically thereafter.
        // This recovers a changed DHCP address without continuously sending
        // multicast traffic while a TV is offline.
        if delay_index == 0 || delay_index % 3 == 2 {
            match reconnect_from_discovery(
                app,
                &device,
                &client_key,
                credential_saved,
                &expected_fingerprint,
                receiver,
            )
            .await?
            {
                Some(connected) => return Ok(connected),
                None if pairing_rejected => {
                    publish(
                        app,
                        saved_device_snapshot_with_error(
                            &device,
                            "pairing_required",
                            "The TV rejected its saved pairing. Pair with the TV again to authorize OLED Helper".to_string(),
                        ),
                    );
                    return Err(ReconnectStop::PairingRequired);
                }
                None => {}
            }
        } else if pairing_rejected {
            publish(
                app,
                saved_device_snapshot_with_error(
                    &device,
                    "pairing_required",
                    "The TV rejected its saved pairing. Pair with the TV again to authorize OLED Helper".to_string(),
                ),
            );
            return Err(ReconnectStop::PairingRequired);
        }

        let delay = reconnect_delay(delay_index);
        delay_index = delay_index.saturating_add(1);
        publish(
            app,
            saved_device_snapshot_with_error(
                &device,
                "reconnecting",
                format!("The TV is offline. Retrying in {} seconds", delay.as_secs()),
            ),
        );
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                command = receiver.recv() => match command {
                    Some(command) if command_cancels_reconnect(&command) => {
                        return Err(ReconnectStop::Command { next: command, resume: None });
                    }
                    Some(command) => reject_command_while_reconnecting(app, command),
                    None => return Err(ReconnectStop::Shutdown),
                }
            }
        }
    }
}

// If the saved address is stale, scan for LG TVs and let the pinned TLS
// certificate identify the paired device. SSDP names and model strings are
// useful hints, not authentication, so every candidate goes through the same
// certificate check before the stored client key can be sent.
async fn reconnect_from_discovery(
    app: &AppHandle,
    device: &TvDeviceConfig,
    client_key: &str,
    credential_saved: bool,
    expected_fingerprint: &str,
    receiver: &mut mpsc::Receiver<WorkerCommand>,
) -> Result<Option<ConnectedTv>, ReconnectStop> {
    let candidates = tokio::select! {
        result = discover_ssdp() => match result {
            Ok(candidates) => candidates,
            Err(error) => {
                log::debug!("[tv] discovery reconnect scan failed detail={}", sanitized_error_detail(&error));
                Vec::new()
            }
        },
        command = receiver.recv() => match command {
            Some(command) if command_cancels_reconnect(&command) => {
                return Err(ReconnectStop::Command { next: command, resume: None });
            }
            Some(command) => {
                reject_command_while_reconnecting(app, command);
                return Ok(None);
            }
            None => return Err(ReconnectStop::Shutdown),
        }
    };

    for candidate in candidates {
        if candidate.host == device.host {
            continue;
        }
        let attempt = connect_and_register(
            app,
            candidate.host,
            device.name.clone(),
            Some(client_key),
            credential_saved,
            "reconnecting",
            expected_fingerprint,
        );
        tokio::pin!(attempt);
        let result = loop {
            tokio::select! {
                result = &mut attempt => break result,
                command = receiver.recv() => match command {
                    Some(command) if command_cancels_reconnect(&command) => {
                        return Err(ReconnectStop::Command { next: command, resume: None });
                    }
                    Some(command) => reject_command_while_reconnecting(app, command),
                    None => return Err(ReconnectStop::Shutdown),
                }
            }
        };
        match result {
            Ok(mut connected) => {
                // The Credential Manager target is keyed by this saved id.
                // Keep it stable while updating the current network address.
                connected.device_id = device.device_id.clone();
                let updated_device = device_with_discovered_host(&device, &connected.host);
                if let Err(error) = crate::config::update_tv_device(app, Some(updated_device)) {
                    log::warn!("[tv] could not save the TV's new network address: {error}");
                }
                log::info!("[tv] reconnected to the paired TV at its discovered address");
                return Ok(Some(connected));
            }
            Err(failure) if failure.authentication_rejected => {
                publish(
                    app,
                    saved_device_snapshot_with_error(device, "pairing_required", failure.message),
                );
                return Err(ReconnectStop::PairingRequired);
            }
            // A certificate mismatch means this candidate is not the paired
            // TV. Keep searching without ever sending its saved pairing key,
            // including when no match is found in this particular scan.
            Err(failure) if failure.certificate_pin_rejected => {}
            Err(failure) => log::debug!(
                "[tv] discovered TV did not accept the reconnect host={} detail={}",
                sanitized_log_value(&failure.host),
                sanitized_error_detail(&failure.message)
            ),
        }
    }
    Ok(None)
}

fn device_with_discovered_host(device: &TvDeviceConfig, host: &str) -> TvDeviceConfig {
    let mut updated = device.clone();
    updated.host = host.to_string();
    updated.endpoint = format!("wss://{host}:{SECURE_PORT}");
    updated
}

async fn recover_lost_connection(
    app: &AppHandle,
    current: ConnectedTv,
    receiver: &mut mpsc::Receiver<WorkerCommand>,
) -> (Option<ConnectedTv>, Option<(WorkerCommand, Option<ReconnectRequest>)>, bool) {
    let Some(client_key) = current.client_key.clone() else {
        return (Some(current), None, false);
    };
    let credential_saved = current.credential_saved;
    let device = device_from_connection(&current);
    match reconnect_saved(app, device, client_key, credential_saved, receiver).await {
        Ok(mut connected) => {
            let (volume, muted) = cached_volume_mute(app);
            connected
                .refresh_picture_context("recovery_picture_context")
                .await;
            publish(app, connected.snapshot(volume, muted, None));
            (Some(connected), None, false)
        }
        Err(ReconnectStop::Command { next, resume }) => (None, Some((next, resume)), false),
        Err(ReconnectStop::PairingRequired) => {
            if let Some(mut device) = crate::config::saved_tv_device() {
                device.paired = false;
                device.auto_reconnect = false;
                let _ = crate::config::update_tv_device(app, Some(device.clone()));
                publish(app, saved_device_snapshot(&device, "pairing_required"));
            }
            (None, None, false)
        }
        Err(ReconnectStop::Shutdown) => (None, None, true),
    }
}

fn validate_host(host: &str) -> Result<IpAddr, String> {
    let host = host.trim();
    if host.is_empty() {
        return Err("Enter the TV IP address".to_string());
    }
    if host.parse::<IpAddr>().is_ok() {
        Ok(host.parse().expect("host was validated"))
    } else {
        Err("Enter a numeric TV IP address for the first connection".to_string())
    }
}

async fn perform_hello(socket: &mut Socket) -> Result<(), String> {
    let started = Instant::now();
    log::debug!("[tv] hello request started");
    let response = send_json(
        socket,
        json!({ "type": "hello", "payload": {} }),
        REQUEST_TIMEOUT,
    )
    .await?;
    let _ = response;
    let hello = recv_response(socket, REQUEST_TIMEOUT, "hello", None, None).await?;
    if hello.get("type").and_then(Value::as_str) == Some("hello") {
        log::debug!(
            "[tv] hello exchange completed elapsed_ms={} response_type={}",
            elapsed_ms(started),
            response_type(&hello)
        );
        Ok(())
    } else {
        Err("The TV returned an invalid hello response".to_string())
    }
}

async fn get_pre_registration_system_info(socket: &mut Socket) -> Result<(), String> {
    send_request(
        socket,
        "ssap://system/getSystemInfo",
        json!({}),
        "pre_registration_system_info",
    )
    .await?;
    Ok(())
}

async fn register_once(socket: &mut Socket, payload: Value) -> Result<Value, String> {
    let started = Instant::now();
    log::debug!("[tv] registration request started");
    send_json(socket, payload, PAIRING_TIMEOUT).await?;
    let first = recv_response(socket, PAIRING_TIMEOUT, "register", None, None).await?;
    if is_pairing_prompt_ack(&first) {
        log::debug!(
            "[tv] registration prompt acknowledged elapsed_ms={} response_type={}",
            elapsed_ms(started),
            response_type(&first)
        );
        let final_response = recv_response(socket, PAIRING_TIMEOUT, "register", None, None).await?;
        log::debug!(
            "[tv] registration exchange completed elapsed_ms={} response_type={}",
            elapsed_ms(started),
            response_type(&final_response)
        );
        return Ok(final_response);
    }
    log::debug!(
        "[tv] registration exchange completed elapsed_ms={} response_type={}",
        elapsed_ms(started),
        response_type(&first)
    );
    Ok(first)
}

fn registration_payload(name: &str, client_key: Option<&str>) -> Value {
    let mut payload = json!({
        "type": "register",
        "payload": {
            "forcePairing": false,
            "pairingType": "PROMPT",
            "manifest": {
                "manifestVersion": 1,
                "appVersion": "1.1",
                "signed": {
                    "created": "20140509",
                    "appId": "com.lge.test",
                    "vendorId": "com.lge",
                    "localizedAppNames": {
                        "": "LG Remote App",
                        "ko-KR": "리모컨 앱",
                        "zxx-XX": "ЛГ Rэмotэ AПП"
                    },
                    "localizedVendorNames": { "": "LG Electronics" },
                    "permissions": [
                        "TEST_SECURE",
                        "CONTROL_INPUT_TEXT",
                        "CONTROL_MOUSE_AND_KEYBOARD",
                        "READ_INSTALLED_APPS",
                        "READ_LGE_SDX",
                        "READ_NOTIFICATIONS",
                        "SEARCH",
                        "WRITE_SETTINGS",
                        "WRITE_NOTIFICATION_ALERT",
                        "CONTROL_POWER",
                        "READ_CURRENT_CHANNEL",
                        "READ_RUNNING_APPS",
                        "READ_UPDATE_INFO",
                        "UPDATE_FROM_REMOTE_APP",
                        "READ_LGE_TV_INPUT_EVENTS",
                        "READ_TV_CURRENT_TIME"
                    ],
                    "serial": "2f930e2d2cfe083771f68e4fe7bb07"
                },
                "localizedAppNames": { "": name },
                "localizedVendorNames": { "": "OLED Helper" },
                "permissions": [
                    "LAUNCH",
                    "LAUNCH_WEBAPP",
                    "APP_TO_APP",
                    "CLOSE",
                    "TEST_OPEN",
                    "TEST_PROTECTED",
                    "CONTROL_AUDIO",
                    "CONTROL_DISPLAY",
                    "CONTROL_INPUT_JOYSTICK",
                    "CONTROL_INPUT_MEDIA_RECORDING",
                    "CONTROL_INPUT_MEDIA_PLAYBACK",
                    "CONTROL_INPUT_TV",
                    "CONTROL_POWER",
                    "READ_APP_STATUS",
                    "READ_CURRENT_CHANNEL",
                    "READ_INPUT_DEVICE_LIST",
                    "READ_NETWORK_STATE",
                    "READ_RUNNING_APPS",
                    "READ_TV_CHANNEL_LIST",
                    "WRITE_NOTIFICATION_TOAST",
                    "READ_POWER_STATE",
                    "READ_COUNTRY_INFO",
                    "READ_SETTINGS",
                    "CONTROL_TV_SCREEN",
                    "CONTROL_TV_STANDBY",
                    "CONTROL_FAVORITE_GROUP",
                    "CONTROL_USER_INFO",
                    "CHECK_BLUETOOTH_DEVICE",
                    "CONTROL_BLUETOOTH",
                    "CONTROL_TIMER_INFO",
                    "STB_INTERNAL_CONNECTION",
                    "CONTROL_RECORDING",
                    "READ_RECORDING_STATE",
                    "WRITE_RECORDING_LIST",
                    "READ_RECORDING_LIST",
                    "READ_RECORDING_SCHEDULE",
                    "CONTROL_MOUSE_AND_KEYBOARD",
                    "READ_TV_ACR_AUTH_TOKEN",
                    "READ_TV_CONTENT_STATE",
                    "READ_TV_CURRENT_TIME",
                    "ADD_LAUNCHER_CHANNEL",
                    "SET_CHANNEL_SKIP",
                    "RELEASE_CHANNEL_SKIP",
                    "CONTROL_CHANNEL_BLOCK",
                    "DELETE_SELECT_CHANNEL",
                    "CONTROL_CHANNEL_GROUP",
                    "SCAN_TV_CHANNELS",
                    "CONTROL_TV_POWER",
                    "CONTROL_WOL"
                ],
                "signatures": [
                    {
                        "signatureVersion": 1,
                        "signature": "eyJhbGdvcml0aG0iOiJSU0EtU0hBMjU2Iiwia2V5SWQiOiJ0ZXN0LXNpZ25pbmctY2VydCIsInNpZ25hdHVyZVZlcnNpb24iOjF9.hrVRgjCwXVvE2OOSpDZ58hR+59aFNwYDyjQgKk3auukd7pcegmE2CzPCa0bJ0ZsRAcKkCTJrWo5iDzNhMBWRyaMOv5zWSrthlf7G128qvIlpMT0YNY+n/FaOHE73uLrS/g7swl3/qH/BGFG2Hu4RlL48eb3lLKqTt2xKHdCs6Cd4RMfJPYnzgvI4BNrFUKsjkcu+WD4OO2A27Pq1n50cMchmcaXadJhGrOqH5YmHdOCj5NSHzJYrsW0HPlpuAx/ECMeIZYDh6RMqaFM2DXzdKX9NmmyqzJ3o/0lkk/N97gfVRLW5hA29yeAwaCViZNCP8iC9aO0q9fQojoa7NQnAtw=="
                    }
                ]
            }
        }
    });
    if let Some(client_key) = client_key {
        payload
            .pointer_mut("/payload")
            .and_then(Value::as_object_mut)
            .expect("registration payload has an object payload")
            .insert(
                "client-key".to_string(),
                Value::String(client_key.to_string()),
            );
    }
    payload
}

fn unsigned_registration_payload(name: &str, client_key: Option<&str>) -> Value {
    let mut payload = registration_payload(name, client_key);
    if let Some(manifest) = payload
        .pointer_mut("/payload/manifest")
        .and_then(Value::as_object_mut)
    {
        manifest.remove("signed");
        manifest.insert("appVersion".to_string(), Value::String("1.0".to_string()));
        if let Some(permissions) = manifest
            .get_mut("permissions")
            .and_then(Value::as_array_mut)
        {
            for permission in ["CONTROL_INPUT_TEXT", "CONTROL_MOUSE_AND_KEYBOARD"] {
                if !permissions
                    .iter()
                    .any(|value| value.as_str() == Some(permission))
                {
                    permissions.push(Value::String(permission.to_string()));
                }
            }
        }
    }
    payload
}

fn next_request_id() -> String {
    format!("{:08x}", NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed))
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

fn sanitized_log_value(value: &str) -> String {
    let filtered: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_graphic() {
                character
            } else {
                ' '
            }
        })
        .collect();
    let trimmed = filtered.trim();
    if trimmed.is_empty() {
        "empty".to_string()
    } else {
        trimmed.chars().take(TV_LOG_VALUE_LIMIT).collect()
    }
}

fn sanitized_uri(uri: &str) -> String {
    let trimmed = uri.trim();
    if trimmed.len() <= TV_LOG_VALUE_LIMIT
        && trimmed
            .chars()
            .all(|character| character.is_ascii_graphic() && character != '?' && character != '#')
    {
        trimmed.to_string()
    } else {
        "<redacted>".to_string()
    }
}

fn sanitized_error_detail(error: &str) -> String {
    let compact = error.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = compact.to_ascii_lowercase();
    if lower.contains("client-key")
        || lower.contains("client key")
        || lower.contains("clientkey")
        || lower.contains("credential")
        || lower.contains("pairing key")
    {
        return "protected credential detail redacted".to_string();
    }
    let secret_like_token = compact.split_whitespace().any(|token| {
        let token = token.trim_matches(|character: char| {
            matches!(
                character,
                '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ','
            )
        });
        token.chars().count() >= 16
            && token
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-_=+/.".contains(character))
    });
    if secret_like_token {
        return "protected credential detail redacted".to_string();
    }
    sanitized_log_value(&compact)
}

fn response_type(value: &Value) -> String {
    value
        .get("type")
        .and_then(Value::as_str)
        .map(sanitized_log_value)
        .unwrap_or_else(|| "unknown".to_string())
}

fn response_error_detail(value: &Value) -> Option<&str> {
    value
        .pointer("/payload/error")
        .and_then(Value::as_str)
        .or_else(|| value.get("error").and_then(Value::as_str))
}

fn normalize_mac_address(value: &str) -> Option<String> {
    let compact: String = value
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect();
    if compact.len() != 12 {
        return None;
    }
    let bytes = compact
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(pair, 16).ok()
        })
        .collect::<Option<Vec<u8>>>()?;
    let mut normalized = String::with_capacity(17);
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            normalized.push(':');
        }
        normalized.push_str(&format!("{byte:02X}"));
    }
    Some(normalized)
}

fn mac_addresses_from_response(response: &Value) -> Vec<String> {
    let mut addresses = Vec::new();
    for path in [
        "/payload/wiredInfo/macAddress",
        "/payload/wifiInfo/macAddress",
    ] {
        let Some(address) = response.pointer(path).and_then(Value::as_str) else {
            continue;
        };
        let Some(address) = normalize_mac_address(address) else {
            continue;
        };
        if !addresses.contains(&address) {
            addresses.push(address);
        }
    }
    addresses
}

fn magic_packet(address: &str) -> Result<Vec<u8>, String> {
    let address = normalize_mac_address(address)
        .ok_or_else(|| "The TV returned an invalid network address".to_string())?;
    let compact: String = address
        .chars()
        .filter(|character| *character != ':')
        .collect();
    let mut packet = vec![0xff; 6 + 16 * 6];
    for index in 0..16 {
        let start = 6 + index * 6;
        packet[start..start + 6].copy_from_slice(
            &(0..6)
                .map(|offset| u8::from_str_radix(&compact[offset * 2..offset * 2 + 2], 16))
                .collect::<Result<Vec<u8>, _>>()
                .map_err(|_| "The TV returned an invalid network address".to_string())?,
        );
    }
    Ok(packet)
}

async fn send_wake_on_lan(addresses: &[String]) -> Result<(), String> {
    if addresses.is_empty() {
        return Err("The TV did not report a network address for Wake on LAN".to_string());
    }
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .await
        .map_err(|error| format!("Could not open the Wake on LAN socket: {error}"))?;
    socket
        .set_broadcast(true)
        .map_err(|error| format!("Could not enable Wake on LAN broadcasting: {error}"))?;
    let mut sent = false;
    let mut last_error = None;
    for address in addresses {
        let packet = match magic_packet(address) {
            Ok(packet) => packet,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        for _ in 0..TV_RESTART_WOL_PACKET_COUNT {
            match socket.send_to(&packet, (Ipv4Addr::BROADCAST, 9)).await {
                Ok(_) => sent = true,
                Err(error) => {
                    last_error = Some(error.to_string());
                    break;
                }
            }
            tokio::time::sleep(TV_RESTART_WOL_PACKET_GAP).await;
        }
    }
    if sent {
        Ok(())
    } else {
        Err(last_error.unwrap_or_else(|| "The Wake on LAN packet could not be sent".to_string()))
    }
}

fn tv_turn_off_request(request_id: String) -> Value {
    json!({
        "type": "request",
        "id": request_id,
        "uri": "ssap://system/turnOff",
        "payload": {}
    })
}

fn parse_tv_switch(value: &str, label: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "1" | "yes" => Ok(true),
        "off" | "false" | "0" | "no" => Ok(false),
        _ => Err(format!("The TV reported an unknown {label} value")),
    }
}

fn tv_restart_setting_params(value: &str) -> Value {
    json!({
        "category": "option",
        "settings": { TV_RESTART_QUICK_START_KEY: value }
    })
}

async fn read_tv_system_setting(
    socket: &mut Socket,
    category: &str,
    key: &str,
    operation: &'static str,
) -> Result<String, String> {
    let response = send_request(
        socket,
        "ss://settings/getSystemSettings",
        json!({ "category": category, "keys": [key] }),
        operation,
    )
    .await?;
    if !response_succeeded(&response) {
        return Err("The TV rejected the settings read".to_string());
    }
    picture_setting_value_from_response(&response, key)
        .ok_or_else(|| format!("The TV did not report {key}"))
}

async fn read_tv_network_addresses(socket: &mut Socket) -> Result<Vec<String>, String> {
    let response = send_request(
        socket,
        "ssap://com.webos.service.connectionmanager/getinfo",
        json!({}),
        "read_connection_info_for_restart",
    )
    .await?;
    let addresses = mac_addresses_from_response(&response);
    if addresses.is_empty() {
        return Err("The TV did not report a wired or Wi-Fi network address".to_string());
    }
    Ok(addresses)
}

async fn request_tv_turn_off(socket: &mut Socket) -> Result<(), String> {
    let request_id = next_request_id();
    send_json(socket, tv_turn_off_request(request_id), REQUEST_TIMEOUT).await?;
    Ok(())
}

async fn set_tv_quick_start(
    connection: &mut ConnectedTv,
    enabled: bool,
    describe: &'static str,
) -> Result<(), String> {
    let value = if enabled { "on" } else { "off" };
    write_settings_via_alert(
        &mut connection.socket,
        &tv_restart_setting_params(value),
        &connection.endpoint,
        &connection.certificate_fingerprint,
        describe,
    )
    .await?;
    let actual = read_tv_system_setting(
        &mut connection.socket,
        "option",
        TV_RESTART_QUICK_START_KEY,
        "read_quick_start_after_restart_write",
    )
    .await?;
    if parse_tv_switch(&actual, "Quick Start+")? != enabled {
        return Err("The TV did not apply the Quick Start+ change".to_string());
    }
    Ok(())
}

async fn restore_tv_quick_start(connection: &mut ConnectedTv) -> Result<(), String> {
    set_tv_quick_start(connection, true, "TV restart setting restore").await
}

async fn send_json(socket: &mut Socket, value: Value, timeout: Duration) -> Result<Value, String> {
    let mut value = value;
    if let Some(object) = value.as_object_mut() {
        object
            .entry("id")
            .or_insert_with(|| Value::String(next_request_id()));
    }
    let text = serde_json::to_string(&value).map_err(|error| error.to_string())?;
    tokio::time::timeout(timeout, socket.send(Message::Text(text.into())))
        .await
        .map_err(|_| "The TV did not answer in time".to_string())?
        .map_err(|error| stable_error(&error.to_string()))?;
    Ok(value)
}

async fn send_request(
    socket: &mut Socket,
    uri: &str,
    payload: Value,
    operation: &'static str,
) -> Result<Value, String> {
    let started = Instant::now();
    let safe_uri = sanitized_uri(uri);
    let request_id = next_request_id();
    if crate::detailed_logging_enabled() {
        log::debug!("[tv] request {operation} started uri={safe_uri} request_id={request_id}");
    }
    let sent = match send_json(
        socket,
        json!({
            "type": "request",
            "id": request_id,
            "uri": uri,
            "payload": payload
        }),
        REQUEST_TIMEOUT,
    )
    .await
    {
        Ok(sent) => sent,
        Err(error) => {
            log::warn!(
                "[tv] request {operation} failed before response uri={safe_uri} elapsed_ms={} detail={}",
                elapsed_ms(started),
                sanitized_error_detail(&error)
            );
            return Err(error);
        }
    };
    let sent_id = sent
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let result = recv_response(
        socket,
        REQUEST_TIMEOUT,
        operation,
        Some(&safe_uri),
        Some(&sent_id),
    )
    .await;
    match &result {
        Ok(response) if crate::detailed_logging_enabled() => log::debug!(
            "[tv] request {operation} succeeded uri={safe_uri} request_id={sent_id} elapsed_ms={} response_type={}",
            elapsed_ms(started),
            response_type(response)
        ),
        Err(error) => log::warn!(
            "[tv] request {operation} failed uri={safe_uri} request_id={sent_id} elapsed_ms={} detail={}",
            elapsed_ms(started),
            sanitized_error_detail(error)
        ),
        Ok(_) => {}
    }
    result
}

async fn recv_response(
    socket: &mut Socket,
    timeout: Duration,
    operation: &'static str,
    uri: Option<&str>,
    expected_id: Option<&str>,
) -> Result<Value, String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("The TV did not answer in time".to_string());
        }
        let message = tokio::time::timeout(remaining, socket.next())
            .await
            .map_err(|_| "The TV did not answer in time".to_string())?
            .ok_or_else(|| "The TV closed the connection".to_string())?
            .map_err(|error| stable_error(&error.to_string()))?;
        let Message::Text(text) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&text)
            .map_err(|_| "The TV sent an invalid response".to_string())?;
        let response_kind = response_type(&value);
        let response_id = value
            .get("id")
            .and_then(Value::as_str)
            .map(sanitized_log_value);
        let safe_uri = uri.unwrap_or("none");
        if let (Some(expected), Some(actual)) = (expected_id, response_id.as_deref()) {
            if actual != expected {
                if crate::detailed_logging_enabled() {
                    log::debug!(
                        "[tv] response {operation} ignored uri={safe_uri} expected_request_id={expected} received_request_id={actual} response_type={response_kind}"
                    );
                }
                continue;
            }
        }
        if crate::detailed_logging_enabled() {
            log::debug!(
                "[tv] response {operation} received uri={safe_uri} request_id={} response_type={response_kind}",
                response_id.as_deref().unwrap_or("none")
            );
        }
        match value.get("type").and_then(Value::as_str) {
            Some("hello") | Some("response") | Some("registered") => return Ok(value),
            Some("error") => {
                let detail = response_error_detail(&value).unwrap_or("The TV rejected the request");
                log::warn!(
                    "[tv] response {operation} failed uri={safe_uri} response_type={response_kind} detail={}",
                    sanitized_error_detail(detail)
                );
                return Err(detail.to_string());
            }
            _ => {}
        }
    }
}

fn is_pairing_prompt_ack(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("response")
        && value.pointer("/payload/pairingType").is_some()
        && value
            .pointer("/payload/returnValue")
            .and_then(Value::as_bool)
            == Some(true)
}

fn parse_pairing_response(value: &Value) -> Result<(bool, Option<String>), String> {
    let payload_type = value.pointer("/payload/type").and_then(Value::as_str);
    let key = value
        .pointer("/payload/client-key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .map(str::to_string);
    if payload_type == Some("rejected") || value.get("error").is_some() {
        return Ok((false, None));
    }
    if payload_type == Some("registered") || key.is_some() {
        return Ok((true, key));
    }
    if value
        .pointer("/payload/returnValue")
        .and_then(Value::as_bool)
        == Some(false)
    {
        return Err("The TV rejected the pairing request".to_string());
    }
    let payload_keys = value
        .get("payload")
        .and_then(Value::as_object)
        .map(|payload| {
            let mut keys: Vec<_> = payload.keys().cloned().collect();
            keys.sort();
            keys.join(",")
        })
        .unwrap_or_else(|| "none".to_string());
    let payload_type = payload_type.unwrap_or("none");
    Err(format!(
        "The TV returned an unsupported pairing response (payload type: {payload_type}, keys: {payload_keys})"
    ))
}

fn parse_bool(value: &Value, key: &str) -> Option<bool> {
    let nested_key = if key == "muted" { "muteStatus" } else { key };
    value
        .pointer(&format!("/payload/{key}"))
        .or_else(|| value.pointer(&format!("/payload/volumeStatus/{nested_key}")))
        .and_then(Value::as_bool)
}

fn parse_number(value: &Value, key: &str) -> Option<u32> {
    value
        .pointer(&format!("/payload/{key}"))
        .or_else(|| value.pointer(&format!("/payload/volumeStatus/{key}")))
        .and_then(Value::as_u64)
        .map(|value| value.min(100) as u32)
}

async fn read_volume(socket: &mut Socket) -> Result<u32, String> {
    let response = send_request(socket, "ss://audio/getVolume", json!({}), "read_volume").await?;
    parse_number(&response, "volume").ok_or_else(|| "The TV did not report a volume".to_string())
}

async fn read_mute(socket: &mut Socket) -> Result<bool, String> {
    let response = send_request(socket, "ss://audio/getMute", json!({}), "read_mute").await?;
    parse_bool(&response, "muted").ok_or_else(|| "The TV did not report mute state".to_string())
}

async fn read_picture_context(socket: &mut Socket, previous: &PictureContext) -> PictureContext {
    // Keep the last reported values while refreshing. A temporary TV timeout
    // should not blank the controls or make a later mode write think the
    // current picture mode is unknown.
    let mut context = previous.clone();
    context.error.clear();
    context.picture_settings_error.clear();
    let mut errors = Vec::new();

    match send_request(
        socket,
        "ss://com.webos.applicationManager/getForegroundAppInfo",
        json!({}),
        "read_foreground_app",
    )
    .await
    {
        Ok(response) => {
            context.input_id = response
                .pointer("/payload/appId")
                .and_then(Value::as_str)
                .and_then(|value| clean_discovery_value(Some(value)))
                .unwrap_or_default();
        }
        Err(error) => {
            let stable = stable_error(&error);
            errors.push(stable.clone());
            if is_picture_transport_failure(&error) {
                context.error = errors.join("; ");
                return context;
            }
        }
    }

    if !context.input_id.is_empty() {
        match send_request(
            socket,
            "ss://tv/getExternalInputList",
            json!({}),
            "read_external_inputs",
        )
        .await
        {
            Ok(response) => {
                context.input_label = input_label_from_response(&response, &context.input_id);
            }
            Err(error) => {
                let stable = stable_error(&error);
                errors.push(stable.clone());
                if is_picture_transport_failure(&stable) {
                    context.error = errors.join("; ");
                    return context;
                }
            }
        }
    }

    match read_picture_mode(socket).await {
        Ok(mode) => {
            context.picture_mode = Some(mode.clone());
            context.picture_modes = picture_modes_for_mode(&mode);
        }
        Err(error) => {
            let stable = stable_error(&error);
            errors.push(stable.clone());
            if is_picture_transport_failure(&error) {
                context.error = errors.join("; ");
                return context;
            }
        }
    }

    if let Err(error) = read_picture_settings(socket, &mut context).await {
        errors.push(error);
        context.error = errors.join("; ");
        return context;
    }

    if !errors.is_empty() {
        context.error = errors.join("; ");
    }
    context
}

fn picture_setting_keys(picture_mode: Option<&str>) -> Vec<String> {
    let mut keys: Vec<String> = PICTURE_SETTING_SPECS
        .iter()
        .filter(|spec| !is_unsupported_picture_setting_key(picture_mode, spec.key))
        .map(|spec| spec.key.to_string())
        .collect();
    keys.push(PICTURE_MODE_CONTEXT_KEY.to_string());
    keys.sort();
    keys.dedup();
    keys
}

// A setting key rejected in one picture mode can still be valid in another.
// webOS rejects the whole getSystemSettings call when even one key is unknown,
// so remember rejected keys per mode before retrying the remaining keys.
fn unsupported_picture_setting_keys() -> &'static Mutex<Vec<(String, String)>> {
    static UNSUPPORTED: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new();
    UNSUPPORTED.get_or_init(|| Mutex::new(Vec::new()))
}

fn is_unsupported_picture_setting_key(picture_mode: Option<&str>, key: &str) -> bool {
    let picture_mode = picture_mode.unwrap_or_default();
    unsupported_picture_setting_keys()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .any(|(mode, existing)| mode == picture_mode && existing == key)
}

fn remember_unsupported_picture_setting_key(picture_mode: Option<&str>, key: &str) {
    let picture_mode = picture_mode.unwrap_or_default();
    let mut keys = unsupported_picture_setting_keys()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if !keys
        .iter()
        .any(|(mode, existing)| mode == picture_mode && existing == key)
    {
        keys.push((picture_mode.to_string(), key.to_string()));
        log::info!(
            "[tv] picture setting key skipped after rejection mode={} key={key}",
            sanitized_log_value(picture_mode)
        );
    }
}

#[cfg(test)]
fn clear_unsupported_picture_setting_keys() {
    unsupported_picture_setting_keys()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
}

fn picture_setting_value_from_response(response: &Value, key: &str) -> Option<String> {
    response
        .pointer(&format!("/payload/settings/{key}"))
        .or_else(|| response.pointer(&format!("/payload/{key}")))
        .map(|value| match value {
            Value::String(text) => text.trim().to_string(),
            Value::Number(number) => number.to_string(),
            Value::Bool(flag) => flag.to_string(),
            _ => String::new(),
        })
        .filter(|value| !value.is_empty())
}

fn merge_picture_setting_value(response: &mut Value, key: &str, single: &Value) -> bool {
    let Some(value) = picture_setting_value_from_response(single, key) else {
        return false;
    };
    response["payload"]["settings"][key] = Value::String(value);
    true
}

async fn read_picture_setting_keys(socket: &mut Socket, keys: &[String]) -> Result<Value, String> {
    send_request(
        socket,
        "ss://settings/getSystemSettings",
        json!({ "category": PICTURE_SETTINGS_CONTEXT, "keys": keys }),
        "read_picture_settings",
    )
    .await
}

async fn read_picture_settings(
    socket: &mut Socket,
    context: &mut PictureContext,
) -> Result<(), String> {
    // First attempt: the usual combined read of every key not refused in
    // this picture mode. Some setting keys are mode-specific on webOS.
    let picture_mode = context.picture_mode.clone();
    let keys = picture_setting_keys(picture_mode.as_deref());
    let combined = read_picture_setting_keys(socket, &keys).await;
    if let Err(error) = &combined {
        if is_picture_transport_failure(error) {
            context.picture_settings_error =
                "The TV did not respond while reading picture settings".to_string();
            return Err(stable_error(error));
        }
    }
    let mut response = match combined {
        Ok(response) if response_succeeded(&response) => response,
        _ => {
            // Individual key probes are useful only when the TV answered and
            // rejected the combined key list. A transport timeout was handled
            // above and returns immediately without waiting through 8 more
            // request timeouts.
            if crate::detailed_logging_enabled() {
                log::debug!("[tv] combined picture settings read rejected, falling back per key");
            }
            let mut recovered = json!({
                "payload": { "settings": {} }
            });
            let mut any_succeeded = false;
            for key in &keys {
                match read_picture_setting_keys(socket, std::slice::from_ref(key)).await {
                    Ok(single) if response_succeeded(&single) => {
                        any_succeeded = true;
                        if let Some(value) = picture_setting_value_from_response(&single, key) {
                            recovered["payload"]["settings"][key.as_str()] = Value::String(value);
                        }
                    }
                    Ok(_) => {
                        remember_unsupported_picture_setting_key(picture_mode.as_deref(), key)
                    }
                    Err(error) if is_picture_transport_failure(&error) => {
                        context.picture_settings_error =
                            "The TV did not respond while reading picture settings".to_string();
                        return Err(stable_error(&error));
                    }
                    Err(error) => {
                        let stable = stable_error(&error);
                        if can_try_picture_write_fallback(&stable) {
                            remember_unsupported_picture_setting_key(picture_mode.as_deref(), key);
                        } else {
                            context.picture_settings_error =
                                "The TV did not report the picture setting values".to_string();
                            return Ok(());
                        }
                    }
                }
            }
            if !any_succeeded {
                context.picture_settings_error =
                    "The TV did not report the picture setting values".to_string();
                return Ok(());
            }
            recovered
        }
    };

    // Either read path can omit optional controls. Probe their keys directly
    // instead of treating an omission as proof that this mode does not support
    // the setting.
    for spec in PICTURE_SETTING_SPECS.iter().filter(|spec| {
        matches!(spec.key, "colorGamut" | "hdrDynamicToneMapping")
            && (spec.key != "hdrDynamicToneMapping"
                || picture_mode.as_deref().is_some_and(is_hdr_picture_mode_id))
    }) {        if picture_setting_value_from_response(&response, spec.key).is_some()
            || is_unsupported_picture_setting_key(picture_mode.as_deref(), spec.key)
        {
            continue;
        }
        match read_picture_setting_keys(socket, &[spec.key.to_string()]).await {
            Ok(single) if response_succeeded(&single) => {
                merge_picture_setting_value(&mut response, spec.key, &single);
            }
            Ok(_) => remember_unsupported_picture_setting_key(picture_mode.as_deref(), spec.key),
            Err(error) if is_picture_transport_failure(&error) => {
                context.picture_settings_error =
                    "The TV did not respond while reading picture settings".to_string();
                return Err(stable_error(&error));
            }
            Err(error) => {
                let stable = stable_error(&error);
                if can_try_picture_write_fallback(&stable) {
                    remember_unsupported_picture_setting_key(picture_mode.as_deref(), spec.key);
                } else {
                    context.picture_settings_error =
                        "The TV did not report the picture setting values".to_string();
                    return Ok(());
                }
            }
        }
    }

    let mut settings = PICTURE_SETTING_SPECS
        .iter()
        .map(|spec| TvPictureSettingState {
            id: spec.id.to_string(),
            label: spec.label.to_string().trim().to_string(),
            kind: spec.kind.as_str().to_string(),
            min: spec.min,
            max: spec.max,
            value: picture_setting_value_from_response(&response, spec.key),
            supported: !is_unsupported_picture_setting_key(picture_mode.as_deref(), spec.key),
        })
        .collect::<Vec<_>>();
    // A key with no value is unavailable in this mode for the current
    // snapshot, but it is not cached because another mode may report it.
    for setting in &mut settings {
        if setting.value.is_none() {
            setting.supported = false;
        }
    }
    context.picture_settings = settings;
    Ok(())
}

fn is_picture_transport_failure(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    if ["401", "403", "404", "permission", "no such service", "rejected"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return false;
    }
    [
        "did not answer",
        "timed out",
        "timeout",
        "closed the connection",
        "connection reset",
        "connection refused",
        "could not connect",
        "secure certificate",
        "certificate",
        "tls",
        "the tv request failed",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

async fn read_picture_mode(socket: &mut Socket) -> Result<String, String> {
    let response = send_request(
        socket,
        "ss://settings/getSystemSettings",
        json!({ "category": "picture", "keys": [PICTURE_MODE_CONTEXT_KEY] }),
        "read_picture_mode",
    )
    .await?;
    picture_mode_from_response(&response)
        .ok_or_else(|| "The TV did not report a picture mode".to_string())
}

async fn write_picture_setting_direct(
    socket: &mut Socket,
    uri: &str,
    params: &Value,
    request_context: &'static str,
    describe: &'static str,
) -> Result<(), String> {
    let response = send_request(socket, uri, params.clone(), request_context).await?;
    if response_succeeded(&response) {
        Ok(())
    } else {
        let detail = response_error_detail(&response).unwrap_or("returnValue was false");
        log::warn!(
            "[tv] {describe} direct response rejected detail={}",
            sanitized_error_detail(detail)
        );
        Err(format!("The TV rejected the {describe} change"))
    }
}

fn picture_setting_luna_params(key: &str, value: &str) -> Value {
    json!({
        "category": PICTURE_SETTINGS_CONTEXT,
        "settings": { key: value }
    })
}

fn picture_alert_payload(params: &Value) -> Value {
    let close_action = json!({
        "uri": PICTURE_MODE_LUNA_URI,
        "params": params.clone()
    });
    json!({
        "title": "",
        "message": " ",
        "modal": false,
        "buttons": [{
            "label": "",
            "focus": true,
            "buttonType": "ok",
            "onClick": PICTURE_MODE_LUNA_URI,
            "params": params.clone()
        }],
        "onclose": close_action.clone(),
        "onfail": close_action,
        "type": "confirm",
        "isSysReq": true
    })
}

fn picture_silent_alert_payload(params: &Value) -> Value {
    let close_action = json!({
        "uri": PICTURE_MODE_LUNA_URI,
        "params": params.clone()
    });
    json!({
        "message": "Applying...",
        "buttons": [{
            "label": "",
            "onClick": PICTURE_MODE_LUNA_URI,
            "params": params.clone()
        }],
        "onclose": close_action.clone(),
        "onfail": close_action
    })
}

#[cfg(test)]
fn picture_setting_alert_payload(key: &str, value: &str) -> Value {
    picture_alert_payload(&picture_setting_luna_params(key, value))
}

fn response_identifier(value: &Value) -> Option<String> {
    let identifier = match value {
        Value::String(value) => value.trim().to_string(),
        Value::Number(value) => value.to_string(),
        _ => return None,
    };
    (!identifier.is_empty() && identifier.chars().count() <= 160).then_some(identifier)
}

fn alert_id_from_response(response: &Value) -> Option<String> {
    response
        .pointer("/payload/alertId")
        .or_else(|| response.pointer("/payload/alert/id"))
        .or_else(|| response.pointer("/payload/id"))
        .and_then(response_identifier)
}

fn can_try_picture_write_fallback(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    ![
        "closed",
        "connect",
        "certificate",
        "tls",
        "timed out",
        "timeout",
        "did not answer",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn specialized_socket_url(socket_path: &str, endpoint: &str) -> Result<String, String> {
    let socket_path = socket_path.trim();
    if socket_path.starts_with("ws://") || socket_path.starts_with("wss://") {
        return Ok(socket_path.to_string());
    }
    if !socket_path.starts_with('/') {
        return Err("The TV returned an invalid remote input socket".to_string());
    }
    let (scheme, rest) = endpoint
        .split_once("://")
        .ok_or_else(|| "The TV connection has no valid remote input endpoint".to_string())?;
    if !matches!(scheme, "ws" | "wss") {
        return Err("The TV connection has no valid remote input endpoint".to_string());
    }
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty() {
        return Err("The TV connection has no valid remote input endpoint".to_string());
    }
    Ok(format!("{scheme}://{authority}{socket_path}"))
}

async fn open_specialized_socket(url: &str, expected_fingerprint: &str) -> Result<Socket, String> {
    if url.starts_with("wss://") {
        let request = url
            .into_client_request()
            .map_err(|_| "The TV returned an invalid remote input socket".to_string())?;
        // Same self signed exception as the main socket, and the same pin
        // check: this socket carries button presses, so it must be the paired
        // TV and not whatever answers at the address.
        let tls = TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()
            .map_err(|_| "Could not initialize the TV remote input connection".to_string())?;
        let connected = tokio::time::timeout(
            POINTER_SOCKET_TIMEOUT,
            connect_async_tls_with_config(request, None, false, Some(Connector::NativeTls(tls))),
        )
        .await
        .map_err(|_| "The TV did not answer the remote input socket in time".to_string())?
        .map_err(|error| stable_error(&error.to_string()))?;
        let (socket, _) = connected;
        if !expected_fingerprint.is_empty() {
            let presented = peer_certificate_fingerprint(&socket)?;
            if !fingerprints_match(&presented, expected_fingerprint) {
                log::warn!(
                    "[tv] remote input socket certificate pin mismatch, refusing the connection"
                );
                return Err(
                    "This address is not the paired TV: its secure certificate does not match"
                        .to_string(),
                );
            }
        }
        return Ok(socket);
    }

    let request = url
        .into_client_request()
        .map_err(|_| "The TV returned an invalid remote input socket".to_string())?;
    let connected = tokio::time::timeout(POINTER_SOCKET_TIMEOUT, connect_async(request))
        .await
        .map_err(|_| "The TV did not answer the remote input socket in time".to_string())?
        .map_err(|error| stable_error(&error.to_string()))?;
    let (socket, _) = connected;
    Ok(socket)
}

async fn send_remote_button(
    socket: &mut Socket,
    endpoint: &str,
    certificate_fingerprint: &str,
    button: &'static str,
) -> Result<(), String> {
    let response = send_request(
        socket,
        "ssap://com.webos.service.networkinput/getPointerInputSocket",
        json!({}),
        "get_pointer_input_socket",
    )
    .await?;
    let socket_path = response
        .pointer("/payload/socketPath")
        .and_then(Value::as_str)
        .ok_or_else(|| "The TV did not return a remote input socket".to_string())?;
    let socket_url = specialized_socket_url(socket_path, endpoint)?;
    let mut pointer_socket = open_specialized_socket(&socket_url, certificate_fingerprint).await?;
    let message = format!("type:button\nname:{button}\n\n");
    tokio::time::timeout(
        POINTER_SOCKET_TIMEOUT,
        pointer_socket.send(Message::Text(message.into())),
    )
    .await
    .map_err(|_| "The TV did not accept the remote input button in time".to_string())?
    .map_err(|error| stable_error(&error.to_string()))?;
    let _ = tokio::time::timeout(POINTER_SOCKET_TIMEOUT, pointer_socket.close(None)).await;
    log::debug!("[tv] remote input button sent button={button}");
    Ok(())
}

async fn write_settings_via_alert(
    socket: &mut Socket,
    params: &Value,
    endpoint: &str,
    certificate_fingerprint: &str,
    describe: &'static str,
) -> Result<(), String> {
    let create_response = send_request(
        socket,
        "ssap://system.notifications/createAlert",
        picture_alert_payload(params),
        "create_settings_alert",
    )
    .await?;
    if !response_succeeded(&create_response) {
        let detail = response_error_detail(&create_response).unwrap_or("returnValue was false");
        log::warn!(
            "[tv] {describe} alert create response rejected detail={}",
            sanitized_error_detail(detail)
        );
        return Err("The TV rejected the settings alert".to_string());
    }
    let Some(alert_id) = alert_id_from_response(&create_response) else {
        return Err("The TV did not return a settings alert id".to_string());
    };
    log::debug!("[tv] {describe} alert created alert_id_present=true");
    // Some webOS builds execute the onclose handler, while others require the
    // focused alert button to receive ENTER. Try both against the same short
    // lived alert, then let the caller read back the setting when needed.
    tokio::time::sleep(PICTURE_ALERT_ENTER_DELAY).await;
    let enter_result = send_remote_button(socket, endpoint, certificate_fingerprint, "ENTER").await;
    match &enter_result {
        Ok(()) => log::debug!("[tv] {describe} alert enter sent"),
        Err(error) => log::warn!(
            "[tv] {describe} alert enter failed detail={}",
            sanitized_error_detail(error)
        ),
    }
    if enter_result.is_ok() {
        tokio::time::sleep(PICTURE_ALERT_ENTER_SETTLE).await;
    }
    let close_result = send_request(
        socket,
        "ssap://system.notifications/closeAlert",
        json!({ "alertId": alert_id }),
        "close_settings_alert",
    )
    .await;
    let close_succeeded = close_result
        .as_ref()
        .map(response_succeeded)
        .unwrap_or(false);
    if close_succeeded {
        if let Ok(close_response) = &close_result {
            log::debug!(
                "[tv] {describe} alert closed response_type={}",
                response_type(close_response)
            );
        }
    } else {
        let detail = close_result
            .as_ref()
            .err()
            .map(String::as_str)
            .or_else(|| close_result.as_ref().ok().and_then(response_error_detail))
            .unwrap_or("returnValue was false");
        log::warn!(
            "[tv] {describe} alert close response rejected detail={}",
            sanitized_error_detail(detail)
        );
    }
    if enter_result.is_ok() || close_succeeded {
        Ok(())
    } else {
        Err("The TV did not execute the settings alert".to_string())
    }
}

async fn write_settings_via_silent_alert(
    socket: &mut Socket,
    params: &Value,
    describe: &'static str,
) -> Result<(), String> {
    let create_response = send_request(
        socket,
        "ssap://system.notifications/createAlert",
        picture_silent_alert_payload(params),
        "create_silent_settings_alert",
    )
    .await?;
    if !response_succeeded(&create_response) {
        let detail = response_error_detail(&create_response).unwrap_or("returnValue was false");
        log::warn!(
            "[tv] {describe} silent alert create response rejected detail={}",
            sanitized_error_detail(detail)
        );
        return Err("The TV rejected the settings alert".to_string());
    }
    let Some(alert_id) = alert_id_from_response(&create_response) else {
        return Err("The TV did not return a settings alert id".to_string());
    };
    log::debug!("[tv] {describe} silent alert created alert_id_present=true");
    let close_result = send_request(
        socket,
        "ssap://system.notifications/closeAlert",
        json!({ "alertId": alert_id }),
        "close_silent_settings_alert",
    )
    .await;
    if close_result
        .as_ref()
        .map(response_succeeded)
        .unwrap_or(false)
    {
        if let Ok(close_response) = &close_result {
            log::debug!(
                "[tv] {describe} silent alert closed response_type={}",
                response_type(close_response)
            );
        }
        Ok(())
    } else {
        let detail = close_result
            .as_ref()
            .err()
            .map(String::as_str)
            .or_else(|| close_result.as_ref().ok().and_then(response_error_detail))
            .unwrap_or("returnValue was false");
        log::warn!(
            "[tv] {describe} silent alert close response rejected detail={}",
            sanitized_error_detail(detail)
        );
        Err("The TV did not execute the silent settings alert".to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PictureWriteMethod {
    Luna,
    DirectSs,
    SilentAlert,
    VisibleAlert,
}

impl PictureWriteMethod {
    fn as_str(&self) -> &'static str {
        match self {
            PictureWriteMethod::Luna => "luna",
            PictureWriteMethod::DirectSs => "ss",
            PictureWriteMethod::SilentAlert => "alert_silent",
            PictureWriteMethod::VisibleAlert => "alert",
        }
    }
}

const PICTURE_WRITE_METHODS: [PictureWriteMethod; 4] = [
    PictureWriteMethod::Luna,
    PictureWriteMethod::DirectSs,
    PictureWriteMethod::SilentAlert,
    PictureWriteMethod::VisibleAlert,
];

async fn write_picture_setting_with_fallbacks(
    socket: &mut Socket,
    params: &Value,
    endpoint: &str,
    certificate_fingerprint: &str,
    describe: &'static str,
) -> Result<PictureWriteMethod, String> {
    let mut last_error = None;
    for method in PICTURE_WRITE_METHODS {
        let result = match method {
            PictureWriteMethod::Luna => {
                write_picture_setting_direct(
                    socket,
                    PICTURE_MODE_LUNA_URI,
                    params,
                    "write_picture_setting_luna",
                    describe,
                )
                .await
            }
            PictureWriteMethod::DirectSs => {
                write_picture_setting_direct(
                    socket,
                    "ss://settings/setSystemSettings",
                    params,
                    "write_picture_setting_ss",
                    describe,
                )
                .await
            }
            PictureWriteMethod::SilentAlert => {
                write_settings_via_silent_alert(socket, params, describe).await
            }
            PictureWriteMethod::VisibleAlert => {
                write_settings_via_alert(
                    socket,
                    params,
                    endpoint,
                    certificate_fingerprint,
                    describe,
                )
                .await
            }
        };
        match result {
            Ok(()) => return Ok(method),
            Err(error) => {
                log::warn!(
                    "[tv] {describe} write rejected path={} detail={}",
                    method.as_str(),
                    sanitized_error_detail(&error)
                );
                if !can_try_picture_write_fallback(&error) {
                    return Err(stable_error(&error));
                }
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| format!("The TV rejected the {describe} change")))
}

async fn set_picture_mode(
    socket: &mut Socket,
    mode: &str,
    endpoint: &str,
    certificate_fingerprint: &str,
) -> Result<(), String> {
    let mode = validate_picture_mode(mode)?;
    let safe_mode = sanitized_log_value(&mode);
    let params = picture_setting_luna_params(PICTURE_MODE_CONTEXT_KEY, &mode);
    log::debug!("[tv] picture mode write started mode={safe_mode}");
    match write_picture_setting_with_fallbacks(
        socket,
        &params,
        endpoint,
        certificate_fingerprint,
        "picture mode",
    )
    .await
    {
        Ok(method) => {
            log::info!(
                "[tv] picture mode write accepted path={} mode={safe_mode}",
                method.as_str()
            );
            Ok(())
        }
        Err(error) => Err(format!(
            "The TV rejected the picture mode change. {}",
            stable_error(&error)
        )),
    }
}

fn picture_setting_spec(setting_id: &str) -> Result<&'static TvPictureSettingSpec, String> {
    PICTURE_SETTING_SPECS
        .iter()
        .find(|spec| spec.id == setting_id)
        .ok_or_else(|| "Choose a supported picture setting".to_string())
}

fn validate_picture_setting_value(
    spec: &TvPictureSettingSpec,
    value: &str,
) -> Result<String, String> {
    let trimmed = value.trim();
    match spec.kind {
        TvPictureSettingKind::Number => {
            let parsed: i64 = trimmed.parse().map_err(|_| {
                format!("Enter a whole number between {} and {}", spec.min, spec.max)
            })?;
            if !(spec.min..=spec.max).contains(&parsed) {
                return Err(format!(
                    "Enter a whole number between {} and {}",
                    spec.min, spec.max
                ));
            }
            Ok(parsed.to_string())
        }
        TvPictureSettingKind::Dtm => {
            let matches = DTM_SETTING_VALUES.iter().any(|allowed| *allowed == trimmed);
            if matches {
                Ok(trimmed.to_string())
            } else {
                Err("Choose a supported Dynamic Tone Mapping value".to_string())
            }
        }
        TvPictureSettingKind::ColorGamut => {
            let matches = COLOR_GAMUT_SETTING_VALUES
                .iter()
                .any(|allowed| *allowed == trimmed);
            if matches {
                Ok(trimmed.to_string())
            } else {
                Err("Choose a supported Color Gamut value".to_string())
            }
        }
    }
}

async fn set_picture_setting(
    socket: &mut Socket,
    spec: &TvPictureSettingSpec,
    value: &str,
    endpoint: &str,
    certificate_fingerprint: &str,
) -> Result<(), String> {
    let value = validate_picture_setting_value(spec, value)?;
    let describe: &'static str = "picture setting";
    log::debug!(
        "[tv] picture setting write started setting={} value={}",
        sanitized_log_value(spec.id),
        sanitized_log_value(&value)
    );
    let params = picture_setting_luna_params(spec.key, &value);
    match write_picture_setting_with_fallbacks(
        socket,
        &params,
        endpoint,
        certificate_fingerprint,
        describe,
    )
    .await
    {
        Ok(method) => {
            log::info!(
                "[tv] picture setting write accepted path={} setting={} value={}",
                method.as_str(),
                sanitized_log_value(spec.id),
                sanitized_log_value(&value)
            );
            Ok(())
        }
        Err(error) => Err(format!(
            "The TV rejected the picture setting change. {}",
            stable_error(&error)
        )),
    }
}

fn input_label_from_response(response: &Value, input_id: &str) -> String {
    response
        .pointer("/payload/devices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|device| {
            device
                .get("appId")
                .and_then(Value::as_str)
                .is_some_and(|value| value == input_id)
        })
        .and_then(|device| {
            device
                .get("label")
                .or_else(|| device.get("name"))
                .and_then(Value::as_str)
        })
        .and_then(|value| clean_discovery_value(Some(value)))
        .unwrap_or_default()
}

fn picture_mode_from_response(response: &Value) -> Option<String> {
    response
        .pointer(&format!("/payload/settings/{PICTURE_MODE_CONTEXT_KEY}"))
        .or_else(|| response.pointer(&format!("/payload/{PICTURE_MODE_CONTEXT_KEY}")))
        .and_then(Value::as_str)
        .and_then(|value| clean_discovery_value(Some(value)))
}

fn is_hdr_picture_mode_id(mode: &str) -> bool {
    mode.starts_with("hdr") || mode.starts_with("dolbyHdr")
}

fn picture_mode_allowed_for_context(mode: &str, current_mode: Option<&str>) -> bool {
    let Some(current_mode) = current_mode else {
        return false;
    };
    picture_modes_for_mode(current_mode)
        .iter()
        .any(|option| option.id == mode)
}

fn picture_modes_for_mode(current_mode: &str) -> Vec<TvPictureModeOption> {
    let allowed = if current_mode.starts_with("dolbyHdr") {
        DOLBY_HDR_PICTURE_MODE_IDS
    } else if current_mode.starts_with("hdr") {
        HDR_PICTURE_MODE_IDS
    } else {
        return PICTURE_MODE_OPTIONS
            .iter()
            .filter(|(id, _)| !is_hdr_picture_mode_id(id))
            .map(|(id, label)| TvPictureModeOption {
                id: (*id).to_string(),
                label: (*label).to_string(),
            })
            .collect();
    };

    allowed
        .iter()
        .filter_map(|id| {
            PICTURE_MODE_OPTIONS
                .iter()
                .find(|(candidate, _)| candidate == id)
                .map(|(id, label)| TvPictureModeOption {
                    id: (*id).to_string(),
                    label: (*label).to_string(),
                })
        })
        .collect()
}

fn validate_picture_mode(mode: &str) -> Result<String, String> {
    PICTURE_MODE_OPTIONS
        .iter()
        .find(|(id, _)| *id == mode)
        .map(|(id, _)| (*id).to_string())
        .ok_or_else(|| "Choose a supported picture mode".to_string())
}

fn picture_mode_write_error(context: &PictureContext, requested_mode: &str) -> Option<String> {
    let Some(actual_mode) = context.picture_mode.as_deref() else {
        return Some("The TV did not report a picture mode after the change".to_string());
    };
    if actual_mode == requested_mode {
        None
    } else {
        Some(format!(
            "Requested picture mode {requested_mode}, but the TV reports {actual_mode}"
        ))
    }
}

fn response_succeeded(response: &Value) -> bool {
    response
        .pointer("/payload/returnValue")
        .or_else(|| response.pointer("/payload/subscribed"))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

fn saved_device_snapshot(device: &TvDeviceConfig, state: &str) -> TvStateSnapshot {
    let pending = matches!(state, "connecting" | "awaiting_approval" | "reconnecting");
    TvStateSnapshot {
        state: state.to_string(),
        device_id: device.device_id.clone(),
        name: device.name.clone(),
        host: device.host.clone(),
        endpoint: if device.endpoint.is_empty() {
            format!("wss://{}:{}", device.host, SECURE_PORT)
        } else {
            device.endpoint.clone()
        },
        paired: state != "pairing_required" && device.paired,
        credential_saved: state != "pairing_required" && device.paired,
        pending,
        error: String::new(),
        ..TvStateSnapshot::default()
    }
}

fn saved_device_snapshot_with_error(
    device: &TvDeviceConfig,
    state: &str,
    error: String,
) -> TvStateSnapshot {
    let mut snapshot = saved_device_snapshot(device, state);
    snapshot.error = error;
    snapshot
}

fn persist_connection(app: &AppHandle, connection: &mut ConnectedTv) -> Option<String> {
    let previous = crate::config::saved_tv_device();
    if let Some(previous) = previous
        .as_ref()
        .filter(|device| !device.device_id.is_empty() && device.device_id != connection.device_id)
    {
        if let Err(error) = delete_credential(&previous.device_id) {
            log::warn!("[tv] could not remove the previous TV pairing: {error}");
            return Some(
                "Connected, but the previous saved pairing could not be removed".to_string(),
            );
        }
    }

    let mut persistence_error = None;
    if let Some(client_key) = connection.client_key.clone() {
        match store_client_key(&connection.device_id, &client_key) {
            Ok(()) => connection.credential_saved = true,
            Err(error) => {
                log::warn!("[tv] {error}");
                if let Err(cleanup_error) = delete_credential(&connection.device_id) {
                    log::warn!("[tv] could not remove the stale TV pairing: {cleanup_error}");
                }
                persistence_error =
                    Some("Connected, but this pairing could not be saved for restart".to_string());
            }
        }
    }
    let mut device = device_from_connection(connection);
    device.auto_reconnect = true;
    if let Err(error) = crate::config::update_tv_device(app, Some(device)) {
        log::warn!("[tv] could not persist TV metadata: {error}");
        persistence_error = Some("Connected, but the TV metadata could not be saved".to_string());
    }
    persistence_error
}

fn device_from_connection(connection: &ConnectedTv) -> TvDeviceConfig {
    let existing = crate::config::saved_tv_device();
    TvDeviceConfig {
        device_id: connection.device_id.clone(),
        name: connection.name.clone(),
        host: connection.host.clone(),
        endpoint: connection.endpoint.clone(),
        model: existing
            .as_ref()
            .filter(|device| device.device_id == connection.device_id)
            .map(|device| device.model.clone())
            .unwrap_or_default(),
        firmware: existing
            .as_ref()
            .filter(|device| device.device_id == connection.device_id)
            .map(|device| device.firmware.clone())
            .unwrap_or_default(),
        paired: connection.credential_saved,
        auto_reconnect: existing
            .as_ref()
            .filter(|device| device.device_id == connection.device_id)
            .map(|device| device.auto_reconnect)
            .unwrap_or(true),
        // The certificate verified on the live connection, not a previous
        // value: an explicit Pair adopts the newly presented certificate, and
        // every reconnect had to match this one to get here.
        certificate_fingerprint: connection.certificate_fingerprint.clone(),
    }
}

fn forget_from_worker(app: &AppHandle) -> Result<(), String> {
    let saved = crate::config::saved_tv_device();
    let device_id = saved
        .as_ref()
        .map(|device| device.device_id.clone())
        .or_else(|| {
            app.state::<TvControllerState>()
                .snapshot
                .lock()
                .ok()
                .map(|snapshot| snapshot.device_id.clone())
                .filter(|device_id| !device_id.is_empty())
        });
    if let Some(device_id) = device_id {
        if let Err(error) = delete_credential(&device_id) {
            let mut snapshot = app
                .state::<TvControllerState>()
                .snapshot
                .lock()
                .map(|snapshot| snapshot.clone())
                .unwrap_or_default();
            snapshot.error = error.clone();
            publish(app, snapshot);
            return Err(error);
        }
    }
    if let Err(error) = crate::config::update_tv_device(app, None) {
        let mut snapshot = app
            .state::<TvControllerState>()
            .snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_default();
        snapshot.error = error.clone();
        publish(app, snapshot);
        return Err(error);
    }
    publish(app, TvStateSnapshot::default());
    Ok(())
}

fn cached_volume_mute(app: &AppHandle) -> (Option<u32>, Option<bool>) {
    app.state::<TvControllerState>()
        .snapshot
        .lock()
        .map(|snapshot| (snapshot.volume, snapshot.muted))
        .unwrap_or((None, None))
}

impl ConnectedTv {
    async fn refresh_picture_context(&mut self, operation: &'static str) {
        let started = Instant::now();
        let previous_input = self.picture_context.input_id.clone();
        let previous_mode = self.picture_context.picture_mode.clone();
        if crate::detailed_logging_enabled() {
            log::debug!("[tv] {operation} started");
        }
        let previous = self.picture_context.clone();
        let refreshed = read_picture_context(&mut self.socket, &previous).await;
        self.picture_context = refreshed;
        let current_mode = self
            .picture_context
            .picture_mode
            .as_deref()
            .unwrap_or("unavailable");
        let previous_mode_value = previous_mode.as_deref().unwrap_or("unavailable");
        if crate::detailed_logging_enabled() {
            log::debug!(
                "[tv] {operation} completed elapsed_ms={} input_before={} input_after={} input_changed={} mode_before={} mode_after={} mode_changed={} error_present={}",
                elapsed_ms(started),
                sanitized_log_value(&previous_input),
                sanitized_log_value(&self.picture_context.input_id),
                previous_input != self.picture_context.input_id,
                sanitized_log_value(previous_mode_value),
                sanitized_log_value(current_mode),
                previous_mode.as_deref() != Some(current_mode),
                !self.picture_context.error.is_empty()
            );
        }
    }

    async fn wait_for_picture_mode(&mut self, expected_mode: &str) {
        if self.picture_context.picture_mode.as_deref() == Some(expected_mode) {
            return;
        }
        for attempt in 1..=PICTURE_MODE_READBACK_ATTEMPTS {
            match read_picture_mode(&mut self.socket).await {
                Ok(mode) => {
                    self.picture_context.picture_modes = picture_modes_for_mode(&mode);
                    self.picture_context.picture_mode = Some(mode);
                }
                Err(error) if is_picture_transport_failure(&error) => {
                    log::warn!(
                        "[tv] picture mode readback stopped after transport failure detail={}",
                        sanitized_error_detail(&error)
                    );
                    return;
                }
                Err(_) => {}
            }
            let actual_mode = self
                .picture_context
                .picture_mode
                .as_deref()
                .unwrap_or("unavailable");
            log::debug!(
                "[tv] picture mode readback check attempt={} expected={} actual={}",
                attempt,
                sanitized_log_value(expected_mode),
                sanitized_log_value(actual_mode)
            );
            if actual_mode == expected_mode {
                return;
            }
            if attempt < PICTURE_MODE_READBACK_ATTEMPTS {
                tokio::time::sleep(PICTURE_MODE_READBACK_DELAY).await;
            }
        }
    }

    fn snapshot(
        &self,
        volume: Option<u32>,
        muted: Option<bool>,
        error: Option<String>,
    ) -> TvStateSnapshot {
        TvStateSnapshot {
            state: "connected".to_string(),
            device_id: self.device_id.clone(),
            name: self.name.clone(),
            host: self.host.clone(),
            endpoint: self.endpoint.clone(),
            paired: self.client_key.is_some(),
            credential_saved: self.credential_saved,
            pending: false,
            volume,
            muted,
            input_id: self.picture_context.input_id.clone(),
            input_label: self.picture_context.input_label.clone(),
            picture_mode: self.picture_context.picture_mode.clone(),
            picture_modes: self.picture_context.picture_modes.clone(),
            picture_settings: self.picture_context.picture_settings.clone(),
            picture_settings_error: self.picture_context.picture_settings_error.clone(),
            picture_error: self.picture_context.error.clone(),
            picture_write_error: String::new(),
            error: error.unwrap_or_default(),
        }
    }
}

fn save_snapshot(app: &AppHandle, snapshot: &TvStateSnapshot) {
    let state = app.state::<TvControllerState>();
    if let Ok(mut current) = state.snapshot.lock() {
        *current = snapshot.clone();
    };
}

fn publish(app: &AppHandle, snapshot: TvStateSnapshot) {
    save_snapshot(app, &snapshot);
    let _ = app.emit("tv-state-changed", snapshot);
}

fn stable_error(error: &str) -> String {
    let lower = error.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        "The TV did not answer in time".to_string()
    } else if lower.contains("certificate") || lower.contains("tls") {
        "The TV secure certificate was rejected".to_string()
    } else if lower.contains("connect") || lower.contains("refused") {
        "Could not connect to the TV".to_string()
    } else if lower.contains("closed") {
        "The TV closed the connection".to_string()
    } else if lower.contains("reject") || lower.contains("403") {
        "The TV rejected the pairing request".to_string()
    } else {
        "The TV request failed".to_string()
    }
}

#[tauri::command]
pub fn tv_get_discovery(state: State<'_, TvControllerState>) -> TvDiscoverySnapshot {
    state
        .discovery
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

#[tauri::command]
pub fn tv_discover(app: AppHandle) -> Result<TvDiscoverySnapshot, String> {
    let state = app.state::<TvControllerState>();
    let current_discovery = state
        .discovery
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    if current_discovery.state == "scanning" {
        return Ok(current_discovery);
    }
    let current_tv = state
        .snapshot
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    if current_tv.pending {
        return Err("Wait for the current TV operation to finish before searching".to_string());
    }

    let sender = ensure_worker(&app)?;
    let snapshot = TvDiscoverySnapshot {
        state: "scanning".to_string(),
        candidates: Vec::new(),
        error: String::new(),
    };
    publish_discovery(&app, snapshot.clone());
    if sender.try_send(WorkerCommand::Discover).is_err() {
        let message = "TV discovery could not start".to_string();
        publish_discovery(
            &app,
            TvDiscoverySnapshot {
                state: "error".to_string(),
                candidates: Vec::new(),
                error: message.clone(),
            },
        );
        return Err(message);
    }
    Ok(snapshot)
}

#[tauri::command]
pub fn tv_get_state(state: State<'_, TvControllerState>) -> TvStateSnapshot {
    state
        .snapshot
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

#[tauri::command]
pub fn tv_pair(
    app: AppHandle,
    host: String,
    name: Option<String>,
) -> Result<TvStateSnapshot, String> {
    let host = validate_host(&host)?.to_string();
    let name = name.unwrap_or_else(|| "OLED Helper".to_string());
    let sender = ensure_worker(&app)?;
    let state = app.state::<TvControllerState>();
    if let Ok(mut snapshot) = state.snapshot.lock() {
        *snapshot = TvStateSnapshot {
            state: "connecting".to_string(),
            name: name.clone(),
            host: host.clone(),
            endpoint: format!("wss://{host}:{SECURE_PORT}"),
            pending: true,
            ..TvStateSnapshot::default()
        };
    }
    sender
        .try_send(WorkerCommand::Pair { host, name })
        .map_err(|_| "TV controller is shutting down".to_string())?;
    Ok(tv_get_state(state))
}

#[tauri::command]
pub fn tv_connect(
    app: AppHandle,
    host: String,
    name: Option<String>,
) -> Result<TvStateSnapshot, String> {
    let host = validate_host(&host)?.to_string();
    if let Some(mut device) = crate::config::saved_tv_device() {
        if device.host == host && device.paired {
            device.auto_reconnect = true;
            crate::config::update_tv_device(&app, Some(device.clone()))?;
            let sender = ensure_worker(&app)?;
            let state = app.state::<TvControllerState>();
        if let Ok(mut snapshot) = state.snapshot.lock() {
            *snapshot = saved_device_snapshot(&device, "connecting");
        }
        sender
            .try_send(WorkerCommand::ReconnectSaved {
                device,
                client_key: None,
                credential_saved: true,
            })
                .map_err(|_| "TV controller is shutting down".to_string())?;
            return Ok(tv_get_state(state));
        }
    }
    tv_pair(app, host, name)
}

#[tauri::command]
pub fn tv_disconnect(app: AppHandle) -> Result<(), String> {
    let sender = ensure_worker(&app)?;
    sender
        .try_send(WorkerCommand::Disconnect)
        .map_err(|_| "TV controller is shutting down".to_string())
}

#[tauri::command]
pub fn tv_adjust_volume(app: AppHandle, delta: i32) -> Result<(), String> {
    if !(-10..=10).contains(&delta) || delta == 0 {
        return Err("TV volume step is out of range".to_string());
    }
    queue_action(
        &app,
        if delta < 0 {
            "decrease_tv_volume"
        } else {
            "increase_tv_volume"
        },
    )
}

#[tauri::command]
pub fn tv_toggle_mute(app: AppHandle) -> Result<(), String> {
    queue_action(&app, "toggle_tv_mute")
}

#[tauri::command]
pub async fn tv_refresh_picture_context(app: AppHandle) -> Result<TvStateSnapshot, String> {
    let sender = ensure_worker(&app)?;
    let (reply, receiver) = oneshot::channel();
    sender
        .try_send(WorkerCommand::RefreshPictureContext { reply })
        .map_err(|_| "TV controller is shutting down".to_string())?;
    receiver
        .await
        .map_err(|_| "TV controller stopped before the refresh completed".to_string())?
}

#[tauri::command]
pub async fn tv_restart(app: AppHandle) -> Result<(), String> {
    let sender = ensure_worker(&app)?;
    let (reply, receiver) = oneshot::channel();
    sender
        .try_send(WorkerCommand::Restart { reply })
        .map_err(|_| "TV controller is shutting down".to_string())?;
    receiver
        .await
        .map_err(|_| "TV controller stopped before the restart request completed".to_string())?
}

#[tauri::command]
pub async fn tv_set_picture_mode(app: AppHandle, mode: String) -> Result<TvStateSnapshot, String> {
    let mode = validate_picture_mode(mode.trim())?;
    let sender = ensure_worker(&app)?;
    let (reply, receiver) = oneshot::channel();
    sender
        .try_send(WorkerCommand::SetPictureMode {
            mode,
            reply: Some(reply),
        })
        .map_err(|_| "TV controller is shutting down".to_string())?;
    receiver
        .await
        .map_err(|_| "TV controller stopped before the picture mode write completed".to_string())?
}

#[tauri::command]
pub async fn tv_set_picture_setting(
    app: AppHandle,
    setting_id: String,
    value: String,
) -> Result<TvStateSnapshot, String> {
    let setting_id = setting_id.trim().to_string();
    let spec = picture_setting_spec(&setting_id)?;
    let value = validate_picture_setting_value(spec, &value)?;
    let sender = ensure_worker(&app)?;
    let (reply, receiver) = oneshot::channel();
    sender
        .try_send(WorkerCommand::SetPictureSetting {
            setting_id,
            value,
            reply: Some(reply),
        })
        .map_err(|_| "TV controller is shutting down".to_string())?;
    receiver.await.map_err(|_| {
        "TV controller stopped before the picture setting write completed".to_string()
    })?
}

#[tauri::command]
pub async fn tv_forget(app: AppHandle) -> Result<(), String> {
    forget_and_wait(&app).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_reconnect_does_not_cancel_an_active_reconnect() {
        let command = WorkerCommand::ReconnectSaved {
            device: TvDeviceConfig::default(),
            client_key: None,
            credential_saved: false,
        };
        assert!(!command_cancels_reconnect(&command));
    }

    #[test]
    fn reconnect_delay_stays_at_the_maximum_after_initial_backoff() {
        assert_eq!(reconnect_delay(0), Duration::from_secs(2));
        assert_eq!(reconnect_delay(1), Duration::from_secs(5));
        assert_eq!(reconnect_delay(2), Duration::from_secs(15));
        assert_eq!(reconnect_delay(3), Duration::from_secs(30));
        assert_eq!(reconnect_delay(4), Duration::from_secs(30));
        assert_eq!(reconnect_delay(100), Duration::from_secs(30));
    }

    #[test]
    fn numeric_hosts_are_accepted() {
        assert_eq!(
            validate_host("192.168.1.50").unwrap().to_string(),
            "192.168.1.50"
        );
        assert_eq!(
            validate_host(" 192.168.1.50 ").unwrap().to_string(),
            "192.168.1.50"
        );
        assert!(validate_host("tv.local").is_err());
        assert!(validate_host("not a host").is_err());
    }

    #[test]
    fn registration_uses_the_ssap_register_message() {
        let payload = registration_payload("OLED Helper", None);
        assert_eq!(
            payload.get("type").and_then(Value::as_str),
            Some("register")
        );
        assert_eq!(
            payload
                .pointer("/payload/pairingType")
                .and_then(Value::as_str),
            Some("PROMPT")
        );
        assert_eq!(
            payload
                .pointer("/payload/manifest/appVersion")
                .and_then(Value::as_str),
            Some("1.1")
        );
        assert_eq!(
            payload
                .pointer("/payload/manifest/signed/serial")
                .and_then(Value::as_str),
            Some("2f930e2d2cfe083771f68e4fe7bb07")
        );
        assert!(payload.pointer("/payload/client-key").is_none());
    }

    #[test]
    fn reconnect_registration_keeps_the_key_in_the_payload() {
        let payload = registration_payload("OLED Helper", Some("stored-secret"));
        assert_eq!(
            payload
                .pointer("/payload/client-key")
                .and_then(Value::as_str),
            Some("stored-secret")
        );
    }

    #[test]
    fn pairing_prompt_ack_is_not_the_final_registration() {
        let value = json!({
            "type": "response",
            "payload": { "pairingType": "PROMPT", "returnValue": true }
        });
        assert!(is_pairing_prompt_ack(&value));
        let registered = json!({ "type": "registered", "payload": { "client-key": "secret" } });
        assert!(!is_pairing_prompt_ack(&registered));
    }

    #[test]
    fn pairing_parser_keeps_key_out_of_errors() {
        let value = json!({ "type": "response", "payload": { "type": "registered", "client-key": "secret" } });
        let (paired, key) = parse_pairing_response(&value).unwrap();
        assert!(paired);
        assert_eq!(key.as_deref(), Some("secret"));
        let rejected = json!({ "type": "response", "payload": { "type": "rejected", "client-key": "secret" } });
        let (paired, key) = parse_pairing_response(&rejected).unwrap();
        assert!(!paired);
        assert!(key.is_none());
    }

    #[test]
    fn response_values_are_bounded() {
        let value = json!({ "payload": { "volume": 500 } });
        assert_eq!(parse_number(&value, "volume"), Some(100));
        let value = json!({ "payload": { "muted": true } });
        assert_eq!(parse_bool(&value, "muted"), Some(true));
        let value = json!({ "payload": { "volumeStatus": { "muteStatus": true, "volume": 20 } } });
        assert_eq!(parse_bool(&value, "muted"), Some(true));
        assert_eq!(parse_number(&value, "volume"), Some(20));
    }

    #[test]
    fn picture_context_parsers_accept_webos_variants() {
        let settings = json!({
            "payload": {
                "returnValue": true,
                "settings": { "pictureMode": "filmmaker" }
            }
        });
        assert_eq!(
            picture_mode_from_response(&settings),
            Some("filmmaker".to_string())
        );
        let legacy = json!({ "payload": { "pictureMode": "cinema" } });
        assert_eq!(
            picture_mode_from_response(&legacy),
            Some("cinema".to_string())
        );
        let inputs = json!({
            "payload": {
                "devices": [
                    { "appId": "HDMI_1", "label": "HDMI 1" },
                    { "appId": "HDMI_2", "label": "HDMI 2" }
                ]
            }
        });
        assert_eq!(input_label_from_response(&inputs, "HDMI_2"), "HDMI 2");
    }

    #[test]
    fn picture_modes_follow_the_current_hdr_context() {
        let sdr = picture_modes_for_mode("cinema");
        assert!(sdr.iter().any(|option| option.id == "filmMaker"));
        assert!(!sdr.iter().any(|option| option.id == "hdrGame"));
        assert!(!sdr.iter().any(|option| option.id.starts_with("dolbyHdr")));

        let hdr = picture_modes_for_mode("hdrGame");
        assert!(hdr.iter().any(|option| option.id == "hdrFilmMaker"));
        assert!(hdr.iter().any(|option| option.id == "hdrCinema"));
        assert!(hdr.iter().any(|option| option.id == "hdrCinemaBright"));
        assert!(!hdr.iter().any(|option| option.id == "game"));
        assert!(!hdr.iter().any(|option| option.id.starts_with("dolbyHdr")));

        let dolby = picture_modes_for_mode("dolbyHdrCinema");
        assert!(dolby.iter().any(|option| option.id == "dolbyHdrGame"));
        assert!(dolby.iter().any(|option| option.id == "dolbyHdrCinema"));
        assert!(dolby.iter().any(|option| option.id == "dolbyHdrCinemaBright"));
        assert!(!dolby.iter().any(|option| option.id.starts_with("hdr")));
    }

    #[test]
    fn picture_mode_writes_are_restricted_to_the_current_context() {
        assert!(picture_mode_allowed_for_context(
            "hdrGame",
            Some("hdrCinema")
        ));
        assert!(picture_mode_allowed_for_context(
            "hdrCinemaBright",
            Some("hdrCinema")
        ));
        assert!(picture_mode_allowed_for_context(
            "dolbyHdrCinemaBright",
            Some("dolbyHdrCinema")
        ));
        assert!(!picture_mode_allowed_for_context("game", Some("hdrCinema")));
        assert!(!picture_mode_allowed_for_context(
            "dolbyHdrGame",
            Some("hdrCinema")
        ));
        assert!(!picture_mode_allowed_for_context("game", None));
    }

    #[test]
    fn hdr_picture_modes_are_recognized_by_id() {
        // Dynamic Tone Mapping is gated on this check, in the TV tab and in
        // the worker's picture setting write, so the two must agree.
        assert!(is_hdr_picture_mode_id("hdrGame"));
        assert!(is_hdr_picture_mode_id("dolbyHdrCinema"));
        assert!(!is_hdr_picture_mode_id("game"));
        assert!(!is_hdr_picture_mode_id("filmmaker"));
    }

    #[test]
    fn picture_mode_writes_require_a_known_identifier() {
        assert_eq!(validate_picture_mode("filmMaker").unwrap(), "filmMaker");
        assert_eq!(
            validate_picture_mode("hdrCinemaBright").unwrap(),
            "hdrCinemaBright"
        );
        assert_eq!(
            validate_picture_mode("dolbyHdrCinemaBright").unwrap(),
            "dolbyHdrCinemaBright"
        );
        assert!(validate_picture_mode("Game").is_err());
        assert!(validate_picture_mode("game; shutdown").is_err());
    }

    #[test]
    fn picture_setting_values_are_validated_per_kind() {
        let numeric = picture_setting_spec("backlight").unwrap();
        assert_eq!(
            validate_picture_setting_value(numeric, " 45 ").unwrap(),
            "45"
        );
        assert!(validate_picture_setting_value(numeric, "101").is_err());
        assert!(validate_picture_setting_value(numeric, "-1").is_err());
        assert!(validate_picture_setting_value(numeric, "forty").is_err());
        assert!(validate_picture_setting_value(numeric, "45; shutdown").is_err());

        // Color temperature is a signed offset on webOS, so negatives are
        // valid inside its own range.
        let temperature = picture_setting_spec("colorTemperature").unwrap();
        assert_eq!(
            validate_picture_setting_value(temperature, "-30").unwrap(),
            "-30"
        );
        assert_eq!(
            validate_picture_setting_value(temperature, " 20 ").unwrap(),
            "20"
        );
        assert!(validate_picture_setting_value(temperature, "-51").is_err());
        assert!(validate_picture_setting_value(temperature, "51").is_err());

        let dtm = picture_setting_spec("hdrDynamicToneMapping").unwrap();
        assert_eq!(validate_picture_setting_value(dtm, "HGIG").unwrap(), "HGIG");
        assert_eq!(validate_picture_setting_value(dtm, "off").unwrap(), "off");
        assert!(validate_picture_setting_value(dtm, "hgig").is_err());
        assert!(validate_picture_setting_value(dtm, "always").is_err());

        let color_depth = picture_setting_spec("color").unwrap();
        assert_eq!(
            validate_picture_setting_value(color_depth, "0").unwrap(),
            "0"
        );
        assert_eq!(
            validate_picture_setting_value(color_depth, "100").unwrap(),
            "100"
        );
        assert!(validate_picture_setting_value(color_depth, "-1").is_err());
        assert!(validate_picture_setting_value(color_depth, "101").is_err());

        let color_gamut = picture_setting_spec("colorGamut").unwrap();
        for value in ["auto", "native", "dynamic", "adobeRGB"] {
            assert_eq!(
                validate_picture_setting_value(color_gamut, value).unwrap(),
                value
            );
        }
        for value in ["Auto", "extended", "wide", "srgb", "adobeRgb"] {
            assert!(validate_picture_setting_value(color_gamut, value).is_err());
        }
    }

    #[test]
    fn picture_setting_alert_payload_carries_the_requested_key() {
        let spec = picture_setting_spec("backlight").unwrap();
        let value = validate_picture_setting_value(spec, "42").unwrap();
        let params = picture_setting_luna_params(spec.key, &value);
        let payload = picture_alert_payload(&params);
        assert_eq!(
            payload
                .pointer("/buttons/0/params/settings/backlight")
                .and_then(Value::as_str),
            Some("42")
        );
        assert_eq!(
            payload.pointer("/onclose/uri").and_then(Value::as_str),
            Some(PICTURE_MODE_LUNA_URI)
        );
        assert!(payload.get("alert").is_none());
        assert!(!payload
            .to_string()
            .to_ascii_lowercase()
            .contains("client-key"));

        let silent_payload =
            picture_silent_alert_payload(&picture_setting_luna_params("colorGamut", "adobeRGB"));
        assert_eq!(
            silent_payload
                .pointer("/buttons/0/params/settings/colorGamut")
                .and_then(Value::as_str),
            Some("adobeRGB")
        );
        assert_eq!(
            silent_payload
                .pointer("/onclose/params/settings/colorGamut")
                .and_then(Value::as_str),
            Some("adobeRGB")
        );
    }

    #[test]
    fn picture_setting_transport_failures_are_distinguished_from_rejections() {
        assert!(is_picture_transport_failure("The TV did not answer in time"));
        assert!(is_picture_transport_failure("The TV closed the connection"));
        assert!(is_picture_transport_failure("The TV request failed"));
        assert!(is_picture_transport_failure("connection reset by peer"));
        assert!(!is_picture_transport_failure("401 insufficient permissions"));
        assert!(!is_picture_transport_failure("404 no such service or method"));
        assert!(!is_picture_transport_failure("The TV rejected the picture setting change"));
    }

    #[test]
    fn picture_setting_values_parse_from_response_variants() {
        let response = json!({
            "payload": {
                "settings": {
                    "backlight": 45,
                    "contrast": "80",
                    "color": "50",
                    "colorGamut": "dynamic",
                    "hdrDynamicToneMapping": "HGIG"
                }
            }
        });
        assert_eq!(
            picture_setting_value_from_response(&response, "backlight").as_deref(),
            Some("45")
        );
        assert_eq!(
            picture_setting_value_from_response(&response, "contrast").as_deref(),
            Some("80")
        );
        assert_eq!(
            picture_setting_value_from_response(&response, "color").as_deref(),
            Some("50")
        );
        assert_eq!(
            picture_setting_value_from_response(&response, "colorGamut").as_deref(),
            Some("dynamic")
        );
        assert_eq!(
            picture_setting_value_from_response(&response, "hdrDynamicToneMapping").as_deref(),
            Some("HGIG")
        );
        assert_eq!(
            picture_setting_value_from_response(&response, "missing"),
            None
        );
    }

    #[test]
    fn missing_combined_picture_settings_are_merged_from_single_key_reads() {
        let mut combined = json!({
            "payload": {
                "settings": {
                    "backlight": 45
                }
            }
        });
        let gamut = json!({
            "payload": {
                "settings": {
                    "colorGamut": "dynamic"
                }
            }
        });
        let dtm = json!({
            "payload": {
                "settings": {
                    "hdrDynamicToneMapping": "HGIG"
                }
            }
        });        assert!(merge_picture_setting_value(&mut combined, "colorGamut", &gamut));
        assert!(merge_picture_setting_value(
            &mut combined,
            "hdrDynamicToneMapping",
            &dtm
        ));
        assert_eq!(
            picture_setting_value_from_response(&combined, "backlight").as_deref(),
            Some("45")
        );
        assert_eq!(
            picture_setting_value_from_response(&combined, "colorGamut").as_deref(),
            Some("dynamic")
        );
        assert_eq!(
            picture_setting_value_from_response(&combined, "hdrDynamicToneMapping").as_deref(),
            Some("HGIG")
        );
        assert!(!merge_picture_setting_value(
            &mut combined,
            "colorGamut",
            &json!({ "payload": { "settings": {} } })
        ));
    }

    #[test]
    fn picture_setting_specs_cover_supported_controls() {
        assert_eq!(PICTURE_SETTING_SPECS.len(), 7);
        assert_eq!(
            PICTURE_SETTING_SPECS
                .iter()
                .map(|spec| spec.id)
                .collect::<Vec<_>>(),
            vec![
                "backlight",
                "colorTemperature",
                "contrast",
                "color",
                "brightness",
                "colorGamut",
                "hdrDynamicToneMapping",
            ]
        );
        for spec in PICTURE_SETTING_SPECS {
            assert!(!spec.id.is_empty());
            assert!(!spec.key.is_empty());
            assert!(!spec.label.is_empty());
        }
        assert!(picture_setting_spec("backlight").is_ok());
        assert!(picture_setting_spec("contrast").is_ok());
        assert!(picture_setting_spec("brightness").is_ok());
        assert!(picture_setting_spec("colorTemperature").is_ok());
        assert!(picture_setting_spec("color").is_ok());
        assert!(picture_setting_spec("colorGamut").is_ok());
        assert!(picture_setting_spec("hdrDynamicToneMapping").is_ok());
        assert!(picture_setting_spec("peakBrightness").is_err());
    }

    #[test]
    fn rejected_picture_setting_keys_are_skipped_and_recoverable() {
        clear_unsupported_picture_setting_keys();
        let hdr_mode = Some("hdrCinemaBright");
        assert!(picture_setting_keys(hdr_mode).contains(&"contrast".to_string()));
        remember_unsupported_picture_setting_key(hdr_mode, "contrast");
        assert!(is_unsupported_picture_setting_key(hdr_mode, "contrast"));
        assert!(!picture_setting_keys(hdr_mode).contains(&"contrast".to_string()));
        assert!(
            picture_setting_keys(Some("dolbyHdrCinemaBright")).contains(&"contrast".to_string())
        );
        assert!(!is_unsupported_picture_setting_key(
            Some("dolbyHdrCinemaBright"),
            "contrast"
        ));
        // pictureMode is always requested, even if every setting key fails.
        assert!(picture_setting_keys(hdr_mode).contains(&PICTURE_MODE_CONTEXT_KEY.to_string()));
        // Repeating the record must not duplicate the entry for this mode.
        remember_unsupported_picture_setting_key(hdr_mode, "contrast");
        let keys = unsupported_picture_setting_keys()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(
            keys.iter()
                .filter(|(mode, key)| mode == "hdrCinemaBright" && key == "contrast")
                .count(),
            1
        );
    }

    #[test]
    fn unsupported_keys_mark_their_setting_state() {
        clear_unsupported_picture_setting_keys();
        remember_unsupported_picture_setting_key(Some("cinema"), "colorTemperature");
        let mut context = PictureContext::default();
        context.picture_settings = PICTURE_SETTING_SPECS
            .iter()
            .map(|spec| TvPictureSettingState {
                id: spec.id.to_string(),
                label: spec.label.to_string(),
                kind: spec.kind.as_str().to_string(),
                min: spec.min,
                max: spec.max,
                value: Some("50".to_string()),
                supported: !is_unsupported_picture_setting_key(Some("cinema"), spec.key),
            })
            .collect();
        let color = context
            .picture_settings
            .iter()
            .find(|setting| setting.id == "colorTemperature")
            .unwrap();
        assert!(!color.supported);
        let contrast = context
            .picture_settings
            .iter()
            .find(|setting| setting.id == "contrast")
            .unwrap();
        assert!(contrast.supported);
    }

    #[test]
    fn picture_mode_readback_reports_a_mismatch() {
        let context = PictureContext {
            picture_mode: Some("hdrGame".to_string()),
            ..PictureContext::default()
        };
        assert_eq!(
            picture_mode_write_error(&context, "hdrCinema").as_deref(),
            Some("Requested picture mode hdrCinema, but the TV reports hdrGame")
        );
        assert!(picture_mode_write_error(&context, "hdrGame").is_none());
        let missing = PictureContext::default();
        assert_eq!(
            picture_mode_write_error(&missing, "hdrGame").as_deref(),
            Some("The TV did not report a picture mode after the change")
        );
    }

    #[test]
    fn picture_mode_alert_payload_uses_the_settings_service_handlers() {
        let payload = picture_setting_alert_payload(PICTURE_MODE_CONTEXT_KEY, "hdrGame");
        assert_eq!(
            payload.pointer("/message").and_then(Value::as_str),
            Some(" ")
        );
        assert_eq!(
            payload.pointer("/modal").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            payload.pointer("/type").and_then(Value::as_str),
            Some("confirm")
        );
        assert_eq!(
            payload.pointer("/isSysReq").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            payload.pointer("/buttons/0/focus").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            payload
                .pointer("/buttons/0/buttonType")
                .and_then(Value::as_str),
            Some("ok")
        );
        assert!(payload.get("alert").is_none());
        assert_eq!(
            payload
                .pointer("/buttons/0/onClick")
                .and_then(Value::as_str),
            Some(PICTURE_MODE_LUNA_URI)
        );
        assert_eq!(
            payload
                .pointer("/buttons/0/params/category")
                .and_then(Value::as_str),
            Some("picture")
        );
        assert_eq!(
            payload
                .pointer("/buttons/0/params/settings/pictureMode")
                .and_then(Value::as_str),
            Some("hdrGame")
        );
        assert_eq!(
            payload.pointer("/onclose/uri").and_then(Value::as_str),
            Some(PICTURE_MODE_LUNA_URI)
        );
        assert_eq!(
            payload
                .pointer("/onfail/params/settings/pictureMode")
                .and_then(Value::as_str),
            Some("hdrGame")
        );
        assert!(!payload
            .to_string()
            .to_ascii_lowercase()
            .contains("client-key"));
    }

    #[test]
    fn picture_setting_luna_params_match_direct_write_shape() {
        let params = picture_setting_luna_params(PICTURE_MODE_CONTEXT_KEY, "hdrGame");
        assert_eq!(
            params.pointer("/category").and_then(Value::as_str),
            Some("picture")
        );
        assert_eq!(
            params
                .pointer("/settings/pictureMode")
                .and_then(Value::as_str),
            Some("hdrGame")
        );
        assert!(params.get("current_app").is_none());
    }

    #[test]
    fn picture_mode_silent_alert_payload_matches_colorcontrol_shape() {
        let payload = picture_silent_alert_payload(&picture_setting_luna_params(
            PICTURE_MODE_CONTEXT_KEY,
            "hdrVivid",
        ));
        assert_eq!(
            payload.pointer("/message").and_then(Value::as_str),
            Some("Applying...")
        );
        assert_eq!(
            payload
                .pointer("/buttons/0/onClick")
                .and_then(Value::as_str),
            Some(PICTURE_MODE_LUNA_URI)
        );
        assert_eq!(
            payload
                .pointer("/buttons/0/params/settings/pictureMode")
                .and_then(Value::as_str),
            Some("hdrVivid")
        );
        assert_eq!(
            payload.pointer("/onclose/uri").and_then(Value::as_str),
            Some(PICTURE_MODE_LUNA_URI)
        );
        assert_eq!(
            payload
                .pointer("/onfail/params/settings/pictureMode")
                .and_then(Value::as_str),
            Some("hdrVivid")
        );
        assert!(payload.get("modal").is_none());
        assert!(payload.get("type").is_none());
        assert!(payload.get("isSysReq").is_none());
    }

    #[test]
    fn picture_alert_id_parser_accepts_webos_response_variants() {
        assert_eq!(
            alert_id_from_response(&json!({ "payload": { "alertId": "alert 1" } })),
            Some("alert 1".to_string())
        );
        assert_eq!(
            alert_id_from_response(&json!({ "payload": { "alert": { "id": 42 } } })),
            Some("42".to_string())
        );
        assert_eq!(
            alert_id_from_response(&json!({ "payload": { "alertId": "  " } })),
            None
        );
    }

    #[test]
    fn fingerprint_matching_is_strict_about_any_differing_digest() {
        let paired = "sha256:aabbcc";
        assert!(fingerprints_match("sha256:aabbcc", paired));
        assert!(fingerprints_match("SHA256:AA:bb:cc", paired));
        assert!(!fingerprints_match("sha256:aabbcd", paired));
        assert!(!fingerprints_match("sha256:aabbc", paired));
        // Nothing pinned yet: no presented value can be trusted, so nothing
        // matches. The callers treat an empty pin as "unpinned" explicitly.
        assert!(!fingerprints_match("sha256:aabbcc", ""));
        assert!(!fingerprints_match("", "sha256:aabbcc"));
    }

    #[test]
    fn unpinned_saved_pairings_have_no_expected_certificate() {
        let legacy = TvDeviceConfig {
            device_id: "lg-192.168.1.50".to_string(),
            paired: true,
            ..TvDeviceConfig::default()
        };
        assert!(expected_certificate_fingerprint(&legacy).is_empty());

        let pinned = TvDeviceConfig {
            device_id: "lg-192.168.1.50".to_string(),
            paired: true,
            certificate_fingerprint: "sha256:ddeeff".to_string(),
            ..TvDeviceConfig::default()
        };
        assert_eq!(
            expected_certificate_fingerprint(&pinned),
            "sha256ddeeff".to_string()
        );
    }

    #[test]
    fn specialized_socket_urls_stay_on_the_tv_endpoint() {
        assert_eq!(
            specialized_socket_url(
                "wss://192.168.1.50:3001/resources/pointer.sock",
                "wss://192.168.1.50:3001"
            )
            .unwrap(),
            "wss://192.168.1.50:3001/resources/pointer.sock"
        );
        assert_eq!(
            specialized_socket_url("/resources/pointer.sock", "wss://192.168.1.50:3001").unwrap(),
            "wss://192.168.1.50:3001/resources/pointer.sock"
        );
        assert!(specialized_socket_url(
            "http://192.168.1.50/resources/pointer.sock",
            "wss://192.168.1.50:3001"
        )
        .is_err());
    }

    #[test]
    fn picture_write_fallback_order_starts_with_direct_methods() {
        assert_eq!(
            PICTURE_WRITE_METHODS,
            [
                PictureWriteMethod::Luna,
                PictureWriteMethod::DirectSs,
                PictureWriteMethod::SilentAlert,
                PictureWriteMethod::VisibleAlert,
            ]
        );
        assert_eq!(PictureWriteMethod::SilentAlert.as_str(), "alert_silent");
        assert_eq!(PictureWriteMethod::VisibleAlert.as_str(), "alert");
    }

    #[test]
    fn picture_write_fallback_skips_transport_failures() {
        assert!(can_try_picture_write_fallback("The TV request failed"));
        assert!(can_try_picture_write_fallback("permission denied"));
        assert!(can_try_picture_write_fallback(
            "401 insufficient permissions"
        ));
        assert!(can_try_picture_write_fallback(
            "404 no such service or method"
        ));
        assert!(!can_try_picture_write_fallback(
            "The TV closed the connection"
        ));
        assert!(!can_try_picture_write_fallback(
            "The TV did not answer in time"
        ));
        assert!(!can_try_picture_write_fallback(
            "The TV secure certificate was rejected"
        ));
    }

    #[test]
    fn tv_diagnostics_redact_sensitive_error_details() {
        assert_eq!(
            sanitized_error_detail("client-key=secret value"),
            "protected credential detail redacted"
        );
        assert_eq!(
            sanitized_error_detail("server said abcdef0123456789abcdef"),
            "protected credential detail redacted"
        );
        assert_eq!(
            sanitized_uri("ss://settings/setSystemSettings?client-key=secret"),
            "<redacted>"
        );
        assert_eq!(sanitized_log_value("  picture mode\n"), "picture mode");
    }

    #[test]
    fn picture_write_rejects_a_false_return_value() {
        assert!(response_succeeded(&json!({
            "payload": { "returnValue": true }
        })));
        assert!(!response_succeeded(&json!({
            "payload": { "returnValue": false }
        })));
    }

    #[test]
    fn discovery_bind_targets_include_the_lan_address_and_a_fallback() {
        let targets = discovery_bind_targets();
        // The wildcard fallback always runs, so a machine whose outbound
        // address is not the TV's subnet still searches the old way.
        assert!(targets.contains(&Ipv4Addr::UNSPECIFIED));
        assert!(targets.len() <= 2);
        // On a host with a real LAN route the LAN address is searched too, and
        // it is preferred over the wildcard.
        if let Some(lan) = primary_outbound_ipv4() {
            assert_eq!(targets.first(), Some(&lan));
            assert!(is_usable_local_discovery_ip(lan));
        }
    }

    #[test]
    fn discovery_rejects_adapter_addresses_that_cannot_reach_a_tv() {
        // Loopback, unspecified, multicast, broadcast, and the 169.254/16
        // self-assigned range a disconnected Wi-Fi adapter picks up are all
        // unusable as an SSDP source.
        for rejected in [
            "127.0.0.1",
            "0.0.0.0",
            "239.255.255.250",
            "255.255.255.255",
            "169.254.224.103",
        ] {
            assert!(
                !is_usable_local_discovery_ip(rejected.parse().expect("valid literal")),
                "expected address to be rejected: {rejected}"
            );
        }
        for accepted in ["192.168.1.36", "10.0.0.5", "172.16.4.9"] {
            assert!(
                is_usable_local_discovery_ip(accepted.parse().expect("valid literal")),
                "expected address to be accepted: {accepted}"
            );
        }
    }

    #[test]
    fn discovered_tv_ip_updates_without_changing_the_pairing_identity() {
        let device = TvDeviceConfig {
            device_id: "lg-192.168.1.50".to_string(),
            name: "OLED TV".to_string(),
            host: "192.168.1.50".to_string(),
            endpoint: "wss://192.168.1.50:3001".to_string(),
            model: "OLED55C4".to_string(),
            firmware: "26.10.05".to_string(),
            paired: true,
            auto_reconnect: true,
            certificate_fingerprint: "sha256:paired-certificate".to_string(),
        };

        let updated = device_with_discovered_host(&device, "192.168.1.72");

        assert_eq!(updated.device_id, device.device_id);
        assert_eq!(updated.host, "192.168.1.72");
        assert_eq!(updated.endpoint, "wss://192.168.1.72:3001");
        assert_eq!(
            updated.certificate_fingerprint,
            device.certificate_fingerprint
        );
        assert_eq!(updated.model, device.model);
        assert_eq!(updated.firmware, device.firmware);
        assert!(updated.paired);
        assert_eq!(device.host, "192.168.1.50");
    }

    #[test]
    fn parses_lg_ssdp_response() {
        let response = b"HTTP/1.1 200 OK\r\nLOCATION: http://192.168.1.50:1985/\r\nST: urn:lge-com:service:webos-second-screen:1\r\nUSN: uuid:tv::urn:lge-com:service:webos-second-screen:1\r\nfriendlyName: [LG] webOS TV OLED55C4\r\nmodelName: OLED55C4\r\n\r\n";
        let (candidate, location) =
            parse_ssdp_response(response, SocketAddr::from(([192, 168, 1, 50], 50000)))
                .expect("valid LG response should parse");
        assert_eq!(candidate.device_id, "lg-192.168.1.50");
        assert_eq!(candidate.host, "192.168.1.50");
        assert_eq!(candidate.name, "[LG] webOS TV OLED55C4");
        assert_eq!(candidate.model, "OLED55C4");
        assert_eq!(location, "http://192.168.1.50:1985/");
    }

    #[test]
    fn rejects_non_lg_or_mismatched_ssdp_responses() {
        let wrong_target = b"HTTP/1.1 200 OK\r\nLOCATION: http://192.168.1.50:1985/\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\nUSN: uuid:renderer\r\n\r\n";
        assert!(
            parse_ssdp_response(wrong_target, SocketAddr::from(([192, 168, 1, 50], 50000)))
                .is_none()
        );

        let wrong_location = b"HTTP/1.1 200 OK\r\nLOCATION: http://192.168.1.51:1985/\r\nST: urn:lge-com:service:webos-second-screen:1\r\nUSN: uuid:tv\r\n\r\n";
        assert!(
            parse_ssdp_response(wrong_location, SocketAddr::from(([192, 168, 1, 50], 50000)))
                .is_none()
        );
    }

    #[test]
    fn discovery_candidates_are_deduplicated_by_device_id() {
        let mut devices = BTreeMap::new();
        merge_discovery_candidate(
            &mut devices,
            TvDiscoveryCandidate {
                device_id: "lg-192.168.1.50".to_string(),
                name: "LG webOS TV".to_string(),
                host: "192.168.1.50".to_string(),
                model: "OLED55C4".to_string(),
                ..TvDiscoveryCandidate::default()
            },
        );
        merge_discovery_candidate(
            &mut devices,
            TvDiscoveryCandidate {
                device_id: "lg-192.168.1.50".to_string(),
                name: DEFAULT_TV_DISCOVERY_NAME.to_string(),
                host: "192.168.1.50".to_string(),
                model: String::new(),
                ..TvDiscoveryCandidate::default()
            },
        );
        assert_eq!(devices.len(), 1);
        assert_eq!(devices["lg-192.168.1.50"].model, "OLED55C4");
    }

    #[test]
    fn discovery_description_xml_extracts_device_details() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:2</deviceType>
    <friendlyName>[LG] webOS TV OLED55C4</friendlyName>
    <manufacturer>LG Electronics</manufacturer>
    <modelName>OLED55C4</modelName>
    <modelNumber>OLED55C4PSA</modelNumber>
    <softwareVersion>26.10.05</softwareVersion>
    <screenType>OLED</screenType>
    <resolution>3840x2160</resolution>
    <width>3840</width>
    <height>2160</height>
  </device>
</root>"#;
        assert_eq!(
            xml_tag_value(xml, "friendlyName").as_deref(),
            Some("[LG] webOS TV OLED55C4")
        );
        assert_eq!(
            xml_tag_value(xml, "modelNumber").as_deref(),
            Some("OLED55C4PSA")
        );
        assert_eq!(xml_tag_pair(xml, "width", "height"), Some((3840, 2160)));
        assert_eq!(xml_tag_value(xml, "missing"), None);
        assert_eq!(xml_tag_pair(xml, "width", "missing"), None);
    }

    #[test]
    fn restart_request_uses_the_webos_power_off_endpoint() {
        let request = tv_turn_off_request("request-1".to_string());
        assert_eq!(request.get("type").and_then(Value::as_str), Some("request"));
        assert_eq!(request.get("id").and_then(Value::as_str), Some("request-1"));
        assert_eq!(
            request.get("uri").and_then(Value::as_str),
            Some("ssap://system/turnOff")
        );
        assert_eq!(
            request
                .pointer("/payload")
                .and_then(Value::as_object)
                .map(|_| ()),
            Some(())
        );
    }

    #[test]
    fn restart_switch_values_are_strictly_validated() {
        assert!(parse_tv_switch("on", "Quick Start+").unwrap());
        assert!(parse_tv_switch("true", "Wake on LAN").unwrap());
        assert!(!parse_tv_switch("off", "Quick Start+").unwrap());
        assert!(!parse_tv_switch("false", "Wake on LAN").unwrap());
        assert!(parse_tv_switch("maybe", "Quick Start+").is_err());
        let params = tv_restart_setting_params("off");
        assert_eq!(
            params
                .pointer("/settings/quickStartMode")
                .and_then(Value::as_str),
            Some("off")
        );
    }

    #[test]
    fn restart_reads_wired_and_wifi_network_addresses() {
        let response = json!({
            "payload": {
                "wiredInfo": { "macAddress": "aa:bb:cc:dd:ee:ff" },
                "wifiInfo": { "macAddress": "11-22-33-44-55-66" }
            }
        });
        assert_eq!(
            mac_addresses_from_response(&response),
            vec![
                "AA:BB:CC:DD:EE:FF".to_string(),
                "11:22:33:44:55:66".to_string()
            ]
        );
        let packet = magic_packet("AA:BB:CC:DD:EE:FF").unwrap();
        assert_eq!(packet.len(), 102);
        assert_eq!(&packet[..6], &[0xff; 6]);
        assert_eq!(&packet[6..12], &[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
    }
}
