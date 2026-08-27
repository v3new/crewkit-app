import { readFileSync } from "node:fs";
import { defineConfig } from "vite";

// The app version shown in the footer comes from tauri.conf.json — the
// single source of truth for what the bundle reports.
const tauriConf = JSON.parse(readFileSync("./src-tauri/tauri.conf.json", "utf-8"));

// Tauri expects a fixed dev server port (see tauri.conf.json → build.devUrl).
export default defineConfig({
  clearScreen: false,
  define: {
    __APP_VERSION__: JSON.stringify(tauriConf.version),
  },
  server: {
    port: 1420,
    strictPort: true,
  },
});
