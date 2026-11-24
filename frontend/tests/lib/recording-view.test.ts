import { describe, expect, test } from 'bun:test';
import {
  DEFAULT_RECORDING_VIEW,
  effectiveView,
  featuredCard,
  parseRecordingView,
  sideOf,
  stripLines,
  tail,
} from '../../src/lib/recording-view';
import { type AssistCard } from '../../src/lib/live-assist';

const card = (id: string, created_at_ms: number, pinned = false): AssistCard => ({
  id,
  kind: 'ask',
  text: id,
  quote: '',
  source_seq: 1,
  pinned,
  created_at_ms,
});

describe('recording view preference', () => {
  test('AI Assist mode is the default, and junk falls back to it', () => {
    expect(DEFAULT_RECORDING_VIEW).toBe('assist');
    expect(parseRecordingView(null)).toBe('assist');
    expect(parseRecordingView('sideways')).toBe('assist');
    expect(parseRecordingView('transcript')).toBe('transcript');
    expect(parseRecordingView('assist')).toBe('assist');
  });

  test('AI Assist mode needs live assist on; off, the page is the transcript', () => {
    expect(effectiveView('assist', true)).toBe('assist');
    expect(effectiveView('assist', false)).toBe('transcript');
    expect(effectiveView('transcript', true)).toBe('transcript');
  });
});

describe('transcript strip', () => {
  test('channel labels map to sides, diarized numbers included', () => {
    expect(sideOf('Speaker-Out')).toBe('them');
    expect(sideOf('Speaker-Out 2')).toBe('them');
    expect(sideOf('Mic-In')).toBe('me');
    expect(sideOf(undefined)).toBe('other');
    expect(sideOf(null)).toBe('other');
  });

  test('each side shows its last final, them first', () => {
    const finals = [
      { text: 'Hello there.', speaker: 'Mic-In' },
      { text: 'What does Flux cost?', speaker: 'Speaker-Out' },
      { text: 'Good question.', speaker: 'Mic-In' },
      { text: 'And the latency?', speaker: 'Speaker-Out 2' },
    ];
    expect(stripLines(finals, [])).toEqual([
      { side: 'them', text: 'And the latency?', interim: false },
      { side: 'me', text: 'Good question.', interim: false },
    ]);
  });

  test('live words replace that side only, and still render greyed', () => {
    const finals = [
      { text: 'What does Flux cost?', speaker: 'Speaker-Out' },
      { text: 'Good question.', speaker: 'Mic-In' },
    ];
    expect(stripLines(finals, [{ speaker: 'Speaker-Out', text: 'and how fast is ' }])).toEqual([
      { side: 'them', text: 'and how fast is', interim: true },
      { side: 'me', text: 'Good question.', interim: false },
    ]);
  });

  test('blank lines are skipped and an unlabelled stream gets one line', () => {
    expect(stripLines([{ text: 'Mixed audio.' }, { text: '  ', speaker: 'Mic-In' }], [])).toEqual([
      { side: 'other', text: 'Mixed audio.', interim: false },
    ]);
    expect(stripLines([], [])).toEqual([]);
  });

  test('tail keeps the end of a long line, cut at a word', () => {
    expect(tail('short line', 40)).toBe('short line');
    const long = 'one two three four five six seven eight nine ten';
    const t = tail(long, 20);
    expect(t.startsWith('…')).toBe(true);
    expect(long.endsWith(t.slice(1))).toBe(true);
    expect(t.length <= 21).toBe(true);
    expect(t.slice(1).startsWith(' ')).toBe(false);
  });
});

describe('featured card', () => {
  test('the newest unpinned card is the large one', () => {
    expect(featuredCard([card('pin', 30, true), card('new', 20), card('old', 10)])?.id).toBe('new');
  });

  test('all pinned: the newest pinned card; none: nothing', () => {
    expect(featuredCard([card('a', 10, true), card('b', 20, true)])?.id).toBe('b');
    expect(featuredCard([])).toBe(null);
  });
});
