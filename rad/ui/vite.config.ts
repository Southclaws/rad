import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Build output goes into the Go binary via go:embed (rad/server/ui/ui.go).
export default defineConfig({
  plugins: [react()],
  build: {
    outDir: "../server/ui/dist",
    emptyOutDir: true,
  },
  server: {
    // The inspection API is served by the admin plane (default port + 1).
    proxy: {
      "/api": "http://127.0.0.1:7238",
    },
  },
});
