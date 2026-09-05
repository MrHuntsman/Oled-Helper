// key_observer.rs, resident elevated key-state observer for hold-to-compare.
//
// Windows filters every in-process key observation a non-elevated app can do
// (low-level hooks, GetAsyncKeyState, raw input) once an elevated window is
// in the foreground. A RegisterHotKey press still arrives (WM_HOTKEY is
// posted to this app's own queue), but the release cannot be seen, so
// hold-to-compare would flash off instead of holding over an admin app or
// game. The fix is an elevated observer: the same executable re-entered
// through the existing HIGHEST scheduled task runs a tiny headless loop that
// reads the physical key state (GetAsyncKeyState works from an elevated
// token over other elevated windows) and answers this app over a named pipe.
//
// Roles:
//   - This app (medium) is the pipe SERVER. It creates the pipe, so the
//     object carries a medium integrity label and the medium user DACL,
//     which the elevated observer (same user, higher integrity) is allowed
//     to open. The observer never creates windows, so no elevated foreground
//     ever exists and external hook tools are unaffected.
//   - The observer child (elevated) is the pipe CLIENT. It stays resident,
//     inert, blocked on a read, and only wakes to answer a query.
//
// The app drives the conversation: during an active hold its 40 ms timer
// asks "is the combo held?" and the observer answers in ~1 ms, so there is
// no spawn latency and release response matches the local path. When no
// observer is connected (portable install without the helper task, or an
// elevated legacy session where it is unneeded), callers fall back to local
// GetAsyncKeyState polling.

#![allow(non_snake_case)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE,
    FILE_FLAG_FIRST_PIPE_INSTANCE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, NAMED_PIPE_MODE};

// One pipe name per app process: a stale observer from an earlier session
// can never attach to the current app, and a crashed app leaves no pipe that
// a later session would inherit.
fn pipe_name() -> String {
    format!(r"\\.\pipe\OledHelperKeyState-{}", std::process::id())
}

// GENERIC_READ | GENERIC_WRITE for the client open.
const GENERIC_READ_WRITE: u32 = 0xC000_0000;

#[derive(Serialize, Deserialize)]
struct KeyQuery {
    vk: u16,
    mods: u32,
}

#[derive(Serialize, Deserialize)]
struct KeyReply {
    held: bool,
}

// One app-side query in flight to the server thread. The reply channel is
// bounded (1): the server thread never blocks, a timed-out query simply
// finds its slot full on try_send and is dropped.
struct Query {
    vk: u16,
    mods: u32,
    reply: mpsc::SyncSender<bool>,
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
// True while the server thread has an accepted observer attached.
static CONNECTED: AtomicBool = AtomicBool::new(false);
// Elevated sessions do not need the observer: local GetAsyncKeyState works.
static SELF_ELEVATED: OnceLock<bool> = OnceLock::new();
// Channel into the server thread's per-connection serve loop.
static QUERY_TX: OnceLock<Mutex<Option<mpsc::Sender<Query>>>> = OnceLock::new();
// Throttle for observer (re)spawn attempts: at most one per interval.
static LAST_SPAWN: Mutex<Option<Instant>> = Mutex::new(None);
static SPAWN_WARNED: OnceLock<()> = OnceLock::new();

fn query_tx() -> &'static Mutex<Option<mpsc::Sender<Query>>> {
    QUERY_TX.get_or_init(|| Mutex::new(None))
}

fn self_elevated() -> bool {
    *SELF_ELEVATED.get_or_init(crate::process::process_is_elevated)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

// ── Line framing over the byte-mode pipe ──────────────────────────────

// Blocking read of one newline-terminated chunk. Partial reads accumulate in
// `carry`. Returns None on pipe error or EOF (peer closed).
fn read_line(handle: HANDLE, carry: &mut Vec<u8>) -> Option<Vec<u8>> {
    loop {
        if let Some(end) = carry.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = carry.drain(..=end).collect();
            line.pop();
            return Some(line);
        }
        let mut chunk = [0u8; 512];
        let mut read = 0u32;
        match unsafe { ReadFile(handle, Some(&mut chunk), Some(&mut read), None) } {
            Ok(()) => {
                if read == 0 {
                    return None;
                }
                carry.extend_from_slice(&chunk[..read as usize]);
            }
            Err(_) => return None,
        }
    }
}

