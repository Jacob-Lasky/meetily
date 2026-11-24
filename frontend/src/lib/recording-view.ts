// The recording view's layout (WORK-757): transcript first with the assist
// cards in a side panel, or AI Assist mode with the cards as the main content
// and the transcript shrunk to a strip. Layout only: recording, transcription,
// card generation and saving do not read this.

import { MIC_IN_LABEL, SPEAKER_OUT_LABEL } from './transcript-speakers';
import type { AssistCard } from './live-assist';
import type { InterimLine } from './transcript-interim';

export type RecordingView = 'transcript' | 'assist';

/**
 * AI Assist mode is the default: Jake asked for it so the cards, not the
 * scrolling transcript, hold his eye during a call (WORK-757). It applies only
 * while Live Assist is on; with it off there are no cards and the view is
 * always the transcript (`effectiveView`).
 */
export const DEFAULT_RECORDING_VIEW: RecordingView = 'assist';

/** localStorage key; a UI preference, like the side panel's collapsed state. */
export const RECORDING_VIEW_KEY = 'recordingView';

/** Window event that carries a change to every mounted view and setting. */
export const RECORDING_VIEW_EVENT = 'recording-view-change';

export function parseRecordingView(raw: string | null | undefined): RecordingView {
  return raw === 'transcript' || raw === 'assist' ? raw : DEFAULT_RECORDING_VIEW;
}

/** The layout to draw: AI Assist mode needs Live Assist on. */
export function effectiveView(view: RecordingView, assistEnabled: boolean): RecordingView {
  return assistEnabled ? view : 'transcript';
}

export type Side = 'them' | 'me' | 'other';

export interface StripLine {
  side: Side;
  /** The words to show: the live (interim) words if any, else the last final. */
  text: string;
  /** True when `text` is not final yet (shown greyed, as in the transcript). */
  interim: boolean;
}

/** Which side of the call a transcript label is ("Speaker-Out 2" is them). */
export function sideOf(speaker: string | null | undefined): Side {
  const channel = (speaker ?? '').replace(/ \d+$/, '');
  if (channel === SPEAKER_OUT_LABEL) return 'them';
  if (channel === MIC_IN_LABEL) return 'me';
  return 'other';
}

/**
 * The compact strip's lines: per side, what is being said now (interim) or
 * else the last thing said. Them first, since the cards answer them; an
 * unlabelled stream (mixed audio) gets one 'other' line.
 */
export function stripLines(
  finals: readonly { text: string; speaker?: string | null }[],
  interim: readonly InterimLine[],
): StripLine[] {
  const out: StripLine[] = [];
  for (const side of ['them', 'me', 'other'] as const) {
    const live = interim.filter((l) => sideOf(l.speaker) === side && l.text.trim()).pop();
    if (live) {
      out.push({ side, text: live.text.trim(), interim: true });
      continue;
    }
    for (let i = finals.length - 1; i >= 0; i--) {
      if (sideOf(finals[i].speaker) === side && finals[i].text.trim()) {
        out.push({ side, text: finals[i].text.trim(), interim: false });
        break;
      }
    }
  }
  return out;
}

/** The end of `text`, at most `max` characters, cut at a word: "…the last words". */
export function tail(text: string, max: number): string {
  const t = text.trim();
  if (t.length <= max) return t;
  const cut = t.slice(t.length - max);
  const space = cut.indexOf(' ');
  return `…${space > 0 && space < max / 3 ? cut.slice(space + 1) : cut}`;
}

/**
 * The card shown large: the newest unpinned card (pinned cards are the ones
 * Me already kept), or the newest pinned one when every card is pinned.
 */
export function featuredCard(cards: readonly AssistCard[]): AssistCard | null {
  const newest = (list: readonly AssistCard[]) =>
    list.reduce<AssistCard | null>((best, c) => (!best || c.created_at_ms > best.created_at_ms ? c : best), null);
  return newest(cards.filter((c) => !c.pinned)) ?? newest(cards);
}
