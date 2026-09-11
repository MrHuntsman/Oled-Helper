// Early pre-Tauri file logger.
//
// The tauri-plugin-log file target only exists after the plugin's setup runs,
// which is deep inside Tauri/WebView2 initialization. Everything the process
// does before that point (the restart handoff's parent wait, the
// single-instance lock race, a WebView2 init failure, the pre-logger crash of
// a UAC child) was emitted into the void, which made the tray-restart failure
// of 2026-09-10 effectively undiagnosable: the replacement process wrote zero
// lines during its entire 19-second wait.
//
// This module installs a logger at the very top of run(), before any Tauri
// init. It appends to the exact same log file the plugin will use, with the
// plugin's exact line format, rotation and prune rules. The global logger
// slot it occupies is later handed to the plugin: the plugin's
// Builder::split() assembles its full logger (all targets, filters, format)
// without registering it globally, and setup() fills this logger's delegate
// with it. Because log::set_boxed_logger can only succeed once per process,
// the swap happens inside this logger: records then flow to the plugin's
// targets (file, webview, history dispatch) and the early file sink is
// disabled, so no line is written twice.
//
// Level policy matches the plugin config in lib.rs: Info globally, Trace for
// the app's own modules.

use log::LevelFilter;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

// Format copied from tauri-plugin-log's desktop default formatter:
// "[[year]-[month]-[day]][[hour]:[minute]:[second]]" including the brackets,
// so early lines and plugin lines are byte-identical.
const TIME_FORMAT: &[time::format_description::FormatItem<'static>] =
    time::macros::format_description!("[[[year]-[month]-[day]][[[hour]:[minute]:[second]]");

// Mirrors tauri-plugin-log's RotatingFile defaults plus the app's overrides:
// FileOpenStrategy::Append, TimezoneStrategy::UseUtc, and from lib.rs a
// 5,000,000-byte cap with KeepSome(5) pruning.
const MAX_FILE_SIZE: u64 = 5_000_000;
const KEEP_ARCHIVED: usize = 5;

// The early sink must never leave the file at or above the plugin's cap:
// the plugin's RotatingFile::new rotates on open when the size already
// reached the cap, and that rename would fail while this sink still holds
// the file open, which would fail the whole plugin setup. This sink rotates
// early instead, so the plugin's rotate-on-open can never trigger.
const ROTATE_HEADROOM: u64 = 4096;

struct RotatingFile {
    dir: PathBuf,
    file_name: String,
    path: PathBuf,
    current_size: u64,
    file: Option<std::fs::File>,
    buffer: Vec<u8>,
}

impl RotatingFile {
    // The plugin opens with create + append and reads the size back to track
    // rotation across sessions; do the same so a mid-file early session does
    // not trip an immediate rotation.
    fn open(dir: PathBuf, file_name: String) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(&file_name).with_extension("log");
        let file = open_append(&path)?;
        let current_size = file.metadata()?.len();
        Ok(Self {
            dir,
            file_name,
            path,
            current_size,
            file: Some(file),
            buffer: Vec::new(),
        })
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        if let Some(mut file) = self.file.take() {
            let _ = file.flush();
        }
        if self.path.exists() {
            // KeepSome(n) keeps n - 1 archived files, so the newest archive
            // (this file) lands inside the cap, exactly like the plugin.
            let keep_archived = KEEP_ARCHIVED.saturating_sub(1);
            prune_archived(&self.dir, &self.file_name, keep_archived);
            let stamp = time::OffsetDateTime::now_utc()
                .format(TIME_FORMAT)
                .unwrap_or_default();
            let archived = self.dir.join(format!("{}_{}.log", self.file_name, stamp));
            let _ = std::fs::rename(&self.path, &archived);
        }
        self.file = Some(open_append(&self.path)?);
        self.current_size = 0;
        Ok(())
    }
}

// Append-mode open that shares the file for read, write, and delete. The
// watchdog and helper children run this logger too, so several processes hold
// handles on the same file at once, and a rotation (rename) must not be
// blocked by a handle that lacks FILE_SHARE_DELETE.
fn open_append(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .share_mode(0x7)
            .open(path)
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    }
}

impl std::io::Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        if self.current_size != 0
            && self.current_size + self.buffer.len() as u64 > MAX_FILE_SIZE - ROTATE_HEADROOM
        {
            self.rotate()?;
        }
        if let Some(file) = self.file.as_mut() {
            file.write_all(&self.buffer)?;
            self.current_size += self.buffer.len() as u64;
            file.flush()?;
        }
        self.buffer.clear();
        Ok(())
    }
}

// Same prune rule as the plugin's RotatingFile::remove_old_files: archived
// names are "<name>_<stamp>.log" and the active "<name>.log" is excluded.
fn prune_archived(dir: &std::path::Path, file_name: &str, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let active = format!("{}.log", file_name);
    let prefix = format!("{}_", file_name);
    let mut archived: Vec<(std::path::PathBuf, String)> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == active || !name.starts_with(&prefix) || !name.ends_with(".log") {
                return None;
            }
            let stamp = name.strip_prefix(&prefix)?.strip_suffix(".log")?.to_string();
            Some((entry.path(), stamp))
        })
        .collect();
    archived.sort_by(|a, b| a.1.cmp(&b.1));
    let excess = archived.len().saturating_sub(keep);
    for (path, _) in archived.iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