fn write_all(handle: HANDLE, bytes: &[u8]) -> bool {
    let mut written_total = 0usize;
    while written_total < bytes.len() {
        let mut written = 0u32;
        match unsafe {
            WriteFile(
                handle,
                Some(&bytes[written_total..]),
                Some(&mut written),
                None,
            )
        } {
            Ok(()) => written_total += written as usize,
            Err(_) => return false,
        }
    }
    true
}

// ── App side: pipe server thread ─────────────────────────────────────

fn server_thread_loop() {
    let name = pipe_name();
    while !SHUTDOWN.load(Ordering::Acquire) {
        let wide_name = wide(&name);
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide_name.as_ptr()),
                // Single instance so a stale pipe can never linger. Access
                // duplex (both directions): the app writes queries, the
                // observer writes replies.
                FILE_FLAGS_AND_ATTRIBUTES(
                    PIPE_ACCESS_DUPLEX.0 | FILE_FLAG_FIRST_PIPE_INSTANCE.0,
                ),
                // Byte mode, blocking reads (all three constants are 0).
                NAMED_PIPE_MODE(0),
                1,
                4096,
                4096,
                0,
                None,
            )
        };
        if handle.is_invalid() {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        if SHUTDOWN.load(Ordering::Acquire) {
            let _ = unsafe { CloseHandle(handle) };
            break;
        }
        // Blocking accept. On any error (including the already-connected
        // case where the observer beat us to it) close and recreate; an
        // observer that connected in that window retries its own open.
        if unsafe { ConnectNamedPipe(handle, None) }.is_err() {
            let _ = unsafe { CloseHandle(handle) };
            continue;
        }

        let (tx, rx) = mpsc::channel::<Query>();
        {
            if let Ok(mut guard) = query_tx().lock() {
                *guard = Some(tx);
            }
        }
        CONNECTED.store(true, Ordering::Release);

        // Serve queries until the pipe breaks (the observer died or the app
        // is shutting down). Queries only arrive while a hold is active, so
        // the thread sits idle in the channel recv between holds.
        for query in rx {
            let payload =
                serde_json::to_string(&KeyQuery { vk: query.vk, mods: query.mods })
                    .unwrap_or_default();
            if !write_all(handle, format!("{payload}\n").as_bytes()) {
                break;
            }
            let mut carry: Vec<u8> = Vec::new();
            match read_line(handle, &mut carry) {
                Some(line) => {
                    // A malformed reply means the observer is misbehaving;
                    // answer "held" so a protocol bug can never flash the
                    // compare off mid-hold. The next tick retries.
                    let held = serde_json::from_slice::<KeyReply>(&line)
                        .map(|reply| reply.held)
                        .unwrap_or(true);
                    let _ = query.reply.try_send(held);
                }
                None => break,
            }
        }

        CONNECTED.store(false, Ordering::Release);
        if let Ok(mut guard) = query_tx().lock() {
            *guard = None;
        }
        let _ = unsafe { CloseHandle(handle) };
        if SHUTDOWN.load(Ordering::Acquire) {
            break;
        }
    }
}

