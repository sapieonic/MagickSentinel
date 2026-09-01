import { describe, expect, it } from 'vitest';
import { formatPaise, formatPaiseApprox, paiseToInputValue, parseRupeesToPaise } from './money.js';

describe('parseRupeesToPaise', () => {
  it('parses whole rupees', () => {
    expect(parseRupeesToPaise('1500')).toEqual({ ok: true, paise: 150000 });
  });

  it('parses one and two decimal places', () => {
    expect(parseRupeesToPaise('1500.5')).toEqual({ ok: true, paise: 150050 });
    expect(parseRupeesToPaise('1500.05')).toEqual({ ok: true, paise: 150005 });
  });

  it('strips grouping, the rupee sign and whitespace', () => {
    expect(parseRupeesToPaise('  ₹ 1,23,456.78 ')).toEqual({ ok: true, paise: 12345678 });
  });

  it('is exact where float maths is not', () => {
    // 12.35 * 100 === 1234.9999999999998 in IEEE-754. A float path books ₹12.34.
    expect(parseRupeesToPaise('12.35')).toEqual({ ok: true, paise: 1235 });
    expect(parseRupeesToPaise('0.29')).toEqual({ ok: true, paise: 29 });
    expect(parseRupeesToPaise('8.87')).toEqual({ ok: true, paise: 887 });
    for (const rupees of ['1.01', '2.03', '4.07', '16.19', '32.33', '1234.56', '99999.99']) {
      const result = parseRupeesToPaise(rupees);
      expect(result.ok).toBe(true);
      if (result.ok) {
        const [whole, fraction = ''] = rupees.split('.');
        expect(result.paise).toBe(Number(whole) * 100 + Number(fraction.padEnd(2, '0')));
      }
    }
  });

  it('accepts a bare fraction', () => {
    expect(parseRupeesToPaise('.5')).toEqual({ ok: true, paise: 50 });
  });

  it('rejects more than two decimals instead of rounding', () => {
    expect(parseRupeesToPaise('1234.567')).toEqual({ ok: false, reason: 'too_many_decimals' });
  });

  it('rejects junk, blanks and negatives', () => {
    expect(parseRupeesToPaise('')).toEqual({ ok: false, reason: 'empty' });
    expect(parseRupeesToPaise('   ')).toEqual({ ok: false, reason: 'empty' });
    expect(parseRupeesToPaise('abc')).toEqual({ ok: false, reason: 'not_a_number' });
    expect(parseRupeesToPaise('1e3')).toEqual({ ok: false, reason: 'not_a_number' });
    expect(parseRupeesToPaise('1.2.3')).toEqual({ ok: false, reason: 'not_a_number' });
    expect(parseRupeesToPaise('-5')).toEqual({ ok: false, reason: 'negative' });
    expect(parseRupeesToPaise('.')).toEqual({ ok: false, reason: 'not_a_number' });
  });

  it('rejects amounts past safe integer paise', () => {
    expect(parseRupeesToPaise('999999999999999999')).toEqual({ ok: false, reason: 'out_of_range' });
  });
});

describe('formatPaise', () => {
  it('groups in the Indian system and always shows paise', () => {
    expect(formatPaise(12345678)).toBe('₹1,23,456.78');
    expect(formatPaise(100)).toBe('₹1.00');
    expect(formatPaise(5)).toBe('₹0.05');
    expect(formatPaise(0)).toBe('₹0.00');
  });

  it('can drop the symbol and compact whole rupees', () => {
    expect(formatPaise(150000, { symbol: false })).toBe('1,500.00');
    expect(formatPaise(150000, { compactWholeRupees: true })).toBe('₹1,500');
    expect(formatPaise(150050, { compactWholeRupees: true })).toBe('₹1,500.50');
  });

  it('renders negatives with the sign outside the symbol', () => {
    expect(formatPaise(-2550)).toBe('-₹25.50');
  });

  it('renders absent amounts as an em dash rather than ₹0.00', () => {
    expect(formatPaise(null)).toBe('—');
    expect(formatPaise(undefined)).toBe('—');
    expect(formatPaise(Number.NaN)).toBe('—');
  });

  it('round-trips through the input value helper', () => {
    for (const paise of [0, 5, 100, 1235, 12345678]) {
      const parsed = parseRupeesToPaise(paiseToInputValue(paise));
      expect(parsed).toEqual({ ok: true, paise });
    }
  });
});

describe('formatPaiseApprox', () => {
  it('rounds to whole rupees', () => {
    expect(formatPaiseApprox(150050)).toBe('₹1,501');
    expect(formatPaiseApprox(150049)).toBe('₹1,500');
    expect(formatPaiseApprox(null)).toBe('—');
  });
});
