import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { fileURLToPath } from 'node:url';

export default defineConfig({
  plugins: [react()],
  resolve: {
    // Exact-match first: a bare prefix alias would rewrite
    // '@sentinel/shared/styles.css' into a path inside index.ts.
    alias: [
      { find: /^@sentinel\/shared$/, replacement: fileURLToPath(new URL('../shared/src/index.ts', import.meta.url)) },
      { find: /^@sentinel\/shared\//, replacement: fileURLToPath(new URL('../shared/src/', import.meta.url)) },
    ],
  },
  build: {
    // The MSI ships this bundle to disk and WebView2 loads it over file:// or a
    // virtual host mapping, so asset URLs must be relative, not root-absolute.
    assetsDir: 'assets',
    sourcemap: true,
  },
  base: './',
});
