'use client';

import { useEffect, useState } from 'react';
import { ChevronLeft, ChevronRight, Loader2, Sparkles, TriangleAlert } from 'lucide-react';
import { MeetingBrief } from './MeetingBrief';
import { AssistCardView, AssistStatusLines, RecordingViewToggle } from './LiveAssistParts';
import type { LiveAssist } from '@/hooks/useLiveAssist';
import type { RecordingView } from '@/lib/recording-view';

const COLLAPSED_KEY = 'liveAssistCollapsed';

/**
 * The transcript-first layout's side panel: the live-assist cards, newest on
 * top, under the meeting brief. The page draws it only while Live Assist is
 * on (Settings › AI Assist); before a recording it holds only the brief, so
 * it can be filled in before the call starts.
 */
export function LiveAssistPanel({
  isRecording,
  assist,
  view,
  onViewChange,
}: {
  isRecording: boolean;
  assist: LiveAssist;
  view: RecordingView;
  onViewChange: (next: RecordingView) => void;
}) {
  const [collapsed, setCollapsed] = useState(false);
  const { cards, status } = assist;

  useEffect(() => {
    setCollapsed(localStorage.getItem(COLLAPSED_KEY) === 'true');
  }, []);

  const toggleCollapsed = () => {
    setCollapsed((c) => {
      localStorage.setItem(COLLAPSED_KEY, String(!c));
      return !c;
    });
  };

  if (collapsed) {
    return (
      <aside className="w-10 flex-shrink-0 border-l border-gray-200 bg-white flex flex-col items-center pt-4 gap-3">
        <button onClick={toggleCollapsed} title="Show live assist" className="text-gray-500 hover:text-gray-900">
          <ChevronLeft className="w-4 h-4" />
        </button>
        <button onClick={() => onViewChange('assist')} title="AI Assist mode: cards first" className="text-blue-500 hover:text-blue-700">
          <Sparkles className="w-4 h-4" />
        </button>
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
      <div className="flex items-center gap-2 px-3 py-2.5 border-b border-gray-200 bg-white">
        <RecordingViewToggle view={view} onChange={onViewChange} />
        <span className="flex-1" />
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
      <AssistStatusLines assist={assist} isRecording={isRecording} />
      <div className="flex-1 overflow-y-auto p-3 space-y-2 pb-28">
        {!isRecording ? (
          <p className="text-xs text-gray-500 text-center mt-6">Cards appear here during a recording.</p>
        ) : cards.length === 0 ? (
          <p className="text-xs text-gray-500 text-center mt-6">
            {assist.briefRead?.state === 'reading'
              ? 'Opening cards from your brief appear here in a moment.'
              : 'Suggestions appear here after the other side speaks.'}
          </p>
        ) : (
          cards.map((card) => (
            <AssistCardView key={card.id} card={card} size="sm" onPin={assist.setPinned} onDismiss={assist.dismiss} />
          ))
        )}
      </div>
    </aside>
  );
}
