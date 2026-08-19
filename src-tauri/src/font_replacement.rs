// System font replacement (milestone M5) — backend.
//
// The eventual goal is to let the user pick a UI font and apply it
// system-wide by impersonating the Segoe UI family (see
// the text-rendering milestones plan, "M5 — System font replacement"). This module
// is built in steps:
//
//   Step 1 (this file): enumerate + inspect installed TrueType fonts so the
//   frontend can offer a picker. Read-only, no admin, no writes.
//
// Later steps add fontTools-style name-table impersonation (read-fonts +
// write-fonts), boot-deferred file replacement, cache purge, and the
// apply/restore commands.
//
// Log prefix: [font]

use serde::{Deserialize, Serialize};
use tauri::Emitter;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT, MOVEFILE_REPLACE_EXISTING,
};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, HWND_BROADCAST, WM_FONTCHANGE};
use write_fonts::read::{FontRef, TableProvider};
use write_fonts::types::Tag;
use write_fonts::FontBuilder;

/// A TrueType font the picker can offer. Weight is the OpenType usWeightClass
/// (100..=900); `variable` means the file carries an `fvar` table; `mono` is
/// the `post` table's fixed-pitch flag.
#[derive(Serialize, Clone)]
pub struct InstalledFont {
    pub family: String,
    pub subfamily: String,
    pub weight: u16,
    pub italic: bool,
    pub variable: bool,
    pub mono: bool,
    /// Symbol/icon fonts (Wingdings, Segoe UI Symbol, …) are filtered from the
    /// picker: their cmap maps to the private-use area, so they'd render as
    /// icons rather than letters if applied as the UI font.
    pub symbol: bool,
    pub path: String,
}

/// Directories scanned for user-selectable fonts: the system font store plus
/// the per-user store. `%WINDIR%\Fonts` may contain `.fon`/`.otf`/collections
/// we deliberately skip for now — only `.ttf` is supported (matches
/// FontWizard, and keeps the later name-table rewrite simple).
fn windows_font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let windir = std::env::var("WINDIR").or_else(|_| std::env::var("SystemRoot"));
    if let Ok(windir) = windir {
        dirs.push(PathBuf::from(windir).join("Fonts"));
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local).join("Microsoft").join("Windows").join("Fonts"));
    }
    dirs
}

/// The family name of a parsed face: prefer the typographic family (name ID
/// 16) and fall back to the legacy family (ID 1). Google Fonts statics and the
/// Segoe variants split this way — e.g. `Roboto-Light.ttf` reports ID1
/// "Roboto Light" with the real family "Roboto" only in ID16, and the core
/// weights (Regular/Bold/…) have no ID16 at all. Matching siblings by ID1
/// alone silently misses every non-core weight.
fn family_name(face: &ttf_parser::Face) -> Option<String> {
    face.names()
        .into_iter()
        .find(|n| n.name_id == ttf_parser::name_id::TYPOGRAPHIC_FAMILY && n.is_unicode())
        .and_then(|n| n.to_string())
        .or_else(|| {
            face.names()
                .into_iter()
                .find(|n| n.name_id == ttf_parser::name_id::FAMILY && n.is_unicode())
                .and_then(|n| n.to_string())
        })
}

/// Parse one .ttf file into an `InstalledFont`. Returns `None` for files that
/// are not parseable TrueType (bad data, collections, or non-font files).
fn inspect_font_file(path: &Path) -> Option<InstalledFont> {
    let data = std::fs::read(path).ok()?;
    let face = ttf_parser::Face::parse(&data, 0).ok()?;

    let family = family_name(&face).unwrap_or_else(|| {
        path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    });

    let subfamily = face
        .names()
        .into_iter()
        .find(|n| n.name_id == ttf_parser::name_id::SUBFAMILY && n.is_unicode())
        .and_then(|n| n.to_string())
        .unwrap_or_default();

    Some(InstalledFont {
        family,
        subfamily,
        weight: face.weight().to_number(),
        italic: face.is_italic(),
        variable: face.is_variable(),
        mono: face.is_monospaced(),
        symbol: is_symbol_font(&face),
        path: path.to_string_lossy().into_owned(),
    })
}

/// Scan the Windows + per-user font stores for `.ttf` files and return one
/// entry per file, de-duplicated by path. Families with several styles
/// (Regular/Bold/Italic/…) show up once per file so the picker can group by
/// `family`.
/// Fonts the picker can offer. Variable fonts are excluded until the
/// auto-instantiate feature lands (a single variable file can't fill the 12
/// static weight slots correctly — every weight collapses to its default
/// instance), and monospace fonts are excluded because they'd render the whole
/// proportional UI monospaced.
fn is_compatible(font: &InstalledFont) -> bool {
    !font.variable && !font.mono && !font.symbol
}

/// True when a face's cmap is symbol-only: a Windows (platform 3) subtable
/// with encoding 0 (symbol) and no Unicode-compatible subtable. That is how
/// icon/symbol fonts (Wingdings, Webdings, Marlett, Segoe UI Symbol, …)
/// register — replacing the UI font with one would turn every letter into a
/// glyph from the private-use area.
fn is_symbol_font(face: &ttf_parser::Face) -> bool {
    let Some(cmap) = face.tables().cmap else {
        return false;
    };
    let mut has_symbol = false;
    let mut has_unicode = false;
    for i in 0..cmap.subtables.len() {
        let Some(subtable) = cmap.subtables.get(i) else { continue };
        if subtable.is_unicode() {
            has_unicode = true;
        } else if subtable.platform_id == ttf_parser::PlatformId::Windows && subtable.encoding_id == 0 {
            has_symbol = true;
        }
    }
    has_symbol && !has_unicode
}

pub fn enumerate_installed_fonts() -> Vec<InstalledFont> {
    let mut seen = std::collections::HashSet::new();
    let mut fonts = Vec::new();
    let mut skipped = 0usize;
    let mut incompatible = 0usize;

    for dir in windows_font_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_ttf = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("ttf"))
                .unwrap_or(false);
            if !is_ttf {
                continue;
            }
            let key = path.to_string_lossy().to_lowercase();
            if !seen.insert(key) {
                continue;
            }
            match inspect_font_file(&path) {
                Some(font) if is_compatible(&font) => fonts.push(font),
                Some(_) => incompatible += 1,
                None => skipped += 1,
            }
        }
    }

    fonts.sort_by(|a, b| a.family.cmp(&b.family).then(a.subfamily.cmp(&b.subfamily)));
    log::info!(
        "[font] enumerated {} compatible .ttf fonts ({} unparseable skipped, {} incompatible variable/mono/symbol excluded)",
        fonts.len(),
        skipped,
        incompatible
    );
    fonts
}

