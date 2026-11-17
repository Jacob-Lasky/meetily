// Live assist: suggestion cards shown while a call records. The backend
// (src-tauri/src/live_assist) follows the transcript, calls the summary model
// and emits these events; this file holds the shared types and the pure list
// logic the panel uses.
//
// DO NOT rename the event names or Card fields here alone: they MUST equal
// ASSIST_*_EVENT and engine::Card in src-tauri/src/live_assist
// (`card_json_matches_the_frontend_contract` checks).

export const ASSIST_UPDATE_EVENT = 'assist-update';
export const ASSIST_WITHDRAW_EVENT = 'assist-withdraw';
export const ASSIST_STATUS_EVENT = 'assist-status';

export type AssistCardKind = 'answer' | 'ask' | 'fact' | 'todo';

export interface AssistCard {
  id: string;
  kind: AssistCardKind;
  text: string;
  /** The transcript words the card responds to. */
  quote: string;
  /** sequence_id of the transcript line the quote came from. */
  source_seq: number;
  pinned: boolean;
  created_at_ms: number;
}

export interface AssistUpdate {
  cards: AssistCard[];
}

export interface AssistWithdraw {
  ids: string[];
  reason: 'retracted' | 'evicted';
}

export type AssistState = 'idle' | 'thinking' | 'error' | 'off' | 'stopped';

export interface AssistStatus {
  state: AssistState;
  message?: string;
}

/** New cards go on top; a card already shown keeps its place. */
export function withAdded(cards: AssistCard[], added: AssistCard[]): AssistCard[] {
  const known = new Set(cards.map((c) => c.id));
  const fresh = added.filter((c) => !known.has(c.id));
  return fresh.length ? [...fresh, ...cards] : cards;
}

/** Cards minus `ids` (same array when none matched, so React skips a render). */
export function withoutIds(cards: AssistCard[], ids: readonly string[]): AssistCard[] {
  const drop = new Set(ids);
  return cards.some((c) => drop.has(c.id)) ? cards.filter((c) => !drop.has(c.id)) : cards;
}

/** Replace one card (after pin/unpin) in place. */
export function withCard(cards: AssistCard[], card: AssistCard): AssistCard[] {
  return cards.map((c) => (c.id === card.id ? card : c));
}

/**
 * Cards after a remount sync: the backend's list, plus live cards that arrived
 * while it was fetched, minus cards withdrawn meanwhile.
 */
export function mergeSynced(
  backend: AssistCard[],
  current: AssistCard[],
  withdrawn: ReadonlySet<string>,
): AssistCard[] {
  return withAdded(backend, current).filter((c) => !withdrawn.has(c.id));
}

export const KIND_LABEL: Record<AssistCardKind, string> = {
  answer: 'Answer',
  ask: 'Ask',
  fact: 'Fact',
  todo: 'To-do',
};
