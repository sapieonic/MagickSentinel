import { defineConfig } from 'vitest/config';
import { fileURLToPath } from 'node:url';

// One vitest run for the whole workspace: the tests are pure logic (money, roles,
// error handling, state derivation) and need no DOM, so a single node environment
// keeps the suite fast and dependency-free.
export default defineConfig({
  resolve: {
    alias: {
      '@sentinel/shared': fileURLToPath(new URL('./shared/src/index.ts', import.meta.url)),
    },
  },
  test: {
    environment: 'node',
    include: ['shared/src/**/*.test.ts', 'widget/src/**/*.test.ts', 'portal/src/**/*.test.ts'],
  },
});