/// Tauri command: list installed TrueType fonts for the picker.
#[tauri::command]
pub fn font_list_installed() -> Vec<InstalledFont> {
    enumerate_installed_fonts()
}

// ── Step 2: name-table impersonation ────────────────────────────────

/// Rebuild `source` so its `name` table impersonates `donor` (the Segoe UI
/// family file being replaced). Every other table is copied verbatim from
/// `source`, and `DSIG` is dropped because a digital signature over the old
/// bytes would no longer match the rebuilt font.
///
/// FontBuilder reassembles the table directory and recomputes the
/// head.checksumAdjustment, so the result is a valid font even though the
/// name table came from a different file. Weight/style metadata (OS/2
/// usWeightClass/fsSelection, head.macStyle, post.italicAngle) is preserved
/// from the source — correct when the source's weight/style matches the slot
/// it fills, which the weight auto-detection in step 3 guarantees.
pub fn build_impersonated_font(source: &[u8], donor: &[u8]) -> Result<Vec<u8>, String> {
    let donor_font = FontRef::new(donor).map_err(|e| format!("donor font parse failed: {e}"))?;
    let source_font = FontRef::new(source).map_err(|e| format!("source font parse failed: {e}"))?;

    const NAME: Tag = Tag::new(b"name");
    const DSIG: Tag = Tag::new(b"DSIG");

    let name_data = donor_font
        .data_for_tag(NAME)
        .ok_or_else(|| "donor font has no name table".to_string())?;

    let mut builder = FontBuilder::new();
    for record in source_font.table_directory().table_records() {
        let tag = record.tag();
        if tag == NAME || tag == DSIG {
            continue;
        }
        match source_font.data_for_tag(tag) {
            Some(data) => {
                builder.add_raw(tag, data.as_bytes());
            }
            None => log::warn!("[font] skipping malformed table {tag} in source font"),
        }
    }
    builder.add_raw(NAME, name_data.as_bytes());
    Ok(builder.build())
}

// ── Step 3: system font manifest + weight detection + artifact build ──

/// One slot in the Windows UI font family: a canonical file in
/// `%WINDIR%\Fonts` we can replace in-place (the filename is preserved, so
/// the `HKLM ...\Fonts` name→file mapping stays valid).
#[derive(Clone, Copy)]
struct SystemFontSlot {
    key: &'static str,
    filename: &'static str,
    weight_class: u16,
    italic: bool,
    /// True for the monospace (Consolas) slots. These are NOT replaced with the
    /// chosen proportional font — putting a proportional font into a monospace
    /// slot breaks column alignment in terminals/VS Code, which reads as huge
    /// gaps. A mono companion is a future option; for now they stay untouched.
    mono: bool,
}

const SYSTEM_FONT_SLOTS: &[SystemFontSlot] = &[
    SystemFontSlot { key: "regular", filename: "segoeui.ttf", weight_class: 400, italic: false, mono: false },
    SystemFontSlot { key: "bold", filename: "segoeuib.ttf", weight_class: 700, italic: false, mono: false },
    SystemFontSlot { key: "italic", filename: "segoeuii.ttf", weight_class: 400, italic: true, mono: false },
    SystemFontSlot { key: "bold_italic", filename: "segoeuiz.ttf", weight_class: 700, italic: true, mono: false },
    SystemFontSlot { key: "light", filename: "segoeuil.ttf", weight_class: 300, italic: false, mono: false },
    SystemFontSlot { key: "semilight", filename: "segoeuisl.ttf", weight_class: 350, italic: false, mono: false },
    SystemFontSlot { key: "semibold", filename: "seguisb.ttf", weight_class: 600, italic: false, mono: false },
    SystemFontSlot { key: "black", filename: "seguibl.ttf", weight_class: 900, italic: false, mono: false },
    SystemFontSlot { key: "black_italic", filename: "seguibli.ttf", weight_class: 900, italic: true, mono: false },
    SystemFontSlot { key: "light_italic", filename: "seguili.ttf", weight_class: 300, italic: true, mono: false },
    SystemFontSlot { key: "semibold_italic", filename: "seguisbi.ttf", weight_class: 600, italic: true, mono: false },
    SystemFontSlot { key: "semilight_italic", filename: "seguisli.ttf", weight_class: 350, italic: true, mono: false },
    SystemFontSlot { key: "variable", filename: "SegUIVar.ttf", weight_class: 400, italic: false, mono: false },
    SystemFontSlot { key: "consolas_regular", filename: "consola.ttf", weight_class: 400, italic: false, mono: true },
    SystemFontSlot { key: "consolas_bold", filename: "consolab.ttf", weight_class: 700, italic: false, mono: true },
    SystemFontSlot { key: "consolas_italic", filename: "consolai.ttf", weight_class: 400, italic: true, mono: true },
    SystemFontSlot { key: "consolas_bold_italic", filename: "consolaz.ttf", weight_class: 700, italic: true, mono: true },
];

fn system_fonts_root() -> PathBuf {
    let windir = std::env::var("WINDIR").or_else(|_| std::env::var("SystemRoot"));
    windir.map_or_else(|_| PathBuf::from(r"C:\Windows"), |w| PathBuf::from(w).join("Fonts"))
}

/// Slots whose canonical file actually exists on this system (Win10 lacks
/// `SegUIVar.ttf`; the Consolas slots always exist).
fn manifest_slots() -> Vec<SystemFontSlot> {
    let root = system_fonts_root();
    SYSTEM_FONT_SLOTS
        .iter()
        .copied()
        .filter(|slot| root.join(slot.filename).is_file())
        .collect()
}

fn family_of(data: &[u8]) -> Option<String> {
    let face = ttf_parser::Face::parse(data, 0).ok()?;
    family_name(&face)
}

/// The canonical family name a slot must report. Read from the live file at
/// runtime because the static Segoe files do NOT share one family name:
/// Regular/Bold/Italic/BoldItalic are "Segoe UI", but Light/Semilight/
/// Semibold/Black are their own families ("Segoe UI Light", …). Impersonation
/// copies the donor's name table, so this stays stable across a replacement
/// and is the right key for WinSxS matching.
fn canonical_family(slot: &SystemFontSlot) -> Option<String> {
    let path = system_fonts_root().join(slot.filename);
    family_of(&std::fs::read(path).ok()?)
}

