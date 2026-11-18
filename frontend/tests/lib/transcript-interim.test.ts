import { describe, expect, test } from 'bun:test';
import { applyInterim, type InterimLine } from '../../src/lib/transcript-interim';

describe('interim transcript lines', () => {
  test('a channel updates in place and keeps its position', () => {
    let lines: InterimLine[] = [];
    lines = applyInterim(lines, { speaker: 'Speaker-Out', text: 'so the' });
    lines = applyInterim(lines, { speaker: 'Mic-In', text: 'yeah' });
    lines = applyInterim(lines, { speaker: 'Speaker-Out', text: 'so the pricing' });
    expect(lines).toEqual([
      { speaker: 'Speaker-Out', text: 'so the pricing' },
      { speaker: 'Mic-In', text: 'yeah' },
    ]);
  });

  test('an empty update clears only that channel, and it returns last', () => {
    let lines: InterimLine[] = [
      { speaker: 'Speaker-Out', text: 'so the' },
      { speaker: 'Mic-In', text: 'yeah' },
    ];
    lines = applyInterim(lines, { speaker: 'Speaker-Out', text: '' });
    expect(lines).toEqual([{ speaker: 'Mic-In', text: 'yeah' }]);
    lines = applyInterim(lines, { speaker: 'Speaker-Out', text: 'and' });
    expect(lines.map((l) => l.speaker)).toEqual(['Mic-In', 'Speaker-Out']);
  });

  test('unchanged or no-op updates return the same array, so React skips a render', () => {
    const lines: InterimLine[] = [{ speaker: 'Mic-In', text: 'yeah' }];
    expect(applyInterim(lines, { speaker: 'Mic-In', text: ' yeah ' })).toBe(lines);
    expect(applyInterim(lines, { speaker: 'Speaker-Out', text: '   ' })).toBe(lines);
  });

  test('an unlabelled stream is its own channel', () => {
    const lines = applyInterim([], { speaker: null, text: 'hello' });
    expect(lines).toEqual([{ speaker: null, text: 'hello' }]);
    expect(applyInterim(lines, { speaker: null, text: '' })).toEqual([]);
  });
});
