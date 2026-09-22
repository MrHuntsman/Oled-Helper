// Favorite "trusted on this monitor" credit.
//
// A favorite may skip the "Keep display mode?" dialog only after it was
// confirmed once on the monitor it was saved on (see config's
// skip_favorite_confirmation). That credit used to be recorded by the main
// window's frontend on flyout-completed, which meant a favorite applied from
// the tray menu, or from a global hotkey while the app sat in the tray, could
// never become trusted: those paths run with no main window present to record
// it, so the user was asked again on every apply no matter how many times the
// Keep button was clicked.
//
// The credit now lives in the backend. Every apply path armed with a favorite
// id (in-app menu, in-app hotkey, tray menu, tray hotkey) calls arm() before
// the confirmation dialog is shown, and a successful Keep
// (confirm_display_mode_change) commits it by writing confirmed_monitor_id
// into that favorite and persisting the config. A failed confirm, a Revert,
// and the automatic timeout rollback all disarm it instead.

use std::sync::{Mutex, OnceLock};
use tauri::{AppHandle, Emitter};

// The favorite id of the display-mode transaction currently awaiting
// confirmation, if that transaction was started for a favorite. None for a
// plain resolution or refresh-rate change. Overwritten by each arm(), so the
// slot can never describe an older, already-resolved transaction.
static PENDING: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn pending() -> &'static Mutex<Option<String>> {
    PENDING.get_or_init(|| Mutex::new(None))
}

/// Remembers that the pending display-mode transaction belongs to this
/// favorite, so a later Keep can mark it trusted. An empty id clears the slot:
/// a favorite without a stable id cannot be resolved at commit time anyway.
pub(crate) fn arm(fav_id: &str) {
    let id = fav_id.trim();
    if let Ok(mut slot) = pending().lock() {
        *slot = if id.is_empty() {
            None
        } else {
            Some(id.to_string())
        };
    }
}

/// Forgets the credit without recording it: the apply failed, the user
/// reverted, or the confirmation deadline expired.
pub(crate) fn disarm() {
    if let Ok(mut slot) = pending().lock() {
        *slot = None;
    }
}

/// Records the armed favorite as trusted on its own monitor. Called only when
/// the temporary mode was actually kept.
///
/// The config is re-read here rather than trusting an in-memory copy, the same
/// read-modify-write the tray menu's own favorite saves use, so a favorite
/// added or removed from another window in the meantime is not clobbered. The
/// new credit is then emitted to the main window: its in-memory favorites list
/// drives later config saves, and a stale confirmed_monitor_id there would
/// write the credit back out as empty.
pub(crate) fn commit(app: &AppHandle) {
    let fav_id = match pending().lock() {
        Ok(mut slot) => match slot.take() {
            Some(id) => id,
            None => return,
        },
        Err(_) => return,
    };
    let mut config = crate::config::load_config(app.clone());
    let Some(favorite) = config
        .favorite_display_modes
        .iter_mut()
        .find(|favorite| favorite.fav_id == fav_id)
    else {
        log::warn!("[favorite] the kept mode change had no matching favorite {fav_id}");
        return;
    };
    // A favorite with no monitor identity can never satisfy the skip check, so
    // there is nothing worth recording for it.
    if favorite.monitor_id.is_empty() {
        return;
    }
    favorite.confirmed_monitor_id = favorite.monitor_id.clone();
    let monitor_id = favorite.monitor_id.clone();
    if let Err(error) = crate::config::save_config(app.clone(), config) {
        log::warn!("[favorite] could not persist the confirmation credit: {error}");
        return;
    }
    log::info!("[favorite] {fav_id} confirmed on {monitor_id}, it may now skip the dialog");
    let _ = app.emit_to(
        crate::main_window::LABEL,
        "favorite-confirmed",
        serde_json::json!({ "fav_id": fav_id, "confirmed_monitor_id": monitor_id }),
    );
}
