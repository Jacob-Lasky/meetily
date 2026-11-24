'use client';

import { CheckCircle2, ClipboardList, LayoutPanelLeft, Loader2, Pin, PinOff, Search, Sparkles, X } from 'lucide-react';
import { AssistCard, AssistCardKind, BRIEF_ORIGIN, KIND_LABEL, briefSize } from '@/lib/live-assist';
import type { LiveAssist } from '@/hooks/useLiveAssist';
import type { RecordingView } from '@/lib/recording-view';

const KIND_STYLE: Record<AssistCardKind, string> = {
  answer: 'bg-green-100 text-green-800',
  ask: 'bg-blue-100 text-blue-800',
  fact: 'bg-amber-100 text-amber-800',
  todo: 'bg-purple-100 text-purple-800',
};

type CardSize = 'sm' | 'md' | 'lg';

const TEXT_SIZE: Record<CardSize, string> = {
  sm: 'text-sm text-gray-900 leading-snug',
  md: 'text-base text-gray-900 leading-snug',
  lg: 'text-2xl font-medium text-gray-900 leading-snug',
};

const QUOTE_SIZE: Record<CardSize, string> = {
  sm: 'mt-1 text-xs line-clamp-2',
  md: 'mt-2 text-sm line-clamp-2',
  lg: 'mt-3 text-base line-clamp-3',
};

/**
 * One live-assist card. Every layout draws cards with this, so pin, dismiss
 * and the quote of the line it answered behave the same everywhere.
 */
export function AssistCardView({
  card,
  size,
  onPin,
  onDismiss,
}: {
  card: AssistCard;
  size: CardSize;
  onPin: (card: AssistCard, pinned: boolean) => void;
  onDismiss: (card: AssistCard) => void;
}) {
  const icon = size === 'lg' ? 'w-5 h-5' : 'w-3.5 h-3.5';
  const badge = size === 'lg' ? 'text-xs px-2 py-0.5' : 'text-[10px] px-1.5 py-0.5';
  return (
    <div
      data-card-kind={card.kind}
      data-card-size={size}
      className={`rounded-lg border bg-white shadow-sm ${size === 'lg' ? 'p-5 border-2' : size === 'md' ? 'p-4' : 'p-3'} ${
        card.pinned ? 'border-blue-300' : 'border-gray-200'
      }`}
    >
      <div className={`flex items-center gap-2 ${size === 'lg' ? 'mb-3' : 'mb-1'}`}>
        <span className={`font-semibold uppercase tracking-wide rounded ${badge} ${KIND_STYLE[card.kind]}`}>
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
          onClick={() => onPin(card, !card.pinned)}
          title={card.pinned ? 'Unpin' : 'Pin (kept with the meeting)'}
          className={card.pinned ? 'text-blue-600' : 'text-gray-400 hover:text-gray-700'}
        >
          {card.pinned ? <PinOff className={icon} /> : <Pin className={icon} />}
        </button>
        <button onClick={() => onDismiss(card)} title="Dismiss" className="text-gray-400 hover:text-gray-700">
          <X className={icon} />
        </button>
      </div>
      <p className={TEXT_SIZE[size]}>{card.text}</p>
      {card.quote && <p className={`${QUOTE_SIZE[size]} text-gray-500 italic`}>“{card.quote}”</p>}
    </div>
  );
}

/** The brief-read, research and error lines under the assist header. */
export function AssistStatusLines({ assist, isRecording }: { assist: LiveAssist; isRecording: boolean }) {
  const { status, briefRead, research } = assist;
  return (
    <>
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
    </>
  );
}

/**
 * Switch between the transcript-first layout and AI Assist mode. Shown on the
 * recording page in both layouts, so it works mid-call; Settings › AI Assist
 * sets the same preference.
 */
export function RecordingViewToggle({
  view,
  onChange,
}: {
  view: RecordingView;
  onChange: (next: RecordingView) => void;
}) {
  const option = (value: RecordingView, label: string, Icon: typeof Sparkles, title: string) => (
    <button
      type="button"
      onClick={() => onChange(value)}
      title={title}
      aria-pressed={view === value}
      data-testid={`recording-view-${value}`}
      className={`flex items-center gap-1 px-2 py-1 text-xs rounded-md ${
        view === value ? 'bg-white text-gray-900 shadow-sm font-medium' : 'text-gray-500 hover:text-gray-900'
      }`}
    >
      <Icon className="w-3.5 h-3.5" />
      {label}
    </button>
  );
  return (
    <div className="flex items-center gap-0.5 rounded-lg bg-gray-100 p-0.5" role="group" aria-label="Recording view">
      {option('transcript', 'Transcript', LayoutPanelLeft, 'Transcript first, cards in a side panel')}
      {option('assist', 'AI Assist', Sparkles, 'AI Assist mode: cards first, transcript as a strip')}
    </div>
  );
}