/// Reverse the (weight, italic) axis back to a slot key, so we can tell which
/// weight a sibling file fills. Non-standard weights are left unmatched.
fn slot_key_for(weight: u16, italic: bool) -> Option<&'static str> {
    Some(match (weight, italic) {
        (400, false) => "regular",
        (700, false) => "bold",
        (400, true) => "italic",
        (700, true) => "bold_italic",
        (300, false) => "light",
        (350, false) => "semilight",
        (600, false) => "semibold",
        (900, false) => "black",
        (900, true) => "black_italic",
        (300, true) => "light_italic",
        (600, true) => "semibold_italic",
        (350, true) => "semilight_italic",
        _ => return None,
    })
}

/// Given the user's chosen regular font, find sibling `.ttf` files in the same
/// directory that carry the same family name and map them onto the Segoe
/// weight slots. The primary file fills whichever slot matches its own
/// weight/style, and any slot left unmatched falls back to the primary.
pub fn detect_weight_files(primary: &Path) -> std::collections::HashMap<String, PathBuf> {
    let mut out = std::collections::HashMap::new();
    let Ok(primary_data) = std::fs::read(primary) else {
        return out;
    };
    let Some(primary_family) = family_of(&primary_data) else {
        return out;
    };

    if let Some(dir) = primary.parent() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()).map(|e| !e.eq_ignore_ascii_case("ttf")).unwrap_or(true) {
                continue;
            }
            let Ok(data) = std::fs::read(&path) else { continue };
            let Some(family) = family_of(&data) else { continue };
            if !family.eq_ignore_ascii_case(&primary_family) {
                continue;
            }
            let Ok(face) = ttf_parser::Face::parse(&data, 0) else { continue };
            let key = slot_key_for(face.weight().to_number(), face.is_italic());
            if let Some(key) = key {
                out.entry(key.to_string()).or_insert_with(|| path.clone());
            }
        }
    }

    // The primary always wins its own slot; every missing slot falls back to it.
    let primary_key = std::fs::read(primary)
        .ok()
        .and_then(|data| {
            let face = ttf_parser::Face::parse(&data, 0).ok()?;
            Some(slot_key_for(face.weight().to_number(), face.is_italic()).unwrap_or("regular"))
        })
        .unwrap_or("regular");
    out.insert(primary_key.to_string(), primary.to_path_buf());
    for slot in manifest_slots() {
        out.entry(slot.key.to_string()).or_insert_with(|| primary.to_path_buf());
    }
    out
}

/// A built font ready to be staged into `%WINDIR%\Fonts`.
#[derive(Serialize, Clone)]
pub struct Artifact {
    pub key: String,
    pub filename: String,
    pub staging_path: String,
    pub sha256: String,
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(data);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Build an impersonated copy of the user's font for every system slot,
/// writing each to `staging`. Each artifact's donor is the canonical file for
/// that slot, and the result is verified to report the donor's family name.
pub fn build_all_artifacts(primary: &Path, staging: &Path) -> Result<Vec<Artifact>, String> {
    let sources = detect_weight_files(primary);
    let root = system_fonts_root();
    let mut artifacts = Vec::new();

    // Monospace slots are left alone: a proportional font breaks column
    // alignment in terminals and VS Code.
    for slot in manifest_slots().into_iter().filter(|s| !s.mono) {
        let donor_path = root.join(slot.filename);
        let donor = std::fs::read(&donor_path)
            .map_err(|e| format!("could not read donor {}: {e}", donor_path.display()))?;
        let source_path = sources
            .get(slot.key)
            .cloned()
            .unwrap_or_else(|| primary.to_path_buf());
        let source = std::fs::read(&source_path)
            .map_err(|e| format!("could not read source {}: {e}", source_path.display()))?;

        // Warn when a slot is filled by a weight-mismatched source (the
        // primary fallback) — e.g. no Bold sibling exists, so the Regular
        // file ends up filling the "bold" slot. Useful for diagnosing why a
        // chosen family's bold looks like its regular.
        if let Some(face) = ttf_parser::Face::parse(&source, 0).ok() {
            let (w, i) = (face.weight().to_number(), face.is_italic());
            if w != slot.weight_class || i != slot.italic {
                log::warn!(
                    "[font] slot {} (weight {} italic {}) filled by {} (weight {w} italic {i})",
                    slot.filename,
                    slot.weight_class,
                    slot.italic,
                    source_path.display()
                );
            }
        }

        let built = build_impersonated_font(&source, &donor)?;

        // Verify the result reports the donor's family before trusting it.
        let donor_family = family_of(&donor).unwrap_or_default();
        let built_family = family_of(&built).unwrap_or_default();
        if !built_family.eq_ignore_ascii_case(&donor_family) {
            return Err(format!(
                "built {} reported family {built_family:?}, expected {:?}",
                slot.filename, donor_family
            ));
        }

        let out_path = staging.join(slot.filename);
        std::fs::write(&out_path, &built)
            .map_err(|e| format!("could not write {}: {e}", out_path.display()))?;
        artifacts.push(Artifact {
            key: slot.key.to_string(),
            filename: slot.filename.to_string(),
            staging_path: out_path.to_string_lossy().into_owned(),
            sha256: sha256_hex(&built),
        });
    }

    log::info!("[font] built {} impersonated artifacts in {}", artifacts.len(), staging.display());
    Ok(artifacts)
}

// ── Step 4: backup + WinSxS authentic-source scan ───────────────────

/// Runtime data root for this feature (build staging, backups, state). Kept
/// under %LOCALAPPDATA% so no admin is needed to build before the apply step.
fn runtime_root() -> PathBuf {
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    PathBuf::from(local).join("OledHelper").join("font-replace")
}

fn backup_root() -> PathBuf {
    runtime_root().join("backup")
}

/// Back up an authentic copy of a slot's font so it can be restored. The
/// source is always the authentic WinSxS component copy (which is what the
/// canonical file was before any replacement, regardless of apply/restore
/// history); the live `%WINDIR%\Fonts` file is only a fallback when WinSxS
/// has no copy, since the live file may already be impersonated and would
/// silently poison the backup (a restore would then "restore" the previous
/// replacement). The WinSxS index is cached, so this needs no per-slot walk.
fn backup_original(slot: &SystemFontSlot) -> Result<PathBuf, String> {
    let root = system_fonts_root();
    let live = root.join(slot.filename);
    let backup = backup_root().join(slot.filename);

    let source = find_winsxs_authentic(slot)
        .or_else(|| live.is_file().then_some(live.clone()))
        .ok_or_else(|| format!("no authentic copy of {} found for backup", slot.filename))?;
    std::fs::create_dir_all(backup_root()).map_err(|e| e.to_string())?;
    // Skip the copy when the backup already holds the same authentic bytes
    // (a previous apply backed this slot up). Switching fonts re-applies the
    // whole slot list, so this makes the "Backing up originals" step instant
    // on every run after the first. A mismatch still re-copies, which also
    // self-heals a backup written by an old buggy build that stored a
    // replaced font as "original".
    if let (Ok(existing), Ok(auth)) = (std::fs::read(&backup), std::fs::read(&source)) {
        if existing == auth {
            log::debug!("[font] backup for {} already current", slot.filename);
            return Ok(backup);
        }
    }
    std::fs::copy(&source, &backup)
        .map_err(|e| format!("could not back up {} from {}: {e}", slot.filename, source.display()))?;
    log::info!("[font] backed up {} from {}", slot.filename, source.display());
    Ok(backup)
}

/// Scan `%WINDIR%\WinSxS` once for the authentic Microsoft copies of the
/// canonical font files. WinSxS is a huge tree, so walking it per slot (as the
/// original code did) made each backup take ~6s and froze the shell — hence a
/// single cached walk, indexed by lowercased filename with the parsed family
/// name stored alongside.
fn winsxs_index() -> &'static std::collections::HashMap<String, Vec<(u64, PathBuf, Option<String>)>> {
    static INDEX: std::sync::OnceLock<
        std::collections::HashMap<String, Vec<(u64, PathBuf, Option<String>)>>,
    > = std::sync::OnceLock::new();
    INDEX.get_or_init(build_winsxs_index)
}

