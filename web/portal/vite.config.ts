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
  build: { sourcemap: true },
});
