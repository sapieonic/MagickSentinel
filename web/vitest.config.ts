import { defineConfig } from 'vitest/config';
import { fileURLToPath } from 'node:url';

// One vitest run for the whole workspace: the tests are pure logic (money, roles,
// error handling, state derivation) and need no DOM, so a single node environment
// keeps the suite fast and dependency-free.
export default defineConfig({
  resolve: {
    // Exact-match first: a bare prefix alias would rewrite
    // '@sentinel/shared/styles.css' into a path inside index.ts.
    alias: [
      { find: /^@sentinel\/shared$/, replacement: fileURLToPath(new URL('./shared/src/index.ts', import.meta.url)) },
      { find: /^@sentinel\/shared\//, replacement: fileURLToPath(new URL('./shared/src/', import.meta.url)) },
    ],
  },
  test: {
    environment: 'node',
    include: ['shared/src/**/*.test.ts', 'widget/src/**/*.test.ts', 'portal/src/**/*.test.ts'],
  },
});