fn build_winsxs_index() -> std::collections::HashMap<String, Vec<(u64, PathBuf, Option<String>)>> {
    let mut index: std::collections::HashMap<String, Vec<(u64, PathBuf, Option<String>)>> =
        std::collections::HashMap::new();
    let winsxs = {
        let windir = std::env::var("WINDIR").or_else(|_| std::env::var("SystemRoot"));
        windir.map_or_else(|_| PathBuf::from(r"C:\Windows\WinSxS"), |w| PathBuf::from(w).join("WinSxS"))
    };
    if !winsxs.is_dir() {
        return index;
    }

    let mut stack = vec![winsxs];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                let name = entry.file_name().to_string_lossy().to_lowercase();
                if name == "manifests" || name == "backup" || name == "r" {
                    continue;
                }
                stack.push(path);
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !SYSTEM_FONT_SLOTS.iter().any(|s| s.filename.eq_ignore_ascii_case(&name)) {
                continue;
            }
            let Ok(data) = std::fs::read(&path) else { continue };
            let size = data.len() as u64;
            let family = family_of(&data);
            index
                .entry(name.to_lowercase())
                .or_insert_with(Vec::new)
                .push((size, path, family));
        }
    }
    index
}

/// Find the authentic Microsoft copy of a slot, matched by filename + the
/// family name the canonical file must report. Prefers the largest match when
/// several component versions exist.
fn find_winsxs_authentic(slot: &SystemFontSlot) -> Option<PathBuf> {
    let expected = canonical_family(slot)?;
    let candidates = winsxs_index().get(&slot.filename.to_lowercase())?;
    let mut best: Option<(u64, PathBuf)> = None;
    for (size, path, family) in candidates {
        if family.as_ref().map(|f| f.eq_ignore_ascii_case(&expected)).unwrap_or(false) {
            if best.as_ref().map(|(s, _)| size > s).unwrap_or(true) {
                best = Some((*size, path.clone()));
            }
        }
    }
    best.map(|(_, p)| p)
}

// ── Step 5: ownership + boot-deferred replacement + state ──────────

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Run a console tool with its window hidden and return its raw output.
fn run_hidden(cmd: &str, args: &[&str]) -> Result<std::process::Output, String> {
    std::process::Command::new(cmd)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("could not run {cmd}: {e}"))
}

/// Staging dir for the boot-time rename. It lives *inside* `%WINDIR%\Fonts` so
/// the rename is guaranteed to stay on the same volume as the destination (a
/// requirement of MOVEFILE_DELAY_UNTIL_REBOOT). A subdirectory of Fonts is not
/// enumerated as an installed font, so the staged files don't pollute the list.
fn staging_root() -> PathBuf {
    system_fonts_root().join("OledHelperStaging")
}

/// One unique id per apply/restore run, used to name staged files. This is
/// what makes repeated no-reboot cycles safe: Windows processes the pending
/// rename list in order at boot and moves each source file exactly once, so
/// two runs sharing a staged filename (the old code reused `segoeui.ttf`)
/// meant an earlier pending entry could consume the source file a later
/// entry still needed — Apply → Restore → Apply ended with the Restore's
/// original font winning at boot even though Apply was the last action. A
/// per-run unique name means every pending entry has its own source file and
/// the LAST scheduled run always wins, regardless of history.
fn next_batch_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{ms:x}-{n:04x}")
}

