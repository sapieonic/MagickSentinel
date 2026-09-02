/**
 * Recording indicator.
 *
 * There is deliberately no dismiss control and no `onClose` prop: spec 12 requires
 * the indicator to be visible for as long as capture is active, so the only way to
 * make it disappear is for capture to stop. Adding a close button here would be a
 * compliance regression, not a UX improvement.
 */
export function RecordingIndicator({ active, tierB = false }: { active: boolean; tierB?: boolean }) {
  if (!active) return null;
  return (
    <div className="sx-rec" role="status" aria-live="polite">
      <span className="sx-rec__dot" aria-hidden="true" />
      <span className="sx-rec__text">Recording{tierB ? ' (mixed audio)' : ''}</span>
    </div>
  );
}
