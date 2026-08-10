# OLED Helper 2

A Windows desktop utility for OLED display calibration and system tweaks, built with Tauri (Rust) and Svelte 5. It ships as a single portable exe, needs no installer, and updates itself in place.

## Features

- **Black Crush Tweak**: Adjust near-black levels with a Reinhard gamma curve while preserving pure black. Includes an interactive HDR (PQ) and SDR (RGB) calibration strip, per-refresh-rate black-level profiles, and a high-nits (100 to 10,000 nits) grid.
- **Taskbar Dimmer**: Dim the Windows taskbar to match your OLED panel's brightness, with configurable opacity, fade times, hover detection, and fullscreen suppression.
- **Windows HDR**: Manage Windows Auto HDR and NVIDIA RTX HDR modes, toggle HDR, adjust SDR white level, and view HDR luminance details.
- **Hotkeys**: Global shortcuts for common actions, including a hold-to-compare view of your calibration.
- **Display tools**: Resolution and refresh-rate switching with safe confirmation and automatic rollback, plus a detailed system and display info tab.
- **Auto-update**: Checks GitHub for new versions silently at startup, shows an indicator in the status bar when one is available, and updates itself with one click.

## Requirements

- Windows 10 or 11
- WebView2 Runtime (already present on most systems that have Microsoft Edge)
- NVIDIA RTX GPU for the RTX HDR features

## Download and update

Grab the latest `oled-helper.exe` from the [Releases](https://github.com/MrHuntsman/Oled-Helper-v2/releases) page and run it from any writable folder. Nothing is installed and nothing touches the registry.

Updates are checked silently when the app starts or is restored from the tray. When a new version exists, the version number in the bottom-left status bar turns into a highlighted chip; clicking it opens the update window with the changelog and a one-click install that downloads, verifies, swaps the exe, and relaunches.

## Development

```bash
npm install
npx tauri dev
```

## Building the portable exe

```bash
npm run tauri build
```

This produces a single standalone `oled-helper.exe` in `src-tauri/target/release/`. No installer is generated.

## Releasing a new version

1. Bump the version in `src-tauri/tauri.conf.json` and `src-tauri/Cargo.toml` (keep them in sync).
2. Create a git tag matching that version, e.g. `v0.1.4`, and push it.
3. The `Release` GitHub Actions workflow builds the exe, generates `latest.json` (version, download url, sha256), and uploads both to the release. The in-app updater reads that manifest to offer and install updates.

## Project structure

- `src/`: Svelte 5 frontend (tabs, components, calibration math)
- `src-tauri/`: Rust backend (Tauri commands, gamma ramp logic, display handling, portable self-updater)
- `.github/workflows/release.yml`: builds and publishes portable releases