/// Remove every file the staging dir currently holds. Called at the start of
/// each apply/restore run: the run supersedes any previously scheduled
/// (un-rebooted) operations, whose pending entries then find a missing source
/// at boot and are harmlessly skipped. The directory is exclusively ours, so
/// deleting everything in it is safe.
fn clear_staging() {
    let root = staging_root();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Take ownership of a canonical font file and grant Administrators full
/// control. Segoe files are owned by TrustedInstaller, so even an elevated
/// process can't overwrite them without this (matches FontWizard's recipe).
/// Best effort: failures are logged, not fatal — the boot-time replace by
/// Session Manager may still succeed because it runs as SYSTEM.
fn take_ownership(path: &Path) {
    let p = path.to_string_lossy().into_owned();
    match run_hidden("takeown", &["/f", &p, "/a"]) {
        Ok(out) if !out.status.success() => log::warn!(
            "[font] takeown {} failed: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => log::warn!("[font] takeown {} failed: {e}", path.display()),
        _ => {}
    }
    // *S-1-5-32-544 is BUILTIN\Administrators as a SID — the literal name
    // "Administrators" fails on non-English Windows ("No mapping between
    // account names and security IDs"), which is why the grant must use the SID.
    match run_hidden("icacls", &[&p, "/grant", "*S-1-5-32-544:F"]) {
        Ok(out) if !out.status.success() => log::warn!(
            "[font] icacls {} failed: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => log::warn!("[font] icacls {} failed: {e}", path.display()),
        _ => {}
    }
}

/// Schedule `staged` to atomically replace `dest` at the next boot. The move is
/// recorded in Session Manager's pending-file-operations list; no file bytes
/// are touched now, which is what dodges the FontCache file lock entirely.
fn schedule_replace(staged: &Path, dest: &Path) -> Result<(), String> {
    let src = wide(&staged.to_string_lossy());
    let dst = wide(&dest.to_string_lossy());
    // SAFETY: both pointers are valid NUL-terminated UTF-16 slices owned by
    // `wide`, and the flags only record a boot-time rename.
    unsafe {
        MoveFileExW(
            PCWSTR(src.as_ptr()),
            PCWSTR(dst.as_ptr()),
            MOVEFILE_DELAY_UNTIL_REBOOT | MOVEFILE_REPLACE_EXISTING,
        )
        .map_err(|e| {
            format!(
                "MoveFileExW boot replace {} -> {} failed: {e}",
                staged.display(),
                dest.display()
            )
        })?;
    }
    Ok(())
}

/// Stage a built artifact and schedule it to replace its canonical file at the
/// next boot. The staged file carries the run's unique batch id so a later
/// run can never clobber this pending operation's source (see next_batch_id).
fn apply_artifact(artifact: &Artifact, batch: &str) -> Result<(), String> {
    let dest = system_fonts_root().join(&artifact.filename);
    let staged = staging_root().join(format!("{}.{batch}", artifact.filename));
    std::fs::create_dir_all(staging_root()).map_err(|e| e.to_string())?;
    std::fs::copy(&artifact.staging_path, &staged)
        .map_err(|e| format!("could not stage {}: {e}", artifact.staging_path))?;
    take_ownership(&dest);
    schedule_replace(&staged, &dest)?;
    log::info!(
        "[font] scheduled {} -> {} at next boot",
        staged.display(),
        dest.display()
    );
    Ok(())
}

/// Persistent record of an applied replacement, kept in LOCALAPPDATA so the UI
/// can show "pending reboot" and offer restore even after a restart.
#[derive(Serialize, Deserialize, Clone)]
pub struct ReplaceState {
    pub family: String,
    pub applied_at_unix: u64,
    pub files: Vec<String>,
}

fn state_path() -> PathBuf {
    runtime_root().join("state.json")
}

pub fn load_state() -> Option<ReplaceState> {
    let data = std::fs::read(state_path()).ok()?;
    serde_json::from_slice(&data).ok()
}

fn save_state(state: &ReplaceState) -> Result<(), String> {
    std::fs::create_dir_all(runtime_root()).map_err(|e| e.to_string())?;
    let data = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    std::fs::write(state_path(), data).map_err(|e| e.to_string())
}

// ── Step 6: font cache purge + change broadcast ────────────────────

/// Delete the on-disk font cache files so Windows re-parses the rebuilt fonts
/// instead of serving stale cache entries. `net stop/start FontCache` needs
/// elevation. Only the real font caches are touched: `FNTCACHE.DAT` in
/// System32 and the `FontCache-*.dat` files in the service's cache dir — never
/// the unrelated .dat files (perf counters, NOISE.DAT, dssec.dat, …) that also
/// live in System32.
fn purge_font_cache() {
    let windir = std::env::var("WINDIR").or_else(|_| std::env::var("SystemRoot"));
    let windir = windir.unwrap_or_else(|_| "C:\\Windows".into());

    let _ = run_hidden("net", &["stop", "FontCache"]);

    let fntcache = PathBuf::from(&windir).join("System32").join("FNTCACHE.DAT");
    if std::fs::remove_file(&fntcache).is_ok() {
        log::debug!("[font] removed font cache {}", fntcache.display());
    }

    let fontcache_dir = PathBuf::from(&windir)
        .join("ServiceProfiles")
        .join("LocalService")
        .join("AppData")
        .join("Local")
        .join("FontCache");
    if let Ok(entries) = std::fs::read_dir(&fontcache_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dat = entry
                .path()
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("dat"))
                .unwrap_or(false);
            if name.starts_with("FontCache-") && is_dat {
                if std::fs::remove_file(entry.path()).is_ok() {
                    log::debug!("[font] removed font cache {}", entry.path().display());
                }
            }
        }
    }

    let _ = run_hidden("net", &["start", "FontCache"]);
}

/// Broadcast WM_FONTCHANGE so running processes reload their font enumeration
/// immediately (matches the explorer-hook broadcast).
fn broadcast_font_change() {
    // SAFETY: PostMessageW with HWND_BROADCAST and valid handles is always
    // safe; the message is a standard broadcast.
    unsafe {
        let _ = PostMessageW(Some(HWND_BROADCAST), WM_FONTCHANGE, WPARAM(0), LPARAM(0));
    }
}

// ── Progress events ────────────────────────────────────────────────

/// Emitted on `font-progress` while download/apply/restore runs (which take
/// several seconds end to end). The UI shows the current phase so the long
/// WinSxS backup walk isn't a silent freeze.
#[derive(Serialize, Clone)]
pub struct FontProgress {
    pub phase: String,
    pub detail: String,
}

fn emit_progress(app: Option<&tauri::AppHandle>, phase: &str, detail: &str) {
    if let Some(app) = app {
        let _ = app.emit(
            "font-progress",
            FontProgress {
                phase: phase.to_string(),
                detail: detail.to_string(),
            },
        );
    }
}

// ── Step 8: curated download catalog + Google Fonts fetch ──────────

/// A family the picker can offer to download on demand (the fonts are big, so
/// nothing is bundled with the app). Each entry is a Google Fonts family the
/// user can fetch the static weight files for and apply like an installed
/// font. The `filter` substring selects the *base* (normal width / normal
/// optical size) static set out of the manifest's full file list.
#[derive(Serialize, Clone)]
pub struct DownloadableFont {
    pub key: String,
    pub family: String,
    pub license: String,
    /// Substring that selects the base static set in the manifest (e.g.
    /// "Inter_18pt-" picks the normal optical-size Inter statics; Roboto's
    /// "Roboto-" skips the _Condensed/_SemiCondensed width variants).
    pub filter: String,
}

fn download_catalog() -> Vec<DownloadableFont> {
    vec![
        DownloadableFont { key: "inter".to_string(), family: "Inter".to_string(), license: "SIL Open Font License".to_string(), filter: "Inter_18pt-".to_string() },
        DownloadableFont { key: "roboto".to_string(), family: "Roboto".to_string(), license: "Apache License 2.0".to_string(), filter: "Roboto-".to_string() },
        DownloadableFont { key: "noto-sans".to_string(), family: "Noto Sans".to_string(), license: "SIL Open Font License".to_string(), filter: "NotoSans-".to_string() },
        DownloadableFont { key: "source-sans-3".to_string(), family: "Source Sans 3".to_string(), license: "SIL Open Font License".to_string(), filter: "SourceSans3-".to_string() },
        DownloadableFont { key: "ibm-plex-sans".to_string(), family: "IBM Plex Sans".to_string(), license: "SIL Open Font License".to_string(), filter: "IBMPlexSans-".to_string() },
        DownloadableFont { key: "ubuntu".to_string(), family: "Ubuntu".to_string(), license: "Ubuntu Font License".to_string(), filter: "Ubuntu-".to_string() },
        DownloadableFont { key: "open-sans".to_string(), family: "Open Sans".to_string(), license: "SIL Open Font License".to_string(), filter: "OpenSans-".to_string() },
        DownloadableFont { key: "lato".to_string(), family: "Lato".to_string(), license: "SIL Open Font License".to_string(), filter: "Lato-".to_string() },
        DownloadableFont { key: "montserrat".to_string(), family: "Montserrat".to_string(), license: "SIL Open Font License".to_string(), filter: "Montserrat-".to_string() },
        DownloadableFont { key: "poppins".to_string(), family: "Poppins".to_string(), license: "SIL Open Font License".to_string(), filter: "Poppins-".to_string() },
        DownloadableFont { key: "work-sans".to_string(), family: "Work Sans".to_string(), license: "SIL Open Font License".to_string(), filter: "WorkSans-".to_string() },
        DownloadableFont { key: "fira-sans".to_string(), family: "Fira Sans".to_string(), license: "SIL Open Font License".to_string(), filter: "FiraSans-".to_string() },
        DownloadableFont { key: "public-sans".to_string(), family: "Public Sans".to_string(), license: "SIL Open Font License".to_string(), filter: "PublicSans-".to_string() },
        DownloadableFont { key: "karla".to_string(), family: "Karla".to_string(), license: "SIL Open Font License".to_string(), filter: "Karla-".to_string() },
        DownloadableFont { key: "overpass".to_string(), family: "Overpass".to_string(), license: "SIL Open Font License".to_string(), filter: "Overpass-".to_string() },
        DownloadableFont { key: "barlow".to_string(), family: "Barlow".to_string(), license: "SIL Open Font License".to_string(), filter: "Barlow-".to_string() },
    ]
}

/// Tauri command: the curated list of families that can be downloaded on
/// demand.
#[tauri::command]
pub fn font_download_catalog() -> Vec<DownloadableFont> {
    download_catalog()
}

/// One catalog family plus the direct URL of its Regular static file, so the
/// frontend can register an `@font-face` and render a real glyph preview
/// without downloading anything.
#[derive(Serialize, Clone)]
pub struct FontPreviewSource {
    key: String,
    family: String,
    regular_url: String,
}

fn preview_cache() -> &'static std::sync::Mutex<Option<Vec<FontPreviewSource>>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<Vec<FontPreviewSource>>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(None))
}

