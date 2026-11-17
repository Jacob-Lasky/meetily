import { describe, expect, test } from 'bun:test';
import { type AssistCard, mergeSynced, withAdded, withCard, withoutIds } from '../../src/lib/live-assist';

const card = (id: string, extra: Partial<AssistCard> = {}): AssistCard => ({
  id,
  kind: 'ask',
  text: `text ${id}`,
  quote: 'q',
  source_seq: 1,
  pinned: false,
  created_at_ms: 0,
  ...extra,
});

describe('live assist card list', () => {
  test('new cards go on top in the order given', () => {
    const list = withAdded([card('old')], [card('a'), card('b')]);
    expect(list.map((c) => c.id)).toEqual(['a', 'b', 'old']);
  });

  test('a card already shown is not added twice', () => {
    const list = [card('a')];
    expect(withAdded(list, [card('a')])).toBe(list);
  });

  test('withdraw removes only the named cards', () => {
    const list = [card('a'), card('b'), card('c')];
    expect(withoutIds(list, ['b']).map((c) => c.id)).toEqual(['a', 'c']);
    expect(withoutIds(list, ['zzz'])).toBe(list);
  });

  test('pin updates the card in place', () => {
    const list = [card('a'), card('b')];
    expect(withCard(list, card('b', { pinned: true }))[1].pinned).toBe(true);
  });

  test('remount sync keeps live cards that arrived during the fetch and drops withdrawn ones', () => {
    const backend = [card('b'), card('a')];
    const current = [card('c')];
    expect(mergeSynced(backend, current, new Set(['a'])).map((c) => c.id)).toEqual(['c', 'b']);
  });

  test('remount sync keeps a pin made after the snapshot was taken', () => {
    const backend = [card('a')];
    const current = [card('a', { pinned: true })];
    expect(mergeSynced(backend, current, new Set()).map((c) => c.pinned)).toEqual([true]);
  });
});
