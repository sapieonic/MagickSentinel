/**
 * The widget's one API client, used only for what the native layer does not hold:
 * the history tab and the post-call summary (spec 6.7).
 *
 * The credential comes from the native layer in both directions — read per request,
 * and re-read on demand when the gateway rejects it. Handing `ApiClient` the refresher
 * is what turns an hourly token rotation from "the history tab breaks halfway through
 * the shift" into one retried request. When the native layer has nothing newer to
 * offer, the refresher reports no credential and the widget falls back to the
 * signed-out view rather than retrying a token the gateway has already refused.
 */
import { ApiClient } from '@sentinel/shared';
import { resolveApiBaseUrl, resolveTokenProvider } from './host/bridge.js';
import type { SentinelHost } from './host/types.js';

export async function createWidgetApi(host: SentinelHost): Promise<ApiClient> {
  const tokens = resolveTokenProvider(host);
  return new ApiClient({
    baseUrl: await resolveApiBaseUrl(host),
    getToken: tokens.getToken,
    refreshToken: tokens.refreshToken,
    // Shorter than the default: a widget request that has not answered in eight
    // seconds is not going to help an agent who is about to take the next call.
    timeoutMs: 8000,
  });
}
