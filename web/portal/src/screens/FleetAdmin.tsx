/**
 * Screen 4 — Fleet / admin (spec 13.3): devices, tier distribution, coverage gaps,
 * enrollment tokens, users and roles, audit log.
 *
 * The rule editor is a stub (see Stubs.tsx). Everything here is admin-gated by the
 * router; the checks repeated inside are belt-and-braces, not the boundary.
 */
import { useState } from 'react';
import { CaptureTierBadge, ROLES, formatDateTime, formatPercent } from '@sentinel/shared';
import type { Device, Role, User } from '@sentinel/shared';
import { ApiError } from '@sentinel/shared';
import { EmptyState, ErrorState, LoadState, Panel } from '../components/Async.js';
import { useAsync } from '../components/useAsync.js';
import { useSession } from '../session.js';

/** Below this, a device is a coverage gap worth a supervisor conversation (spec 6.8). */
const COVERAGE_GAP_THRESHOLD = 0.9;

export function FleetAdmin() {
  return (
    <div className="pt-stack">
      <Devices />
      <Users />
      <AuditLog />
    </div>
  );
}

function Devices() {
  const { api } = useSession();
  const [busy, setBusy] = useState(false);
  const [token, setToken] = useState<{ token: string; expires_at: string } | null>(null);
  const devices = useAsync(async (signal) => api.listDevices({ limit: 200 }, signal), [api]);

  const items = devices.data?.items ?? [];
  const distribution = devices.data?.tier_distribution ?? {};
  const gaps = items.filter(
    (device) => device.coverage_pct_7d !== null && device.coverage_pct_7d !== undefined && device.coverage_pct_7d < COVERAGE_GAP_THRESHOLD,
  );

  const revoke = async (device: Device) => {
    // Revocation terminates live capture inside 60 s; no undo, so confirm first.
    if (!window.confirm(`Revoke ${device.machine_guid}? Capture stops within 60 seconds.`)) return;
    setBusy(true);
    try {
      await api.revokeDevice(device.id, 'revoked from portal');
      devices.reload();
    } finally {
      setBusy(false);
    }
  };

  const mintToken = async () => {
    setBusy(true);
    try {
      setToken(await api.createEnrollmentToken());
    } catch (error) {
      if (error instanceof ApiError) setToken(null);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Panel
      title="Fleet"
      actions={
        <button disabled={busy} onClick={() => void mintToken()}>
          Mint enrollment token
        </button>
      }
    >
      {token ? (
        <div className="pt-notice">
          {/* Shown once, on screen only. Never logged: it is a bearer credential for
              24 h and a console line would outlive the page. */}
          <p>
            Single-use enrollment token, valid until {formatDateTime(token.expires_at)}. Copy it now — it is not shown
            again.
          </p>
          <code className="pt-token">{token.token}</code>
        </div>
      ) : null}

      <div className="pt-stats">
        <Stat label="Devices" value={String(items.length)} />
        <Stat label="Tier A" value={String(distribution.A ?? 0)} />
        <Stat label="Tier B" value={String(distribution.B ?? 0)} />
        <Stat label="Coverage gaps" value={String(gaps.length)} tone={gaps.length > 0 ? 'warn' : undefined} />
      </div>

      {devices.loading ? (
        <LoadState />
      ) : devices.error ? (
        <ErrorState error={devices.error} onRetry={devices.reload} />
      ) : items.length === 0 ? (
        <EmptyState label="No devices enrolled." />
      ) : (
        <table className="pt-table">
          <thead>
            <tr>
              <th>Machine</th>
              <th>OS</th>
              <th>Tier</th>
              <th>Agent</th>
              <th>State</th>
              <th>Last seen</th>
              <th>Coverage 7d</th>
              <th>Status</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {items.map((device) => (
              <tr key={device.id} className={device.status === 'revoked' ? 'pt-row--muted' : undefined}>
                <td className="sx-mono">{device.machine_guid}</td>
                <td className="sx-mono">{device.os_build}</td>
                <td>
                  <CaptureTierBadge tier={device.capture_tier} />
                </td>
                <td className="sx-mono">{device.agent_version}</td>
                <td>{device.last_capture_state ?? '—'}</td>
                <td>{formatDateTime(device.last_seen_at)}</td>
                <td
                  className={
                    device.coverage_pct_7d !== null &&
                    device.coverage_pct_7d !== undefined &&
                    device.coverage_pct_7d < COVERAGE_GAP_THRESHOLD
                      ? 'sx-nums pt-cell--warn'
                      : 'sx-nums'
                  }
                >
                  {formatPercent(device.coverage_pct_7d, 1)}
                </td>
                <td>{device.status}</td>
                <td>
                  {device.status === 'active' ? (
                    <button disabled={busy} onClick={() => void revoke(device)}>
                      Revoke
                    </button>
                  ) : null}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Panel>
  );
}

function Users() {
  const { api } = useSession();
  const users = useAsync(async (signal) => api.listUsers(signal), [api]);
  const [busy, setBusy] = useState<string | null>(null);

  const change = async (user: User, patch: { role?: Role; status?: 'active' | 'suspended' }) => {
    setBusy(user.firebase_uid);
    try {
      await api.updateUser(user.firebase_uid, patch);
      users.reload();
    } finally {
      setBusy(null);
    }
  };

  return (
    <Panel title="Users and roles">
      {users.loading ? (
        <LoadState />
      ) : users.error ? (
        <ErrorState error={users.error} onRetry={users.reload} />
      ) : (users.data?.length ?? 0) === 0 ? (
        <EmptyState label="No users." />
      ) : (
        <table className="pt-table">
          <thead>
            <tr>
              <th>Name</th>
              <th>UID</th>
              <th>Role</th>
              <th>Team</th>
              <th>Status</th>
            </tr>
          </thead>
          <tbody>
            {(users.data ?? []).map((user) => (
              <tr key={user.firebase_uid}>
                <td>{user.display_name}</td>
                <td className="sx-mono">{user.firebase_uid}</td>
                <td>
                  <select
                    value={user.role}
                    disabled={busy === user.firebase_uid}
                    onChange={(event) => void change(user, { role: event.target.value as Role })}
                  >
                    {ROLES.map((role) => (
                      <option key={role} value={role}>
                        {role}
                      </option>
                    ))}
                  </select>
                </td>
                <td className="sx-mono">{user.team_id ?? '—'}</td>
                <td>
                  <button
                    disabled={busy === user.firebase_uid}
                    onClick={() => void change(user, { status: user.status === 'active' ? 'suspended' : 'active' })}
                  >
                    {user.status === 'active' ? 'Suspend' : 'Reactivate'}
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Panel>
  );
}

function AuditLog() {
  const { api } = useSession();
  const [actor, setActor] = useState('');
  const [entity, setEntity] = useState('');
  const audit = useAsync(
    async (signal) => api.getAuditLog({ actor_uid: actor || undefined, entity: entity || undefined, limit: 100 }, signal),
    [api, actor, entity],
  );

  return (
    <Panel title="Audit log">
      <div className="pt-filters">
        <label>
          <span>Actor UID</span>
          <input value={actor} onChange={(event) => setActor(event.target.value)} />
        </label>
        <label>
          <span>Entity</span>
          <input value={entity} onChange={(event) => setEntity(event.target.value)} />
        </label>
      </div>
      {audit.loading ? (
        <LoadState />
      ) : audit.error ? (
        <ErrorState error={audit.error} onRetry={audit.reload} />
      ) : (audit.data?.items?.length ?? 0) === 0 ? (
        <EmptyState label="No audit entries." />
      ) : (
        <table className="pt-table">
          <thead>
            <tr>
              <th>When</th>
              <th>Actor</th>
              <th>Action</th>
              <th>Entity</th>
              <th>Detail</th>
            </tr>
          </thead>
          <tbody>
            {(audit.data?.items ?? []).map((entry) => (
              <tr key={entry.id}>
                <td>{formatDateTime(entry.at)}</td>
                <td className="sx-mono">{entry.actor_uid ?? 'system'}</td>
                <td>{entry.action}</td>
                <td className="sx-mono">
                  {entry.entity}
                  {entry.entity_id ? `/${entry.entity_id}` : ''}
                </td>
                {/* The contract forbids transcript text, borrower names or account
                    refs in `detail`; it is rendered as-is rather than parsed. */}
                <td className="pt-evidence sx-mono">{entry.detail ? JSON.stringify(entry.detail) : '—'}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </Panel>
  );
}

function Stat({ label, value, tone }: { label: string; value: string; tone?: 'warn' | undefined }) {
  return (
    <div className={tone === 'warn' ? 'pt-stat pt-stat--warn' : 'pt-stat'}>
      <span className="pt-stat__value sx-nums">{value}</span>
      <span className="pt-stat__label sx-muted">{label}</span>
    </div>
  );
}