/// Tauri command: for each catalog family, the URL of its Regular static file
/// (from the same manifest the download flow uses). Fetched once and cached;
/// failures return whatever succeeded so the previews degrade gracefully.
#[tauri::command]
pub async fn font_preview_sources() -> Vec<FontPreviewSource> {
    if let Some(cached) = preview_cache().lock().unwrap().as_ref() {
        return cached.clone();
    }

    let client = match reqwest::Client::builder()
        .user_agent("OLED-Helper-Fonts")
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(client) => client,
        Err(_) => return Vec::new(),
    };

    // Fetch every family manifest in parallel: the catalog is 16 families,
    // and sequential round-trips added a second or two to the first tab
    // open, which delayed the whole Windows Font tab.
    let responses = futures_util::future::join_all(download_catalog().iter().map(|entry| {
        client
            .get(format!(
                "https://fonts.google.com/download/list?family={}",
                entry.family.replace(' ', "%20")
            ))
            .send()
    }))
    .await;

    let mut sources = Vec::new();
    for (entry, response) in download_catalog().iter().zip(responses) {
        let Ok(response) = response else { continue };
        let Ok(text) = response.text().await else { continue };
        let text = text.strip_prefix(")]}'").unwrap_or(&text);
        let Ok(manifest) = serde_json::from_str::<GfManifest>(text) else {
            continue;
        };
        let files = select_static_files(&entry, &manifest.manifest.file_refs);
        // Prefer the Regular (weight 400, non-italic) file: the filename says
        // "Regular" and is not "Italic" (Roboto-Regular.ttf, Inter_18pt-
        // Regular.ttf). Fall back to the first static file of the set.
        let regular = files
            .iter()
            .find(|f| {
                let name = f.filename.to_lowercase();
                name.contains("regular") && !name.contains("italic")
            })
            .or_else(|| files.first());
        if let Some(file) = regular {
            sources.push(FontPreviewSource {
                key: entry.key.clone(),
                family: entry.family.clone(),
                regular_url: file.url.clone(),
            });
        }
    }

    *preview_cache().lock().unwrap() = Some(sources.clone());
    sources
}

/// Google Fonts `download/list` manifest. The response is JSON with a `)]}'`
/// anti-hijacking prefix; the fonts themselves are binary, referenced by URL
/// in `manifest.fileRefs` (no zip, no base64 — the `files` array only carries
/// the small text license/readme files).
#[derive(Deserialize)]
struct GfManifest {
    manifest: GfManifestInner,
}

#[derive(Deserialize)]
struct GfManifestInner {
    #[serde(rename = "fileRefs")]
    file_refs: Vec<GfFileRef>,
}

#[derive(Deserialize, Clone)]
struct GfFileRef {
    filename: String,
    url: String,
}

/// Pick the static `.ttf` files for one family out of the manifest: drop the
/// variable fonts and the width/optical variants that aren't the base set.
/// The per-family `filter` substring (e.g. "Roboto-") selects the base set and
/// naturally excludes `Roboto_Condensed-*` / `Roboto_SemiCondensed-*`.
fn select_static_files(entry: &DownloadableFont, refs: &[GfFileRef]) -> Vec<GfFileRef> {
    refs.iter()
        .filter(|r| {
            let name = r.filename.to_lowercase();
            name.ends_with(".ttf")
                && !name.contains("variable")
                && r.filename.contains(&entry.filter)
        })
        .cloned()
        .collect()
}

