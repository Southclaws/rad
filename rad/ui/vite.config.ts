import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// Build output goes into the Go binary via go:embed (rad/server/ui.go).
export default defineConfig({
  plugins: [react()],
  build: {
    outDir: '../server/dist',
    emptyOutDir: true,
  },
  server: {
    proxy: {
      '/api': 'http://127.0.0.1:7237',
    },
  },
})
