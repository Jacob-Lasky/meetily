'use client';

import { useEffect, useMemo, useState } from 'react';
import { Loader2, PanelRightClose, PanelRightOpen, Sparkles, TriangleAlert } from 'lucide-react';
import { MeetingBrief } from './MeetingBrief';
import { AssistCardView, AssistStatusLines, RecordingViewToggle } from './LiveAssistParts';
import { TranscriptPanel } from '@/app/_components/TranscriptPanel';
import { useTranscripts } from '@/contexts/TranscriptContext';
import { useRecordingState } from '@/contexts/RecordingStateContext';
import { useInterimTranscripts } from '@/hooks/useInterimTranscripts';
import type { LiveAssist } from '@/hooks/useLiveAssist';
import type { ModalType } from '@/hooks/useModalState';
import { RecordingView, Side, featuredCard, stripLines, tail } from '@/lib/recording-view';
import { MIC_IN_LABEL, SPEAKER_OUT_LABEL, speakerLabelClass } from '@/lib/transcript-speakers';

const TRANSCRIPT_OPEN_KEY = 'assistModeTranscriptOpen';

const SIDE_LABEL: Record<Side, string> = { them: 'Them', me: 'Me', other: 'Audio' };
// The transcript's own channel colours.
const SIDE_STYLE: Record<Side, string> = {
  them: speakerLabelClass(SPEAKER_OUT_LABEL),
  me: speakerLabelClass(MIC_IN_LABEL),
  other: speakerLabelClass(''),
};

/**
 * The transcript shrunk to a strip: the latest words from each side, live
 * (greyed) words included, so the cards keep their context.
 */
function TranscriptStrip({ onExpand }: { onExpand: () => void }) {
  const { transcripts } = useTranscripts();
  const { isRecording, isPaused } = useRecordingState();
  const { lines: interim } = useInterimTranscripts(isRecording, isPaused);
  const lines = useMemo(() => stripLines(transcripts, interim), [transcripts, interim]);

  return (
    <div className="border-b border-gray-200 bg-white px-4 py-2 flex gap-3 items-start" data-testid="transcript-strip">
      <div className="flex-1 min-w-0 space-y-1">
        {lines.length === 0 ? (
          <p className="text-sm text-gray-400">{isRecording ? 'Listening…' : 'The transcript shows here during a recording.'}</p>
        ) : (
          lines.map((line) => (
            <p key={line.side} className="text-sm leading-snug line-clamp-2" data-strip-side={line.side}>
              <span className={`font-semibold mr-2 ${SIDE_STYLE[line.side]}`}>{SIDE_LABEL[line.side]}</span>
              <span className={line.interim ? 'text-gray-400' : 'text-gray-700'}>{tail(line.text, 240)}</span>
            </p>
          ))
        )}
      </div>
      <button
        onClick={onExpand}
        title="Show the full transcript beside the cards"
        className="flex-shrink-0 flex items-center gap-1 text-xs text-gray-500 hover:text-gray-900 mt-0.5"
      >
        <PanelRightOpen className="w-4 h-4" />
        Transcript
      </button>
    </div>
  );
}

/**
 * AI Assist mode (WORK-757): the recording view built around the cards. The
 * newest card is large, the rest sit in a grid below it, and the transcript
 * is a strip at the top that opens into a side column. Draws from the same
 * live-assist state as the side panel, so switching layout mid-call keeps
 * every card.
 */
export function AssistModeView({
  isRecording,
  assist,
  view,
  onViewChange,
  isProcessingStop,
  isStopping,
  showModal,
}: {
  isRecording: boolean;
  assist: LiveAssist;
  view: RecordingView;
  onViewChange: (next: RecordingView) => void;
  isProcessingStop: boolean;
  isStopping: boolean;
  showModal: (name: ModalType, message?: string) => void;
}) {
  const [transcriptOpen, setTranscriptOpen] = useState(false);
  const { cards, status } = assist;
  const featured = featuredCard(cards);
  const rest = featured ? cards.filter((c) => c.id !== featured.id) : cards;

  useEffect(() => {
    setTranscriptOpen(localStorage.getItem(TRANSCRIPT_OPEN_KEY) === 'true');
  }, []);

  const showTranscript = (open: boolean) => {
    localStorage.setItem(TRANSCRIPT_OPEN_KEY, String(open));
    setTranscriptOpen(open);
  };

  return (
    <div className="flex flex-1 min-w-0 overflow-hidden" data-testid="assist-mode">
      <section className="flex-1 min-w-0 flex flex-col overflow-hidden bg-gray-50" aria-label="AI Assist">
        <div className="flex items-center gap-3 px-4 py-2.5 border-b border-gray-200 bg-white">
          <Sparkles className="w-5 h-5 text-blue-500" />
          <h2 className="text-base font-semibold text-gray-900">AI Assist</h2>
          {status.state === 'thinking' && <Loader2 className="w-4 h-4 text-gray-400 animate-spin" aria-label="Thinking" />}
          {status.state === 'error' && (
            <span title={status.message ?? 'Assist call failed'}>
              <TriangleAlert className="w-4 h-4 text-red-500" aria-label="Assist call failed" />
            </span>
          )}
          <span className="flex-1" />
          {transcriptOpen && (
            <button
              onClick={() => showTranscript(false)}
              title="Back to the transcript strip"
              className="flex items-center gap-1 text-xs text-gray-500 hover:text-gray-900"
            >
              <PanelRightClose className="w-4 h-4" />
              Hide transcript
            </button>
          )}
          <RecordingViewToggle view={view} onChange={onViewChange} />
        </div>
        {!transcriptOpen && <TranscriptStrip onExpand={() => showTranscript(true)} />}
        <AssistStatusLines assist={assist} isRecording={isRecording} />
        <MeetingBrief defaultOpen={!isRecording} autoOpen={!isRecording} />
        <div className="flex-1 overflow-y-auto px-6 pt-5 pb-36" data-testid="assist-mode-cards">
          {!isRecording ? (
            <p className="text-sm text-gray-500 text-center mt-10">Cards appear here, large, during a recording.</p>
          ) : !featured ? (
            <p className="text-sm text-gray-500 text-center mt-10">
              {assist.briefRead?.state === 'reading'
                ? 'Opening cards from your brief appear here in a moment.'
                : 'Suggestions appear here after the other side speaks.'}
            </p>
          ) : (
            <div className="max-w-5xl mx-auto space-y-4">
              <AssistCardView key={featured.id} card={featured} size="lg" onPin={assist.setPinned} onDismiss={assist.dismiss} />
              {rest.length > 0 && (
                <div className="grid grid-cols-1 lg:grid-cols-2 gap-3">
                  {rest.map((card) => (
                    <AssistCardView key={card.id} card={card} size="md" onPin={assist.setPinned} onDismiss={assist.dismiss} />
                  ))}
                </div>
              )}
            </div>
          )}
        </div>
      </section>
      {transcriptOpen && (
        <div className="w-[36%] min-w-[300px] max-w-[520px] flex-shrink-0 flex border-l border-gray-200" data-testid="assist-mode-transcript">
          <TranscriptPanel isProcessingStop={isProcessingStop} isStopping={isStopping} showModal={showModal} compact />
        </div>
      )}
    </div>
  );
}
