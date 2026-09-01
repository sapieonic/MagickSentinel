/**
 * Post-call card (spec 13.1).
 *
 * The hard requirement: on screen within 5 s of hangup. Everything that could be
 * slow is therefore optional. The disposition dropdown, the PTP fields and Confirm
 * are rendered from local state on the first paint; the summary, the model's PTP
 * extraction and the account reference are fetched afterwards and folded in if and
 * when they arrive. Nothing here awaits the analysis before rendering — if that
 * inverts, the card misses its budget on every slow call.
 */
import { useEffect, useRef, useState } from 'react';
import {
  ApiError,
  CaptureTierBadge,
  DISPOSITION_LABEL,
  DISPOSITIONS,
  formatPaise,
  paiseToInputValue,
  parseFailureMessage,
  parseRupeesToPaise,
} from '@sentinel/shared';
import type { ApiClient, CallConfirmation, CallDetail, CaptureTier, Disposition } from '@sentinel/shared';
import { summaryState } from '../state.js';

export interface PostCallCardProps {
  callId: string;
  endedAt: string;
  tier: CaptureTier;
  api: ApiClient | null;
  onConfirm: (id: string, payload: CallConfirmation) => Promise<void>;
  onOpenPortal: (path: string) => void;
}

export function PostCallCard({ callId, endedAt, tier, api, onConfirm, onOpenPortal }: PostCallCardProps) {
  const [detail, setDetail] = useState<CallDetail | null>(null);
  const [elapsedMs, setElapsedMs] = useState(0);

  const [disposition, setDisposition] = useState<Disposition | ''>('');
  const [ptpPresent, setPtpPresent] = useState(false);
  const [amountInput, setAmountInput] = useState('');
  const [dueDate, setDueDate] = useState('');
  const [amountError, setAmountError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);

  // Tracks whether the agent has touched a field, so a late-arriving extraction
  // never overwrites something they typed.
  const touched = useRef({ disposition: false, ptp: false });

  useEffect(() => {
    const started = Date.now();
    const id = setInterval(() => setElapsedMs(Date.now() - started), 1000);
    return () => clearInterval(id);
  }, [callId]);

  useEffect(() => {
    if (!api) return;
    const controller = new AbortController();
    let stopped = false;

    // Poll while analysis is still running. Backoff is deliberate: the pipeline
    // takes seconds, not milliseconds, and a tight loop from every widget on the
    // floor is a self-inflicted load spike.
    const delays = [400, 1000, 2000, 3000, 5000, 5000, 8000];
    let attempt = 0;

    const poll = async () => {
      try {
        const next = await api.getMyCall(callId, controller.signal);
        if (stopped) return;
        setDetail(next);
        applyExtraction(next);
        if (next.status === 'complete' || next.status === 'failed' || next.status === 'discarded') return;
      } catch (error) {
        // A failed summary fetch is never a failed card — the agent can still
        // confirm. Only stop retrying when retrying cannot help: a 404 is expected
        // for a few seconds while the call is still being ingested, but a rejected
        // token or a forbidden call will be rejected identically forever.
        if (stopped) return;
        if (error instanceof ApiError && (error.isAuthFailure || error.isForbidden)) return;
      }
      if (stopped || attempt >= delays.length) return;
      setTimeout(poll, delays[attempt++]!);
    };

    const applyExtraction = (next: CallDetail) => {
      if (!touched.current.disposition && next.disposition) setDisposition(next.disposition);
      if (!touched.current.ptp && next.ptp) {
        setPtpPresent(next.ptp.present === true);
        const extracted = next.ptp.agent_amount_paise ?? next.ptp.amount_paise;
        if (extracted !== null && extracted !== undefined) setAmountInput(paiseToInputValue(extracted));
        const extractedDate = next.ptp.agent_due_date ?? next.ptp.due_date;
        if (extractedDate) setDueDate(extractedDate);
      }
    };

    void poll();
    return () => {
      stopped = true;
      controller.abort();
    };
  }, [api, callId]);

  const summary = detail?.summary ?? null;
  const summaryStatus = summaryState(summary, elapsedMs);

  const submit = async () => {
    if (disposition === '') return;
    setSubmitError(null);

    let amountPaise: number | null = null;
    if (ptpPresent && amountInput.trim() !== '') {
      const parsed = parseRupeesToPaise(amountInput);
      if (!parsed.ok) {
        setAmountError(parseFailureMessage(parsed.reason));
        return;
      }
      amountPaise = parsed.paise;
    }
    setAmountError(null);

    const payload: CallConfirmation = {
      disposition,
      ptp_present: ptpPresent,
      ptp_amount_paise: amountPaise,
      ptp_due_date: ptpPresent && dueDate !== '' ? dueDate : null,
    };

    setSubmitting(true);
    try {
      // Confirmation goes through the native layer (spec 6.7) so it is spooled and
      // retried with the rest of the client's traffic if the link is down.
      await onConfirm(callId, payload);
    } catch {
      setSubmitError('Could not save. Try again, or correct it in the portal.');
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <div className="wg-panel wg-card">
      <div className="wg-row">
        <strong>Call ended</strong>
        <CaptureTierBadge tier={tier} />
      </div>

      <div className="wg-card__summary">
        {summaryStatus === 'ready' ? (
          <p>{summary}</p>
        ) : summaryStatus === 'pending' ? (
          <p className="sx-muted">Writing summary…</p>
        ) : (
          <p className="sx-muted">Summary not available yet — it will appear in your history.</p>
        )}
      </div>

      <label className="wg-field">
        <span>Disposition</span>
        <select
          value={disposition}
          onChange={(event) => {
            touched.current.disposition = true;
            const next = event.target.value as Disposition | '';
            setDisposition(next);
            // A PTP disposition without the PTP box ticked is the single most common
            // data-entry mistake on a collections floor; tick it for them.
            if (next === 'ptp' && !touched.current.ptp) setPtpPresent(true);
          }}
        >
          <option value="">Select…</option>
          {DISPOSITIONS.map((value) => (
            <option key={value} value={value}>
              {DISPOSITION_LABEL[value]}
            </option>
          ))}
        </select>
      </label>

      <label className="wg-check">
        <input
          type="checkbox"
          checked={ptpPresent}
          onChange={(event) => {
            touched.current.ptp = true;
            setPtpPresent(event.target.checked);
          }}
        />
        <span>Promise to pay</span>
      </label>

      {ptpPresent ? (
        <div className="wg-ptp-fields">
          <label className="wg-field">
            <span>Amount (₹)</span>
            <input
              inputMode="decimal"
              value={amountInput}
              placeholder="0.00"
              onChange={(event) => {
                touched.current.ptp = true;
                setAmountInput(event.target.value);
                setAmountError(null);
              }}
            />
          </label>
          <label className="wg-field">
            <span>Due date</span>
            <input
              type="date"
              value={dueDate}
              onChange={(event) => {
                touched.current.ptp = true;
                setDueDate(event.target.value);
              }}
            />
          </label>
        </div>
      ) : null}

      {amountError ? <p className="sx-error">{amountError}</p> : null}
      {submitError ? <p className="sx-error">{submitError}</p> : null}

      {detail?.ptp?.amount_paise !== null && detail?.ptp?.amount_paise !== undefined ? (
        <p className="sx-muted wg-extracted">Extracted: {formatPaise(detail.ptp.amount_paise)}</p>
      ) : null}

      <div className="wg-row wg-card__actions">
        <button className="sx-btn--primary" onClick={() => void submit()} disabled={disposition === '' || submitting}>
          {submitting ? 'Saving…' : 'Confirm'}
        </button>
        <button onClick={() => onOpenPortal(`/me/calls/${callId}`)}>Open in portal</button>
      </div>

      <p className="sx-muted wg-card__ended">Ended {new Date(endedAt).toLocaleTimeString()}</p>
    </div>
  );
}
