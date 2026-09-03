import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { viteSingleFile } from 'vite-plugin-singlefile';
import { fileURLToPath } from 'node:url';

export default defineConfig({
  // The installer packages exactly one file, `widget.html` (client/installer/Sentinel.wxs),
  // so the bundle has to be genuinely self-contained. Vite's default output is
  // `index.html` plus hashed `assets/*.js` and `*.css`, which the MSI does not carry:
  // the install would succeed, the service would register and report healthy, and the
  // widget would render blank -- including the non-dismissible recording indicator that
  // is a compliance requirement rather than a nicety. Inlining is what makes the WiX
  // authoring and the staging script honest.
  plugins: [react(), viteSingleFile()],
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
    // Inlined into the single HTML file rather than emitted beside it, since nothing
    // beside it is packaged. A separate .map would be one more unshipped file.
    sourcemap: false,
    // Belt and braces with viteSingleFile: no asset may be large enough to be spilled
    // to its own file, and CSS must not be split across chunks.
    assetsInlineLimit: Number.MAX_SAFE_INTEGER,
    cssCodeSplit: false,
  },
  base: './',
});
