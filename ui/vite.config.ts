import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  build: {
    // The window is drawn by the system web engine, not a bundled one: on
    // macOS that is the installed Safari's WebKit, which is Safari 13 on an
    // up-to-date Catalina (10.15, the oldest macOS Tauri supports). Vite's
    // default targets Safari 16.4, so its output (`??=` and the like) did not
    // even parse there and the window stayed black. Safari 13 also covers
    // WebKitGTK on Linux and WebView2 on Windows.
    target: "safari13",
  },
  server: {
    port: 1420,
    strictPort: true,
  },
});
