import { useEffect, useMemo, useRef, useState } from 'react';
import type { ApiClient, CallConfirmation } from '@sentinel/shared';
import { createWidgetApi } from './api.js';
import { deriveWidgetView } from './state.js';
import type { WidgetView } from './state.js';
import type { SentinelHost } from './host/types.js';
import { useHost } from './useHost.js';
import { Armed } from './views/Armed.js';
import { ErrorView } from './views/ErrorView.js';
import { HistoryTab } from './views/HistoryTab.js';
import { IdlePill } from './views/IdlePill.js';
import { InCall } from './views/InCall.js';
import { PostCallCard } from './views/PostCallCard.js';
import { SignedOut } from './views/SignedOut.js';
import { Wrap } from './views/Wrap.js';

type Tab = 'now' | 'history';

export function App() {
  const { state, host, native } = useHost();
  const [api, setApi] = useState<ApiClient | null>(null);
  const [tab, setTab] = useState<Tab>('now');

  useEffect(() => {
    if (!host) return;
    let cancelled = false;
    void createWidgetApi(host).then((client) => {
      if (!cancelled) setApi(client);
    });
    return () => {
      cancelled = true;
    };
  }, [host]);

  const view = useMemo(() => (state ? deriveWidgetView(state) : null), [state]);
  const callStartedAt = useCallStart(view);

  if (!view || !host) return <div className="wg-panel sx-muted">Starting Sentinel…</div>;

  // Signed out and error own the whole frame: there is nothing useful behind a tab
  // when you cannot record and cannot authenticate.
  const showTabs = view.kind !== 'signed_out' && view.kind !== 'error';
  const openPortal = (path: string) => void host.openPortal(path);

  return (
    <div className={`wg-root wg-root--${view.kind}`}>
      {!native ? <div className="wg-devbanner">Mock host — no agent process attached</div> : null}

      {showTabs ? (
        <nav className="wg-tabs">
          <button className={tabClass(tab === 'now')} onClick={() => setTab('now')}>
            Now
          </button>
          <button className={tabClass(tab === 'history')} onClick={() => setTab('history')}>
            History
          </button>
        </nav>
      ) : null}

      {showTabs && tab === 'history' ? (
        <HistoryTab api={api} onOpenPortal={openPortal} />
      ) : (
        <CurrentView view={view} host={host} api={api} callStartedAt={callStartedAt} onOpenPortal={openPortal} />
      )}
    </div>
  );
}

function tabClass(active: boolean): string {
  return active ? 'wg-tab wg-tab--on' : 'wg-tab';
}

function CurrentView({
  view,
  host,
  api,
  callStartedAt,
  onOpenPortal,
}: {
  view: WidgetView;
  host: SentinelHost;
  api: ApiClient | null;
  callStartedAt: number;
  onOpenPortal: (path: string) => void;
}) {
  switch (view.kind) {
    case 'signed_out':
      return <SignedOut signingIn={view.signingIn} onSignIn={() => void host.signIn()} />;
    case 'idle':
      return <IdlePill tier={view.tier} coverage={view.coverage} />;
    case 'armed':
      return <Armed tier={view.tier} />;
    case 'in_call':
      return <InCall tier={view.tier} startedAtEpochMs={callStartedAt} />;
    case 'wrap':
      return <Wrap tier={view.tier} />;
    case 'post_call':
      return (
        <PostCallCard
          callId={view.callId}
          endedAt={view.endedAt}
          tier={view.tier}
          api={api}
          onConfirm={(id: string, payload: CallConfirmation) => host.confirmCall(id, payload)}
          onOpenPortal={onOpenPortal}
        />
      );
    case 'error':
      return <ErrorView cause={view.cause} detail={view.detail} onSignOut={() => void host.signOut()} />;
  }
}

/**
 * Wall-clock start of the current call.
 *
 * Anchored to the first frame in which the call was seen rather than to component
 * mount, so switching to the History tab and back does not restart the elapsed timer
 * in front of the agent.
 */
function useCallStart(view: WidgetView | null): number {
  const ref = useRef<{ callId: string | null; at: number } | null>(null);
  if (view?.kind === 'in_call') {
    if (ref.current?.callId !== view.callId) ref.current = { callId: view.callId, at: Date.now() };
  } else if (view?.kind !== 'armed') {
    ref.current = null;
  }
  return ref.current?.at ?? Date.now();
}
