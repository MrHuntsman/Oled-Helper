import { defineConfig } from 'vite'
import { svelte } from '@sveltejs/vite-plugin-svelte'
import { resolve } from 'path'

// https://vite.dev/config/
export default defineConfig({
  plugins: [svelte()],
  build: {
    rollupOptions: {
      // Two HTML entry points: the main app window and the small always-on-top
      // "flyout" window used for the tray mode-confirmation popup. Both are
      // plain Tauri WebviewWindows pointed at their own HTML file/route so the
      // flyout is a real (tiny) webview rendering the same Svelte modal markup
      // and CSS as the in-app confirmation, instead of a hand-painted native
      // Win32 window that has to mirror app.css by hand.
      input: {
        main: resolve(__dirname, 'index.html'),
        flyout: resolve(__dirname, 'flyout.html'),
      },
    },
  },
})
