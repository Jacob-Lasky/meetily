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
/** The research lane (Pipecat engine, WORK-581). */
export const ASSIST_RESEARCH_EVENT = 'assist-research';
/** Reading the meeting brief as the call opens, or after it changed (WORK-559). */
export const ASSIST_BRIEF_EVENT = 'assist-brief';
/** `AssistCard.origin` of the cards read from the meeting brief (engine::BRIEF_ORIGIN). */
export const BRIEF_ORIGIN = 'brief';
/** The meeting brief was saved with the meeting that stopped and cleared. */
export const ASSIST_BRIEF_CLEARED_EVENT = 'assist-brief-cleared';

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
  /**
   * Research cards: the MCP servers the lookup used (e.g. "DeepHive").
   * Cards read from the meeting brief: BRIEF_ORIGIN.
   */
  origin?: string;
}

export interface AssistUpdate {
  cards: AssistCard[];
}

export interface AssistWithdraw {
  ids: string[];
  reason: 'retracted' | 'evicted';
}

export type AssistState = 'idle' | 'thinking' | 'error' | 'off' | 'stopped';

export interface AssistResearch {
  state: 'started' | 'done' | 'error';
  question: string;
  message?: string;
  cards?: number;
}

export interface AssistBriefRead {
  /** 'cancelled': the read stopped before a result (brief changed, assist off). */
  state: 'reading' | 'done' | 'error' | 'cancelled';
  /** Length of the brief that was read. */
  chars: number;
  /** Cards the read added (state 'done'). */
  cards?: number;
  message?: string;
}

/** "7.7k characters" / "640 characters", for the brief status line. */
export function briefSize(chars: number): string {
  return chars >= 1000 ? `${(chars / 1000).toFixed(1).replace(/\.0$/, '')}k characters` : `${chars} characters`;
}

export interface AssistStatus {
  state: AssistState;
  message?: string;
}

/**
 * New cards go right under the pinned ones, above older unpinned cards; a
 * card already shown keeps its place. Same order as the backend engine.
 */
export function withAdded(cards: AssistCard[], added: AssistCard[]): AssistCard[] {
  const known = new Set(cards.map((c) => c.id));
  const fresh = added.filter((c) => !known.has(c.id));
  if (!fresh.length) return cards;
  const pinned = cards.filter((c) => c.pinned);
  return [...pinned, ...fresh, ...cards.filter((c) => !c.pinned)];
}

/** Cards minus `ids` (same array when none matched, so React skips a render). */
export function withoutIds(cards: AssistCard[], ids: readonly string[]): AssistCard[] {
  const drop = new Set(ids);
  return cards.some((c) => drop.has(c.id)) ? cards.filter((c) => !drop.has(c.id)) : cards;
}

/**
 * Replace one card after pin/unpin: a pinned card moves to the very top, an
 * unpinned one to the top of the unpinned cards (same as engine.rs).
 */
export function withCard(cards: AssistCard[], card: AssistCard): AssistCard[] {
  if (!cards.some((c) => c.id === card.id)) return cards;
  const rest = cards.filter((c) => c.id !== card.id);
  if (card.pinned) return [card, ...rest];
  const pins = rest.filter((c) => c.pinned);
  return [...pins, card, ...rest.filter((c) => !c.pinned)];
}

/**
 * Cards after a remount sync: the backend's list, plus live cards that arrived
 * while it was fetched, minus cards withdrawn meanwhile. For a card in both,
 * the current copy wins: it may carry a pin made after the snapshot was taken.
 */
export function mergeSynced(
  backend: AssistCard[],
  current: AssistCard[],
  withdrawn: ReadonlySet<string>,
): AssistCard[] {
  const currentById = new Map(current.map((c) => [c.id, c]));
  const synced = backend.map((c) => currentById.get(c.id) ?? c);
  const merged = withAdded(synced, current).filter((c) => !withdrawn.has(c.id));
  // Pins made after the snapshot (in `current`) are the newest: they go on
  // top, in the order the panel already shows them, as in the engine.
  const mergedById = new Map(merged.map((c) => [c.id, c]));
  const recent = current.filter((c) => c.pinned && mergedById.has(c.id)).map((c) => mergedById.get(c.id)!);
  const recentIds = new Set(recent.map((c) => c.id));
  const otherPins = merged.filter((c) => c.pinned && !recentIds.has(c.id));
  return [...recent, ...otherPins, ...merged.filter((c) => !c.pinned)];
}

export const KIND_LABEL: Record<AssistCardKind, string> = {
  answer: 'Answer',
  ask: 'Ask',
  fact: 'Fact',
  todo: 'To-do',
};
