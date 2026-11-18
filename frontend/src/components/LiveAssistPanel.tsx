'use client';

import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen, UnlistenFn } from '@tauri-apps/api/event';
import { toast } from 'sonner';
import { CheckCircle2, ChevronLeft, ChevronRight, ClipboardList, Loader2, Pin, PinOff, Search, Sparkles, TriangleAlert, X } from 'lucide-react';
import { MeetingBrief } from './MeetingBrief';
import {
  ASSIST_BRIEF_EVENT,
  ASSIST_RESEARCH_EVENT,
  ASSIST_STATUS_EVENT,
  ASSIST_UPDATE_EVENT,
  ASSIST_WITHDRAW_EVENT,
  AssistCard,
  AssistBriefRead,
  AssistCardKind,
  AssistResearch,
  BRIEF_ORIGIN,
  briefSize,
  AssistStatus,
  AssistUpdate,
  AssistWithdraw,
  KIND_LABEL,
  mergeSynced,
  withAdded,
  withCard,
  withoutIds,
} from '@/lib/live-assist';

const KIND_STYLE: Record<AssistCardKind, string> = {
  answer: 'bg-green-100 text-green-800',
  ask: 'bg-blue-100 text-blue-800',
  fact: 'bg-amber-100 text-amber-800',
  todo: 'bg-purple-100 text-purple-800',
};

const COLLAPSED_KEY = 'liveAssistCollapsed';

/**
 * Side panel on the recording view with the live-assist cards, newest on top,
 * under the meeting brief. Renders nothing unless the feature is on
 * (Settings › AI Assist); before a recording it holds only the brief, so it
 * can be filled in before the call starts.
 */
