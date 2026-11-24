import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen, UnlistenFn } from '@tauri-apps/api/event';
import { toast } from 'sonner';
import {
  ASSIST_BRIEF_EVENT,
  ASSIST_RESEARCH_EVENT,
  ASSIST_STATUS_EVENT,
  ASSIST_UPDATE_EVENT,
  ASSIST_WITHDRAW_EVENT,
  AssistBriefRead,
  AssistCard,
  AssistResearch,
  AssistStatus,
  AssistUpdate,
  AssistWithdraw,
  mergeSynced,
  withAdded,
  withCard,
  withoutIds,
} from '@/lib/live-assist';

export interface LiveAssist {
  /** Settings › AI Assist's live-assist switch (the tray flips it too). */
  enabled: boolean;
  /** The cards on screen, in the backend engine's order (pinned first, then newest). */
  cards: AssistCard[];
  status: AssistStatus;
  research: AssistResearch | null;
  briefRead: AssistBriefRead | null;
  setPinned: (card: AssistCard, pinned: boolean) => Promise<void>;
  dismiss: (card: AssistCard) => void;
}

/**
 * The live-assist cards and status of the current recording, from the
 * backend's events. One instance per recording page: both layouts (the side
 * panel and AI Assist mode, WORK-757) draw from it, so switching layout
 * mid-call keeps every card, pin and status line.
 */
export function useLiveAssist(isRecording: boolean): LiveAssist {
  const [enabled, setEnabled] = useState(false);
  const [cards, setCards] = useState<AssistCard[]>([]);
  const [status, setStatus] = useState<AssistStatus>({ state: 'idle' });
  const [research, setResearch] = useState<AssistResearch | null>(null);
  const [briefRead, setBriefRead] = useState<AssistBriefRead | null>(null);
  const withdrawnRef = useRef<Set<string>>(new Set());

  useEffect(() => {
    invoke<boolean>('live_assist_get_enabled').then(setEnabled).catch((e) => {
      console.error('live assist: could not read the setting', e);
    });
  }, []);

  useEffect(() => {
    const unlisteners: Promise<UnlistenFn>[] = [
      listen<AssistUpdate>(ASSIST_UPDATE_EVENT, (e) => setCards((prev) => withAdded(prev, e.payload.cards))),
      listen<AssistWithdraw>(ASSIST_WITHDRAW_EVENT, (e) => {
        e.payload.ids.forEach((id) => withdrawnRef.current.add(id));
        setCards((prev) => withoutIds(prev, e.payload.ids));
      }),
      listen<AssistResearch>(ASSIST_RESEARCH_EVENT, (e) => setResearch(e.payload)),
      listen<AssistBriefRead>(ASSIST_BRIEF_EVENT, (e) =>
        setBriefRead(e.payload.state === 'cancelled' ? null : e.payload),
      ),
      listen<AssistStatus>(ASSIST_STATUS_EVENT, (e) => {
        setStatus(e.payload);
        if (e.payload.state === 'off') setEnabled(false);
        if (e.payload.state === 'idle' || e.payload.state === 'thinking') setEnabled(true);
      }),
    ];
    return () => {
      unlisteners.forEach((p) => p.then((un) => un()));
    };
  }, []);

  // A new recording starts with no cards; a remount mid-recording
  // (navigating back from another page) picks up the session's cards.
  useEffect(() => {
    if (!isRecording) return;
    // A fetch still pending when this recording stops must not fill the next one.
    let current = true;
    withdrawnRef.current = new Set();
    setStatus({ state: 'idle' });
    setResearch(null);
    setBriefRead(null);
    invoke<AssistCard[]>('live_assist_get_cards')
      .then((backend) => {
        if (current) setCards((prev) => mergeSynced(backend, prev, withdrawnRef.current));
      })
      .catch((e) => console.error('live assist: could not load cards', e));
    return () => {
      current = false;
      setCards([]);
    };
  }, [isRecording]);

  // The "brief read" confirmation is a moment, not a state: it fades after a
  // while. "Reading" and errors stay until the next event replaces them.
  useEffect(() => {
    if (briefRead?.state !== 'done') return;
    const t = setTimeout(() => setBriefRead((b) => (b?.state === 'done' ? null : b)), 12_000);
    return () => clearTimeout(t);
  }, [briefRead]);

  const setPinned = useCallback(async (card: AssistCard, pinned: boolean) => {
    try {
      const updated = await invoke<AssistCard>('live_assist_set_pinned', { id: card.id, pinned });
      setCards((prev) => withCard(prev, updated));
    } catch (e) {
      toast.error(pinned ? 'Could not pin the card' : 'Could not unpin the card', { description: String(e) });
    }
  }, []);

  const dismiss = useCallback((card: AssistCard) => {
    // Also keeps a card-list sync still in flight from bringing it back.
    withdrawnRef.current.add(card.id);
    setCards((prev) => withoutIds(prev, [card.id]));
    invoke('live_assist_dismiss', { id: card.id }).catch((e) => console.error('live assist: dismiss failed', e));
  }, []);

  return { enabled, cards, status, research, briefRead, setPinned, dismiss };
}
