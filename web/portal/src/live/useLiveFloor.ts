/**
 * React binding for `LiveConnection`. Owns nothing but the subscription: all the
 * behaviour worth arguing about lives in the connection, where it can be tested
 * without a browser.
 */
import { useEffect, useState } from 'react';
import { ApiError } from '@sentinel/shared';
import type { ApiClient } from '@sentinel/shared';
import { LiveConnection } from './connection.js';
import type { EventSourceLike, LiveState } from './connection.js';

const INITIAL: LiveState = { status: 'idle', calls: [], attempt: 0, refusal: null, lastEventAt: null };

export function useLiveFloor(api: ApiClient, teamId: string | null): LiveState {
  const [state, setState] = useState<LiveState>(INITIAL);

  useEffect(() => {
    if (!teamId) {
      setState(INITIAL);
      return;
    }
    if (typeof EventSource === 'undefined') {
      setState({ ...INITIAL, status: 'refused', refusal: 'This browser cannot open a server-sent event stream.' });
      return;
    }
    setState(INITIAL);
    const connection = new LiveConnection({
      teamId,
      mintTicket: (id, signal) => api.createLiveTicket(id, signal),
      streamUrl: (id, ticket) => api.teamLiveUrl(id, ticket),
      open: (url) => new EventSource(url) as unknown as EventSourceLike,
      onChange: setState,
      // 401 and 403 on the ticket mint mean the session or the role is wrong, not
      // that the network hiccuped, so retrying is pointless noise.
      isTerminal: (error) => error instanceof ApiError && (error.isAuthFailure || error.isForbidden),
    });
    connection.start();
    return () => connection.stop();
  }, [api, teamId]);

  return state;
}
