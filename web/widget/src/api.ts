/**
 * The widget's one API client, used only for what the native layer does not hold:
 * the history tab and the post-call summary (spec 6.7).
 */
import { ApiClient } from '@sentinel/shared';
import { resolveApiBaseUrl, resolveTokenProvider } from './host/bridge.js';
import type { SentinelHost } from './host/types.js';

export async function createWidgetApi(host: SentinelHost): Promise<ApiClient> {
  return new ApiClient({
    baseUrl: await resolveApiBaseUrl(host),
    getToken: resolveTokenProvider(host),
    // Shorter than the default: a widget request that has not answered in eight
    // seconds is not going to help an agent who is about to take the next call.
    timeoutMs: 8000,
  });
}
