import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// The backend (workflow-graph-roundtrip) serves the built app from
// `frontend/dist/` at single origin, so the build target is `dist` and all
// asset URLs are relative. In dev, Vite talks to the backend cross-origin
// (CORS is enabled) at the address below.
const BACKEND = process.env.WORKFLOW_GRAPH_ORIGIN ?? 'http://127.0.0.1:3057';

export default defineConfig({
  plugins: [svelte()],
  base: './',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
  server: {
    port: 5179,
    // Proxy API calls to the backend so dev and prod use the same relative URLs.
    proxy: {
      '/workflow': { target: BACKEND, changeOrigin: true },
      '/operations': { target: BACKEND, changeOrigin: true },
      '/validate': { target: BACKEND, changeOrigin: true },
      '/project': { target: BACKEND, changeOrigin: true },
      '/run': { target: BACKEND, changeOrigin: true },
      '/approvals': { target: BACKEND, changeOrigin: true },
      '/healthz': { target: BACKEND, changeOrigin: true },
    },
  },
});