/// Download the static files of a catalog family into a private cache dir and
/// return the path of the primary (Regular weight 400) file, ready for
/// `apply_replacement`. The cache dir is under the runtime root, so it needs
/// no admin rights and is not enumerated as an installed font.
async fn download_family(entry: &DownloadableFont, app: Option<&tauri::AppHandle>) -> Result<PathBuf, String> {
    emit_progress(app, "download", &format!("Fetching {} from Google Fonts…", entry.family));
    let client = reqwest::Client::builder()
        .user_agent("OLED-Helper-Fonts")
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| format!("could not create HTTP client: {e}"))?;

    let url = format!(
        "https://fonts.google.com/download/list?family={}",
        entry.family.replace(' ', "%20")
    );
    let text = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("could not fetch {} manifest: {e}", entry.family))?
        .text()
        .await
        .map_err(|e| format!("could not read {} manifest: {e}", entry.family))?;
    let text = text.strip_prefix(")]}'").unwrap_or(&text);
    let manifest: GfManifest =
        serde_json::from_str(text).map_err(|e| format!("manifest parse failed: {e}"))?;

    let files = select_static_files(entry, &manifest.manifest.file_refs);
    if files.is_empty() {
        return Err(format!("no static files found for {}", entry.family));
    }

    let dir = runtime_root().join("downloads").join(&entry.key);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let mut primary: Option<PathBuf> = None;
    let mut downloaded = 0usize;
    let total = files.len();
    for file in &files {
        // The manifest names carry a static/ prefix; store flat by basename.
        let name = Path::new(&file.filename)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.filename.clone());
        let out = dir.join(&name);
        let bytes = client
            .get(&file.url)
            .send()
            .await
            .map_err(|e| format!("could not download {}: {e}", name))?
            .bytes()
            .await
            .map_err(|e| format!("could not read {}: {e}", name))?;
        std::fs::write(&out, &bytes).map_err(|e| format!("could not write {}: {e}", out.display()))?;
        downloaded += 1;
        emit_progress(
            app,
            "download",
            &format!("Downloading {name} ({downloaded}/{total})…"),
        );

        // Remember the Regular (weight 400, non-italic) file as the primary
        // source; siblings are matched by family in the same directory.
        if let Some(face) = ttf_parser::Face::parse(&bytes, 0).ok() {
            if face.weight().to_number() == 400 && !face.is_italic() && primary.is_none() {
                primary = Some(out);
            }
        }
    }

    let primary = primary.ok_or_else(|| format!("no Regular weight found for {}", entry.family))?;
    log::info!(
        "[font] downloaded {} static files for {} to {}",
        downloaded,
        entry.family,
        dir.display()
    );
    Ok(primary)
}

/// Tauri command: download a catalog family's static files and apply it as the
/// system font (same pipeline as an installed font).
#[tauri::command]
pub async fn font_download_apply(
    app: tauri::AppHandle,
    family_key: String,
) -> Result<ReplaceState, String> {
    let entry = download_catalog()
        .iter()
        .find(|e| e.key == family_key)
        .ok_or_else(|| format!("unknown font family key {family_key:?}"))?
        .clone();
    let primary = download_family(&entry, Some(&app)).await?;
    apply_replacement(&primary, Some(&app))
}

// ── Step 7: apply/restore orchestration + commands ──────────────────

fn ensure_elevated() -> Result<(), String> {
    if crate::process_is_elevated() {
        Ok(())
    } else {
        Err("font replacement requires administrator rights".to_string())
    }
}

/// Restore one canonical slot to its authentic Microsoft copy (from WinSxS,
/// falling back to the local backup), scheduled at the next boot. WinSxS is
/// the preferred source: it is authentic by definition, whereas a backup file
/// written by an older build could itself hold a replaced font. The staged
/// file carries the run's unique batch id (see next_batch_id).
fn restore_slot(slot: &SystemFontSlot, batch: &str) -> Result<(), String> {
    let root = system_fonts_root();
    let dest = root.join(slot.filename);
    let backup = backup_root().join(slot.filename);

    let source = find_winsxs_authentic(slot)
        .or_else(|| backup.is_file().then_some(backup.clone()))
        .ok_or_else(|| format!("no authentic copy of {} for restore", slot.filename))?;

    let staged = staging_root().join(format!("restore_{}.{batch}", slot.filename));
    std::fs::create_dir_all(staging_root()).map_err(|e| e.to_string())?;
    std::fs::copy(&source, &staged)
        .map_err(|e| format!("could not stage restore {}: {e}", slot.filename))?;
    take_ownership(&dest);
    schedule_replace(&staged, &dest)?;
    log::info!("[font] scheduled restore of {} at next boot", slot.filename);
    Ok(())
}

/// Full apply: preflight, build every impersonated artifact, back up the
/// current originals, schedule the boot-time replace, purge the cache, and
/// persist state. Returns the family name + count of files scheduled.
pub fn apply_replacement(primary: &Path, app: Option<&tauri::AppHandle>) -> Result<ReplaceState, String> {
    ensure_elevated()?;
    emit_progress(app, "build", "Building font files…");
    let primary_data = std::fs::read(primary)
        .map_err(|e| format!("could not read {}: {e}", primary.display()))?;
    let family = family_of(&primary_data)
        .ok_or_else(|| "the selected file is not a valid TrueType font".to_string())?;

    // Variable and monospace fonts are filtered from the picker; refuse them
    // here too so the command can never be reached with one (defense in depth).
    let face = ttf_parser::Face::parse(&primary_data, 0)
        .map_err(|_| "the selected file is not a valid TrueType font".to_string())?;
    if face.is_variable() {
        return Err(
            "variable fonts are not supported yet — install the static weight files of this font (e.g. Inter-Regular.ttf, Inter-Bold.ttf)"
                .to_string(),
        );
    }
    if face.is_monospaced() {
        return Err("monospace fonts would render the whole UI monospaced".to_string());
    }
    if is_symbol_font(&face) {
        return Err("symbol fonts (Wingdings, Segoe UI Symbol, …) have no letters and cannot be used as the UI font".to_string());
    }

    let build_dir = runtime_root().join("staged-build");
    std::fs::create_dir_all(&build_dir).map_err(|e| e.to_string())?;
    let artifacts = build_all_artifacts(primary, &build_dir)?;

    // This run supersedes any un-rebooted apply/restore from before: clear the
    // staging dir so old pending sources are gone (their boot entries are then
    // harmlessly skipped) and give this run a unique id so its own entries
    // can't be consumed by leftovers in the pending list.
    clear_staging();
    let batch = next_batch_id();

    // Back up the originals before touching anything. Always from the
    // authentic WinSxS copies (see backup_original). Monospace slots are
    // never replaced, so they are not backed up.
    let backup_slots: Vec<SystemFontSlot> = manifest_slots().into_iter().filter(|s| !s.mono).collect();
    let backup_total = backup_slots.len();
    for (i, slot) in backup_slots.iter().enumerate() {
        emit_progress(
            app,
            "backup",
            &format!("Backing up originals ({}/{backup_total})…", i + 1),
        );
        backup_original(&slot)?;
    }

    // Schedule every artifact for boot-time replacement.
    let mut applied = Vec::new();
    let apply_total = artifacts.len();
    for (i, artifact) in artifacts.iter().enumerate() {
        emit_progress(
            app,
            "apply",
            &format!("Scheduling boot replacement ({}/{apply_total})…", i + 1),
        );
        apply_artifact(artifact, &batch)?;
        applied.push(artifact.filename.clone());
    }

    emit_progress(app, "cache", "Clearing font cache…");
    purge_font_cache();
    broadcast_font_change();

    let state = ReplaceState {
        family,
        applied_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        files: applied,
    };
    save_state(&state)?;
    log::info!(
        "[font] replacement applied for family {:?} ({} files), reboot required",
        state.family,
        state.files.len()
    );
    Ok(state)
}

