import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// Build output goes into the Go binary via go:embed (cmd/rad/ui.go).
export default defineConfig({
  plugins: [react()],
  build: {
    outDir: '../cmd/rad/dist',
    emptyOutDir: true,
  },
  server: {
    proxy: {
      '/api': 'http://127.0.0.1:7237',
    },
  },
})