export function LiveAssistPanel({ isRecording }: { isRecording: boolean }) {
  const [enabled, setEnabled] = useState(false);
  const [cards, setCards] = useState<AssistCard[]>([]);
  const [status, setStatus] = useState<AssistStatus>({ state: 'idle' });
  const [collapsed, setCollapsed] = useState(false);
  const [research, setResearch] = useState<AssistResearch | null>(null);
  const [briefRead, setBriefRead] = useState<AssistBriefRead | null>(null);
  const withdrawnRef = useRef<Set<string>>(new Set());

  useEffect(() => {
    setCollapsed(localStorage.getItem(COLLAPSED_KEY) === 'true');
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
      listen<AssistBriefRead>(ASSIST_BRIEF_EVENT, (e) => setBriefRead(e.payload)),
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

  // A new recording starts with an empty panel; a remount mid-recording
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

  if (!enabled) return null;

  const toggleCollapsed = () => {
    setCollapsed((c) => {
      localStorage.setItem(COLLAPSED_KEY, String(!c));
      return !c;
    });
  };

  const setPinned = async (card: AssistCard, pinned: boolean) => {
    try {
      const updated = await invoke<AssistCard>('live_assist_set_pinned', { id: card.id, pinned });
      setCards((prev) => withCard(prev, updated));
    } catch (e) {
      toast.error(pinned ? 'Could not pin the card' : 'Could not unpin the card', { description: String(e) });
    }
  };

  const dismiss = (card: AssistCard) => {
    // Also keeps a card-list sync still in flight from bringing it back.
    withdrawnRef.current.add(card.id);
    setCards((prev) => withoutIds(prev, [card.id]));
    invoke('live_assist_dismiss', { id: card.id }).catch((e) => console.error('live assist: dismiss failed', e));
  };

  if (collapsed) {
    return (
      <aside className="w-10 flex-shrink-0 border-l border-gray-200 bg-white flex flex-col items-center pt-4 gap-3">
        <button onClick={toggleCollapsed} title="Show live assist" className="text-gray-500 hover:text-gray-900">
          <ChevronLeft className="w-4 h-4" />
        </button>
        <Sparkles className="w-4 h-4 text-blue-500" />
        {cards.length > 0 && (
          <span className="text-xs font-medium text-white bg-blue-500 rounded-full w-5 h-5 flex items-center justify-center">
            {cards.length}
          </span>
        )}
      </aside>
    );
  }

  return (
    <aside className="w-80 flex-shrink-0 border-l border-gray-200 bg-gray-50 flex flex-col overflow-hidden" aria-label="Live assist">
      <div className="flex items-center gap-2 px-3 py-3 border-b border-gray-200 bg-white">
        <Sparkles className="w-4 h-4 text-blue-500" />
        <h2 className="text-sm font-semibold text-gray-900 flex-1">Live assist</h2>
        {status.state === 'thinking' && <Loader2 className="w-4 h-4 text-gray-400 animate-spin" aria-label="Thinking" />}
        {status.state === 'error' && (
          <span title={status.message ?? 'Assist call failed'}>
            <TriangleAlert className="w-4 h-4 text-red-500" aria-label="Assist call failed" />
          </span>
        )}
        <button onClick={toggleCollapsed} title="Hide live assist" className="text-gray-500 hover:text-gray-900">
          <ChevronRight className="w-4 h-4" />
        </button>
      </div>
      <MeetingBrief defaultOpen={!isRecording} />
      {status.state === 'error' && status.message && (
        <p className="px-3 py-2 text-xs text-red-700 bg-red-50 border-b border-red-100">{status.message}</p>
      )}
      {isRecording && briefRead?.state === 'reading' && (
        <p className="px-3 py-2 text-xs text-blue-800 bg-blue-50 border-b border-blue-100 flex items-center gap-2" data-testid="assist-brief">
          <ClipboardList className="w-3.5 h-3.5 flex-shrink-0" />
          <span className="flex-1">Reading your brief ({briefSize(briefRead.chars)})…</span>
          <Loader2 className="w-3.5 h-3.5 animate-spin flex-shrink-0" />
        </p>
      )}
      {isRecording && briefRead?.state === 'done' && (
        <p className="px-3 py-2 text-xs text-green-800 bg-green-50 border-b border-green-100 flex items-center gap-2" data-testid="assist-brief">
          <CheckCircle2 className="w-3.5 h-3.5 flex-shrink-0" />
          <span>
            Brief read{briefRead.cards ? ` · ${briefRead.cards} opening card${briefRead.cards === 1 ? '' : 's'} below` : ''}. It goes
            with every card this call.
          </span>
        </p>
      )}
      {isRecording && briefRead?.state === 'error' && (
        <p className="px-3 py-2 text-xs text-amber-800 bg-amber-50 border-b border-amber-100" data-testid="assist-brief" title={briefRead.message}>
          Could not read your brief ({briefRead.message}). It still goes with every card.
        </p>
      )}
      {isRecording && research?.state === 'started' && (
        <p className="px-3 py-2 text-xs text-blue-800 bg-blue-50 border-b border-blue-100 flex items-center gap-2" data-testid="assist-research">
          <Search className="w-3.5 h-3.5 flex-shrink-0" />
          <span className="truncate" title={research.question}>Looking up: {research.question}</span>
          <Loader2 className="w-3.5 h-3.5 animate-spin flex-shrink-0" />
        </p>
      )}
      {isRecording && research?.state === 'error' && (
        <p className="px-3 py-2 text-xs text-amber-800 bg-amber-50 border-b border-amber-100" title={research.message}>
          Lookup failed: {research.message}
        </p>
      )}
      <div className="flex-1 overflow-y-auto p-3 space-y-2 pb-28">
        {!isRecording ? (
          <p className="text-xs text-gray-500 text-center mt-6">Cards appear here during a recording.</p>
        ) : cards.length === 0 ? (
          <p className="text-xs text-gray-500 text-center mt-6">
            {briefRead?.state === 'reading'
              ? 'Opening cards from your brief appear here in a moment.'
              : 'Suggestions appear here after the other side speaks.'}
          </p>
        ) : (
          cards.map((card) => (
            <div
              key={card.id}
              data-card-kind={card.kind}
              className={`rounded-lg border bg-white p-3 shadow-sm ${card.pinned ? 'border-blue-300' : 'border-gray-200'}`}
            >
              <div className="flex items-center gap-2 mb-1">
                <span className={`text-[10px] font-semibold uppercase tracking-wide px-1.5 py-0.5 rounded ${KIND_STYLE[card.kind]}`}>
                  {KIND_LABEL[card.kind]}
                </span>
                {card.origin === BRIEF_ORIGIN ? (
                  <span className="text-[10px] text-gray-500 flex items-center gap-1" title="Read from your meeting brief">
                    <ClipboardList className="w-3 h-3" />
                    From your brief
                  </span>
                ) : (
                  card.origin && (
                    <span className="text-[10px] text-gray-500" title="Looked up with MCP tools">
                      via {card.origin}
                    </span>
                  )
                )}
                <span className="flex-1" />
                <button
                  onClick={() => setPinned(card, !card.pinned)}
                  title={card.pinned ? 'Unpin' : 'Pin (kept with the meeting)'}
                  className={card.pinned ? 'text-blue-600' : 'text-gray-400 hover:text-gray-700'}
                >
                  {card.pinned ? <PinOff className="w-3.5 h-3.5" /> : <Pin className="w-3.5 h-3.5" />}
                </button>
                <button onClick={() => dismiss(card)} title="Dismiss" className="text-gray-400 hover:text-gray-700">
                  <X className="w-3.5 h-3.5" />
                </button>
              </div>
              <p className="text-sm text-gray-900 leading-snug">{card.text}</p>
              {card.quote && <p className="mt-1 text-xs text-gray-500 italic line-clamp-2">“{card.quote}”</p>}
            </div>
          ))
        )}
      </div>
    </aside>
  );
}
