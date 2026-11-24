'use client';

import { LayoutPanelLeft, Sparkles } from 'lucide-react';
import { useRecordingView } from '@/hooks/useRecordingView';
import type { RecordingView } from '@/lib/recording-view';

const OPTIONS: { value: RecordingView; label: string; help: string; Icon: typeof Sparkles }[] = [
  {
    value: 'assist',
    label: 'AI Assist mode',
    help: 'The cards fill the screen, newest largest. The transcript shrinks to the last words from each side, and opens beside the cards when you want it.',
    Icon: Sparkles,
  },
  {
    value: 'transcript',
    label: 'Transcript first',
    help: 'The full transcript, with the cards in a side panel.',
    Icon: LayoutPanelLeft,
  },
];

/**
 * Settings › AI Assist: the recording view's layout (WORK-757). The same
 * preference as the toggle on the recording page, which switches it mid-call.
 * Layout only; recording, transcription and the cards work the same in both.
 */
export function RecordingViewSettings() {
  const [view, setView] = useRecordingView();
  return (
    <div className="bg-white rounded-lg border border-gray-200 p-6 shadow-sm" data-testid="recording-view-settings">
      <h3 className="text-lg font-semibold text-gray-900 mb-1">Recording view</h3>
      <p className="text-sm text-gray-600 mb-4">
        How the recording page looks while live assist is on. You can also switch during a call with the toggle at the
        top of the cards. With live assist off the page always shows the transcript.
      </p>
      <div className="grid grid-cols-1 md:grid-cols-2 gap-3" role="radiogroup" aria-label="Recording view">
        {OPTIONS.map(({ value, label, help, Icon }) => (
          <button
            key={value}
            type="button"
            role="radio"
            aria-checked={view === value}
            onClick={() => setView(value)}
            className={`text-left rounded-lg border p-4 ${
              view === value ? 'border-blue-500 ring-1 ring-blue-500 bg-blue-50' : 'border-gray-200 hover:border-gray-300'
            }`}
          >
            <div className="flex items-center gap-2 mb-1">
              <Icon className="w-4 h-4 text-gray-700" />
              <span className="text-sm font-semibold text-gray-900">{label}</span>
            </div>
            <p className="text-xs text-gray-600">{help}</p>
          </button>
        ))}
      </div>
    </div>
  );
}