/// Full restore: schedule authentic copies back over every replaced slot and
/// clear the applied state.
pub fn restore_originals(app: Option<&tauri::AppHandle>) -> Result<(), String> {
    ensure_elevated()?;
    // This run supersedes any un-rebooted apply/restore from before (same
    // reasoning as apply_replacement: clear + unique batch id so the last
    // action always wins at boot).
    clear_staging();
    let batch = next_batch_id();
    // Restore every slot, including the monospace (Consolas) ones, so a
    // restore also undoes any Consolas replacement from an older build.
    let slots: Vec<SystemFontSlot> = manifest_slots();
    let total = slots.len();
    for (i, slot) in slots.iter().enumerate() {
        emit_progress(app, "restore", &format!("Restoring originals ({}/{total})…", i + 1));
        restore_slot(&slot, &batch)?;
    }
    emit_progress(app, "cache", "Clearing font cache…");
    purge_font_cache();
    broadcast_font_change();
    let _ = std::fs::remove_file(state_path());
    log::info!("[font] restore scheduled for all system font slots, reboot required");
    Ok(())
}

/// Live status for the UI: whether the app is elevated, the pending apply
/// state (if any), and whether a restart is actually needed to finish it.
#[derive(Serialize)]
pub struct FontStatus {
    pub elevated: bool,
    pub applied: Option<ReplaceState>,
    pub needs_restart: bool,
}

/// True when a boot-time font swap is still pending. Each apply/restore
/// stages its sources in OledHelperStaging and schedules them onto the live
/// fonts via Session Manager's pending-file-operations list; at boot the
/// staged files are moved away. Their presence therefore means the scheduled
/// swap has NOT run yet — the applied font isn't live, so a restart is
/// required. Unreadable staging (permissions) errs toward "needs restart".
fn restart_pending() -> bool {
    let Ok(entries) = std::fs::read_dir(staging_root()) else {
        return true;
    };
    entries.flatten().next().is_some()
}

#[tauri::command]
pub fn font_apply(app: tauri::AppHandle, primary_path: String) -> Result<ReplaceState, String> {
    apply_replacement(Path::new(&primary_path), Some(&app))
}

#[tauri::command]
pub fn font_restore(app: tauri::AppHandle) -> Result<(), String> {
    restore_originals(Some(&app))
}

#[tauri::command]
pub fn font_get_status() -> FontStatus {
    let applied = load_state();
    FontStatus {
        elevated: crate::process_is_elevated(),
        needs_restart: restart_pending(),
        applied,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system_font(name: &str) -> Option<Vec<u8>> {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
        std::fs::read(Path::new(&windir).join("Fonts").join(name)).ok()
    }

    #[test]
    fn impersonation_copies_donor_family_name() {
        let (Some(donor), Some(source)) = (system_font("segoeui.ttf"), system_font("arial.ttf"))
        else {
            eprintln!("skipping: Segoe UI / Arial system fonts not present");
            return;
        };
        let built = build_impersonated_font(&source, &donor).expect("build should succeed");
        let face = ttf_parser::Face::parse(&built, 0).expect("built font should reparse");
        let family = face
            .names()
            .into_iter()
            .find(|n| n.name_id == ttf_parser::name_id::FAMILY && n.is_unicode())
            .and_then(|n| n.to_string())
            .unwrap_or_default();
        assert_eq!(family, "Segoe UI");
    }

    #[test]
    fn symbol_font_detection_flags_symbol_fonts() {
        // wingding.ttf / marlett.ttf use a Windows symbol cmap (platform 3,
        // encoding 0, no Unicode subtable). They must be filtered from the
        // picker so a user can't apply an icon font as the UI font.
        for name in ["wingding.ttf", "marlett.ttf", "webdings.ttf", "symbol.ttf"] {
            let Some(data) = system_font(name) else {
                eprintln!("skipping: {name} not present");
                continue;
            };
            let face = ttf_parser::Face::parse(&data, 0).expect("parse should succeed");
            assert!(is_symbol_font(&face), "{name} should be detected as a symbol font");
        }
    }

    #[test]
    fn symbol_font_detection_passes_normal_fonts() {
        let Some(data) = system_font("arial.ttf") else {
            eprintln!("skipping: Arial not present");
            return;
        };
        let face = ttf_parser::Face::parse(&data, 0).expect("parse should succeed");
        assert!(!is_symbol_font(&face), "Arial should not be a symbol font");
    }

    #[test]
    fn family_of_prefers_typographic_family_name() {
        // segoeuil.ttf reports ID1 "Segoe UI Light" but its typographic family
        // (ID16) is "Segoe UI". Sibling matching must use ID16, or Light /
        // Semilight / Semibold / Black files never match their Regular sibling
        // (Google Fonts statics use the same ID1/ID16 split).
        let Some(light) = system_font("segoeuil.ttf") else {
            eprintln!("skipping: Segoe UI Light not present");
            return;
        };
        assert_eq!(family_of(&light).as_deref(), Some("Segoe UI"));

        // Regular has no ID16, so it falls back to ID1 "Segoe UI".
        let Some(regular) = system_font("segoeui.ttf") else {
            eprintln!("skipping: Segoe UI not present");
            return;
        };
        assert_eq!(family_of(&regular).as_deref(), Some("Segoe UI"));
    }

    #[test]
    fn impersonation_preserves_source_outline_tables() {
        let (Some(donor), Some(source)) = (system_font("segoeui.ttf"), system_font("arial.ttf"))
        else {
            eprintln!("skipping: Segoe UI / Arial system fonts not present");
            return;
        };
        let built = build_impersonated_font(&source, &donor).expect("build should succeed");
        // Rebuilding must keep the source's cmap/glyf so it still renders the
        // user's glyphs; only the name table changes.
        let face = ttf_parser::Face::parse(&built, 0).expect("built font should reparse");
        assert!(face.tables().cmap.is_some());
        assert!(face.tables().glyf.is_some());
    }
}
