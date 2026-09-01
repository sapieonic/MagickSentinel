import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { fileURLToPath } from 'node:url';

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      '@sentinel/shared': fileURLToPath(new URL('../shared/src/index.ts', import.meta.url)),
    },
  },
  build: {
    // The MSI ships this bundle to disk and WebView2 loads it over file:// or a
    // virtual host mapping, so asset URLs must be relative, not root-absolute.
    assetsDir: 'assets',
    sourcemap: true,
  },
  base: './',
});
