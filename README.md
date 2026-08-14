# OLED Helper 2

A Windows desktop utility for OLED display calibration and system tweaks, built with Tauri (Rust) and Svelte 5. Portable version with auto-update.

## Features

- **Black Crush Tweak**: Tweak black levels while preserving pure black, with HDR/SDR calibration strips (Black and White) and per-refresh-rate profiles. Includes real HDR calibration strips.
- **Taskbar Dimmer**: Dim the taskbar to prevent OLED burn-in, with configurable opacity and hover animation.
- **Text rendering**: Mitigate text color fringing by setting the subpixel mode (Off / Grayscale / RGB / BGR) for both GDI and DirectWrite pipelines. Includes Windows Explorer fix and a live preview.
- **Windows HDR**: Controls for Windows SDR white level, Auto HDR, RTX HDR.
- **Hotkeys**: Global shortcuts, including a hold-to-compare calibration view.
- **Custom tray menu**: Resolution and refresh-rate switching, favorites, black level and taskbar dimmer quick controls, plus restart/exit right from the tray icon.

## Requirements

- Windows 10 or 11
- WebView2 Runtime (already present on most systems that have Microsoft Edge)

## Download and update

Grab the latest `OledHelper.exe` from the [Releases](https://github.com/MrHuntsman/Oled-Helper-v2/releases) page and run it from any writable folder. Nothing is installed, the exe is self-contained. Two notes:

- Font-smoothing tweaks are stored as per-user HKCU registry values, which is where Windows keeps ClearType settings.
- The Windows Explorer fix injects a hook into explorer.exe; some antivirus heuristics may flag it. It is harmless, and turning the fix off (or "Restore defaults") returns Explorer to its normal rendering.

Updates are checked silently when the app starts or is restored from the tray, and on demand from the About tab. When a new version exists, the version number in the bottom-left status bar turns into a highlighted chip; clicking it opens the update window with the changelog and a one-click install that downloads, verifies, swaps the exe, and relaunches.

## Development

```bash
npm install
npx tauri dev
```

## Building the portable exe

```bash
npm run tauri build
```

This produces a single standalone `OledHelper.exe` in `src-tauri/target/release/`. No installer is generated.

## Releasing a new version

1. Bump the version in `src-tauri/tauri.conf.json` and `src-tauri/Cargo.toml` (keep them in sync).
2. Create a git tag matching that version, e.g. `v0.1.4`, and push it.
3. The `Release` GitHub Actions workflow builds the exe, generates `latest.json` (version, download url, sha256), and uploads both to the release. The in-app updater reads that manifest to offer and install updates.

## Project structure

- `src/`: Svelte 5 frontend (tabs, components, calibration math)
- `src-tauri/`: Rust backend (Tauri commands, gamma ramp logic, display handling, portable self-updater)
- `.github/workflows/release.yml`: builds and publishes portable releases