// One lock over both phases, so the handoff is atomic per record: a record
// either sees the early sink or the delegate, never a window where both are
// gone (which would drop the record).
struct State {
    sink: Option<RotatingFile>,
    delegate: Option<Box<dyn log::Log>>,
}

static STATE: Mutex<State> = Mutex::new(State {
    sink: None,
    delegate: None,
});
// Guards against a second init (and documents the handoff for diagnostics).
static INSTALLED: AtomicBool = AtomicBool::new(false);
static HANDOFF_DONE: AtomicBool = AtomicBool::new(false);

fn early_enabled(metadata: &log::Metadata) -> bool {
    // Same policy as the plugin config: app modules at Trace, the rest at
    // Info. Targets are module paths, so a prefix match on "app_lib" (with
    // the crate root itself covered by the same prefix) mirrors the plugin's
    // level_for("app_lib", Trace).
    metadata.level() <= LevelFilter::Info
        || (metadata.target().starts_with("app_lib") && metadata.level() <= LevelFilter::Trace)
}

struct EarlyLogger;

impl log::Log for EarlyLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        let state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        match state.delegate.as_ref() {
            Some(delegate) => delegate.enabled(metadata),
            None => early_enabled(metadata),
        }
    }

    fn log(&self, record: &log::Record) {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(delegate) = state.delegate.as_ref() {
            delegate.log(record);
            return;
        }
        let Some(sink) = state.sink.as_mut() else {
            return;
        };
        if !early_enabled(record.metadata()) {
            return;
        }
        // Same format as the plugin's desktop default formatter, plus the
        // trailing newline its fern writer target adds.
        let stamp = time::OffsetDateTime::now_utc()
            .format(TIME_FORMAT)
            .unwrap_or_default();
        let _ = write!(
            sink,
            "{}[{}][{}] {}\n",
            stamp,
            record.target(),
            record.level(),
            record.args()
        );
        let _ = sink.flush();
    }

    fn flush(&self) {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(delegate) = state.delegate.as_ref() {
            delegate.flush();
            return;
        }
        if let Some(sink) = state.sink.as_mut() {
            let _ = sink.flush();
        }
    }
}

// Installs the early global logger. Call at the very top of run(), before any
// Tauri init. Best effort: if the file cannot be opened or another logger
// already owns the global slot (unit tests), early records stay invisible and
// nothing else changes.
pub fn init() {
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    let sink = log_dir()
        .map(|dir| RotatingFile::open(dir, file_name()))
        .map(|result| result.ok())
        .unwrap_or(None);
    if let Ok(mut state) = STATE.lock() {
        state.sink = sink;
    }
    // If this fails (a logger is already installed, e.g. in tests) the sink
    // simply stays unused.
    let _ = log::set_boxed_logger(Box::new(EarlyLogger));
    log::set_max_level(LevelFilter::Trace);
}

// Fills the delegate with the plugin's assembled logger. Call this with the
// result of tauri_plugin_log::Builder::split() from setup(), before any other
// setup logging. From that point records flow through the plugin's targets
// and the early file sink is disabled, so the two never write the same line.
pub fn attach_plugin_logger(max_level: LevelFilter, delegate: Box<dyn log::Log>) {
    if HANDOFF_DONE.swap(true, Ordering::AcqRel) {
        return;
    }
    // Raise the max level first so no record is filtered by the macros while
    // the delegate takes over.
    log::set_max_level(max_level);
    if let Ok(mut state) = STATE.lock() {
        state.delegate = Some(delegate);
        // The delegate now serves every record; emptying the sink here under
        // the same lock keeps early and plugin lines from ever duplicating.
        state.sink = None;
    }
}

// Same directory the plugin's TargetKind::LogDir resolves to on Windows:
// %LOCALAPPDATA%/<identifier>/logs. Resolved without an AppHandle because no
// Tauri runtime exists yet; the identifier comes from the embedded
// tauri.conf.json (helper::app_data_dir uses the same trick).
fn log_dir() -> Option<PathBuf> {
    #[derive(serde::Deserialize)]
    struct Conf {
        identifier: String,
    }
    let conf: Conf = serde_json::from_str(include_str!("../tauri.conf.json")).ok()?;
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var_os("LOCALAPPDATA")?;
        Some(PathBuf::from(base).join(conf.identifier).join("logs"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Matches the plugin's LogDir on the other desktop platforms; the app
        // only really ships on Windows, this keeps non-Windows builds valid.
        let base = std::env::var_os("HOME")?;
        Some(
            PathBuf::from(base)
                .join(".local")
                .join("share")
                .join(conf.identifier)
                .join("logs"),
        )
    }
}

// The plugin names the file after package_info().name, which comes from
// tauri.conf.json's productName. RotatingFile strips/derives the extension,
// so the base name must be the bare product name.
fn file_name() -> String {
    #[derive(serde::Deserialize)]
    #[allow(non_snake_case)]
    struct Conf {
        productName: String,
    }
    serde_json::from_str::<Conf>(include_str!("../tauri.conf.json"))
        .map(|conf| conf.productName)
        .unwrap_or_else(|_| "app".to_string())
}
