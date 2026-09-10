import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri serves the front end from this dev server in development and from `dist` in a
// release build.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  build: { target: "es2021", sourcemap: true },
});