// Spawn the observer child through the registered HIGHEST task. Fire and
// forget: the child stays resident until the pipe closes. Throttled so a
// missing task (portable install that never did the one-time setup) cannot
// spam schtasks; the warning is emitted once.
fn request_respawn() {
    if SHUTDOWN.load(Ordering::Acquire) || self_elevated() {
        return;
    }
    {
        let mut guard = match LAST_SPAWN.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard
            .as_ref()
            .is_some_and(|last| last.elapsed() < Duration::from_secs(10))
        {
            return;
        }
        *guard = Some(Instant::now());
    }
    let name = pipe_name();
    let _ = std::thread::Builder::new()
        .name("key-observer-spawn".into())
        .spawn(move || {
            match crate::helper::launch_key_observer(&name) {
                Ok(()) => {}
                Err(error) => {
                    if SPAWN_WARNED.get().is_none() {
                        let _ = SPAWN_WARNED.get_or_init(|| {
                            log::warn!(
                                "[key-observer] elevated observer unavailable: {error}. \
                                 Hold-to-compare over elevated (admin) windows will not hold \
                                 until the privileged helper task is registered."
                            );
                        });
                    } else {
                        log::debug!("[key-observer] observer spawn failed: {error}");
                    }
                }
            }
        });
}

// Query the elevated observer for the physical state of one combo. Returns
// None when no observer is connected (or this app runs elevated, where the
// local read already works). Callers fall back to local GetAsyncKeyState.
pub(crate) fn combo_held(vk: u32, mods: u32) -> Option<bool> {
    if SHUTDOWN.load(Ordering::Acquire) || self_elevated() {
        return None;
    }
    if !CONNECTED.load(Ordering::Acquire) {
        request_respawn();
        return None;
    }
    let sender = query_tx().lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone();
    let Some(sender) = sender else {
        return None;
    };
    let (reply_tx, reply_rx) = mpsc::sync_channel::<bool>(1);
    if sender
        .send(Query {
            vk: vk as u16,
            mods,
            reply: reply_tx,
        })
        .is_err()
    {
        return None;
    }
    match reply_rx.recv_timeout(Duration::from_millis(100)) {
        Ok(held) => Some(held),
        Err(_) => None,
    }
}

// Start the pipe server once and spawn the initial observer. Called from
// hotkeys::init (a hold_compare binding exists, so the observer will be
// used). Never fails: without the helper task the spawn logs once and the
// app falls back to local key reads.
pub(crate) fn start() {
    static STARTED: OnceLock<()> = OnceLock::new();
    let _ = STARTED.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("key-observer-server".into())
            .spawn(server_thread_loop);
        if !self_elevated() {
            request_respawn();
        }
    });
}

pub(crate) fn shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
    // The server thread is a daemon: it may be blocked in ConnectNamedPipe
    // or a pipe read, so it is not joined. Closing its handle from here
    // would race those calls; the process exits right after shutdown, which
    // releases the handle and ends the thread.
}

// ── Elevated side: the observer child ────────────────────────────────
//
// Reached from helper.rs run_privileged_helper for op "key-observer". The
// request file already carried the pipe name. Connects to the app's pipe and
// answers "is the combo held?" queries until the app disconnects (app exit,
// which also exits this child since the pipe breaks).

pub(crate) fn run_elevated_observer(pipe_name: &str) {
    if !crate::process::process_is_elevated() {
        return;
    }
    let name = wide(pipe_name);
    // Retry the open: the app may still be creating the pipe when the
    // scheduled task starts this child. 40 attempts x 150 ms (~6 s).
    for _ in 0..40 {
        let handle = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                GENERIC_READ_WRITE,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        };
        let Ok(handle) = handle else {
            std::thread::sleep(Duration::from_millis(150));
            continue;
        };
        let mut carry: Vec<u8> = Vec::new();
        loop {
            let Some(line) = read_line(handle, &mut carry) else {
                break;
            };
            let Ok(query) = serde_json::from_slice::<KeyQuery>(&line) else {
                continue;
            };
            let held =
                crate::hotkeys::combo_is_down(query.vk as u32, query.mods);
            let reply = serde_json::to_string(&KeyReply { held }).unwrap_or_default();
            if !write_all(handle, format!("{reply}\n").as_bytes()) {
                break;
            }
        }
        let _ = unsafe { CloseHandle(handle) };
        return;
    }
    log::debug!("[key-observer] could not connect to the app pipe within the retry window");
}
