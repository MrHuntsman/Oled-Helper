# OLED Helper 2

A Windows desktop utility for OLED display calibration and tweaks, built with Tauri (Rust) + Svelte 5.

## Features

- **Black Crush Tweak** — Adjust near-black levels using a Reinhard gamma curve while preserving pure black. Includes an interactive HDR (PQ) / SDR (RGB) calibration strip.

## Tech stack

- **Frontend**: Svelte 5 (runes), Vite
- **Backend**: Rust via Tauri v2 — applies gamma ramp adjustments directly through the Win32 GDI API (`SetDeviceGammaRamp`)

## Development

```bash
npm install
npx tauri dev
```

## Building

```bash
npx tauri build
```

## Project structure

- `src/` — Svelte frontend (tabs, components, calibration math)
- `src-tauri/` — Rust backend (Tauri commands, gamma ramp logic, window icon handling)
