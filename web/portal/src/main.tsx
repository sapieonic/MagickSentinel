import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { BrowserRouter } from 'react-router-dom';
import '@sentinel/shared/styles.css';
import './portal.css';
import { App } from './App.js';
import { SessionProvider } from './session.js';

const container = document.getElementById('root');
if (!container) throw new Error('portal: #root missing from index.html');

// Gateway origin comes from the build environment; the local-development server in
// contracts/openapi.yaml is the fallback so a fresh checkout runs without config.
const baseUrl = import.meta.env['VITE_API_BASE_URL'] ?? 'http://localhost:8080';

/**
 * Token seam. The portal's Identity Platform session is not part of this workspace;
 * the SDK that owns it sets `window.__SENTINEL_PORTAL_TOKEN__` (or this function is
 * replaced outright when it lands). Returning null makes every request fail with a
 * clean `no_credentials` rather than a confusing 401.
 */
const getToken = (): string | null =>
  (globalThis as { __SENTINEL_PORTAL_TOKEN__?: string }).__SENTINEL_PORTAL_TOKEN__ ?? null;

createRoot(container).render(
  <StrictMode>
    <BrowserRouter>
      <SessionProvider baseUrl={baseUrl} getToken={getToken}>
        <App />
      </SessionProvider>
    </BrowserRouter>
  </StrictMode>,
);
