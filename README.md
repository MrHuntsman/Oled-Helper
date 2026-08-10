# OLED Helper 2

A Windows desktop utility for OLED display calibration and system tweaks, built with Tauri (Rust) and Svelte 5. It ships as a single portable exe, needs no installer, and updates itself in place.

## Features

- **Black Crush Tweak**: Fix near-black levels while preserving pure black, with HDR/SDR calibration strips and per-refresh-rate profiles.
- **Taskbar Dimmer**: Match the taskbar to your OLED brightness with configurable opacity, fades, and hover detection.
- **Windows HDR**: Control Windows Auto HDR, RTX HDR, HDR on/off, and SDR white level.
- **Hotkeys**: Global shortcuts, including a hold-to-compare calibration view.
- **Display tools**: Resolution and refresh-rate switching with auto rollback, plus detailed system info.
- **Auto-update**: Silent checks at startup, with one-click in-app updates.

## Requirements

- Windows 10 or 11
- WebView2 Runtime (already present on most systems that have Microsoft Edge)

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
