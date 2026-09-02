/**
 * Money helpers. Amounts cross the wire as an integer number of paise
 * (`*_paise`, int64) and are converted to rupees only for display.
 *
 * The rule these functions exist to enforce: never let a rupee value become a
 * JavaScript float. `12.35 * 100` is 1234.9999999999998, so any parse that routes
 * through `parseFloat` will eventually book a PTP a paisa short. Parsing here is
 * string-based and exact.
 */

/** Largest paise value that survives integer arithmetic in JS (~₹90,071,992,547.40). */
export const MAX_SAFE_PAISE = Number.MAX_SAFE_INTEGER;

export interface FormatPaiseOptions {
  /** Render the ₹ sign. Off for bare table cells that already have a currency header. */
  symbol?: boolean;
  /** Drop `.00` on whole-rupee amounts. Useful in dense lists. */
  compactWholeRupees?: boolean;
}

/**
 * Indian digit grouping (1,23,456.78). Built once — constructing an Intl formatter
 * per row is measurably slow in a 500-row call list.
 */
const RUPEE_GROUPS_WHOLE = new Intl.NumberFormat('en-IN', {
  minimumFractionDigits: 0,
  maximumFractionDigits: 0,
});

/**
 * Formats paise for display. Splits into whole rupees and remainder *before*
 * handing anything to Intl so the fractional part is never subject to binary
 * rounding.
 */
export function formatPaise(paise: number | null | undefined, options: FormatPaiseOptions = {}): string {
  if (paise === null || paise === undefined || !Number.isFinite(paise)) return '—';
  const { symbol = true, compactWholeRupees = false } = options;

  const negative = paise < 0;
  const abs = Math.abs(Math.trunc(paise));
  const rupees = Math.floor(abs / 100);
  const remainder = abs % 100;

  const body =
    compactWholeRupees && remainder === 0
      ? RUPEE_GROUPS_WHOLE.format(rupees)
      : `${RUPEE_GROUPS_WHOLE.format(rupees)}.${String(remainder).padStart(2, '0')}`;

  return `${negative ? '-' : ''}${symbol ? '₹' : ''}${body}`;
}

/** Formats to whole rupees for axis labels and totals where paise are noise. */
export function formatPaiseApprox(paise: number | null | undefined): string {
  if (paise === null || paise === undefined || !Number.isFinite(paise)) return '—';
  return `₹${RUPEE_GROUPS_WHOLE.format(Math.round(paise / 100))}`;
}

export type ParseResult = { ok: true; paise: number } | { ok: false; reason: ParseFailure };

export type ParseFailure = 'empty' | 'not_a_number' | 'too_many_decimals' | 'negative' | 'out_of_range';

/**
 * Parses a user-entered rupee string into paise, exactly.
 *
 * Accepts `1234`, `1234.5`, `1,234.50`, `₹1,234.50`, and surrounding whitespace.
 * Rejects more than two decimal places rather than rounding: an agent typing
 * `1234.567` has made a mistake, and silently truncating it books an amount they did
 * not enter into a legally meaningful promise-to-pay.
 */
export function parseRupeesToPaise(input: string): ParseResult {
  const cleaned = input.trim().replace(/^₹\s*/, '').replace(/,/g, '').replace(/\s/g, '');
  if (cleaned === '') return { ok: false, reason: 'empty' };
  if (cleaned.startsWith('-')) return { ok: false, reason: 'negative' };

  const match = /^(\d*)(?:\.(\d*))?$/.exec(cleaned);
  if (!match) return { ok: false, reason: 'not_a_number' };

  const whole = match[1] ?? '';
  const fraction = match[2];
  if (whole === '' && (fraction === undefined || fraction === '')) return { ok: false, reason: 'not_a_number' };
  if (fraction !== undefined && fraction.length > 2) return { ok: false, reason: 'too_many_decimals' };

  // String maths only: pad the fraction to exactly two digits and concatenate.
  const paiseDigits = (whole === '' ? '0' : whole) + (fraction ?? '').padEnd(2, '0');
  const paise = Number(paiseDigits);
  if (!Number.isSafeInteger(paise)) return { ok: false, reason: 'out_of_range' };
  return { ok: true, paise };
}

/** Message text for a rejected amount, suitable for an inline field error. */
export function parseFailureMessage(reason: ParseFailure): string {
  switch (reason) {
    case 'empty':
      return 'Enter an amount.';
    case 'not_a_number':
      return 'Amount must be a number, for example 1500 or 1500.50.';
    case 'too_many_decimals':
      return 'Amounts go to paise only — at most two decimal places.';
    case 'negative':
      return 'Amount cannot be negative.';
    case 'out_of_range':
      return 'Amount is too large.';
  }
}

/** Round-trips a stored paise value into the editable rupee string for an input. */
export function paiseToInputValue(paise: number | null | undefined): string {
  if (paise === null || paise === undefined || !Number.isFinite(paise)) return '';
  const abs = Math.abs(Math.trunc(paise));
  const sign = paise < 0 ? '-' : '';
  return `${sign}${Math.floor(abs / 100)}.${String(abs % 100).padStart(2, '0')}`;
}
