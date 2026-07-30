import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Keep asset names stable so the Rust binary can embed the production bundle
// with include_bytes! while the TypeScript source remains independently
// editable and testable.
export default defineConfig({
  plugins: [react()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    rollupOptions: {
      output: {
        entryFileNames: "assets/app.js",
        chunkFileNames: "assets/[name].js",
        assetFileNames: ({ names }) =>
          names.some((name) => name.endsWith(".css"))
            ? "assets/app.css"
            : "assets/[name][extname]",
      },
    },
  },
  server: {
    // The inspection API is served by the admin plane (default port + 1).
    proxy: {
      "/api": "http://127.0.0.1:7238",
    },
  },
});
