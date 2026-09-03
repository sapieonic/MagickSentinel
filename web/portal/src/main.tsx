import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { BrowserRouter } from 'react-router-dom';
import '@sentinel/shared/styles.css';
import './portal.css';
import { AuthProvider } from './auth/AuthProvider.js';
import { readApiBaseUrl, readAuthConfig } from './auth/config.js';
import type { ConfigResult } from './auth/config.js';
import { createFirebaseIdentityBackend } from './auth/firebase.js';
import { PortalRoot } from './PortalRoot.js';

const container = document.getElementById('root');
if (!container) throw new Error('portal: #root missing from index.html');

/**
 * All configuration is read once, here, before anything renders.
 *
 * Both halves are validated together and their problems merged into one result, so a
 * deployment missing three variables sees three lines on one screen rather than
 * discovering them one redeploy at a time. Nothing below this point invents a default:
 * the Identity Platform tenant and — in a production build — the gateway origin have
 * no fallback, because guessing either produces a portal that looks like it works and
 * cannot authenticate.
 */
const env = import.meta.env as unknown as Record<string, string | boolean | undefined>;
const isDev = import.meta.env.DEV === true;

const authConfig = readAuthConfig(env);
const apiBaseUrl = readApiBaseUrl(env, isDev);

const configResult: ConfigResult = apiBaseUrl.ok
  ? authConfig
  : {
      ok: false,
      problems: [...(authConfig.ok ? [] : authConfig.problems), apiBaseUrl.problem],
    };

// Only reached when the configuration is good; the misconfigured screen never needs a
// gateway URL because it never makes a request.
const baseUrl = apiBaseUrl.ok ? apiBaseUrl.baseUrl : '';

createRoot(container).render(
  <StrictMode>
    <BrowserRouter>
      {/* The backend is passed as a constructor rather than an instance so the Firebase
          SDK is only touched once the configuration has been checked — and so a test
          or a future provider can substitute its own IdentityBackend without this file
          changing. */}
      <AuthProvider configResult={configResult} createBackend={createFirebaseIdentityBackend}>
        <PortalRoot baseUrl={baseUrl} />
      </AuthProvider>
    </BrowserRouter>
  </StrictMode>,
);
