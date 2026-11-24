import { useCallback, useEffect, useState } from 'react';
import {
  DEFAULT_RECORDING_VIEW,
  RECORDING_VIEW_EVENT,
  RECORDING_VIEW_KEY,
  RecordingView,
  parseRecordingView,
} from '@/lib/recording-view';

function readStored(): RecordingView {
  try {
    return parseRecordingView(localStorage.getItem(RECORDING_VIEW_KEY));
  } catch {
    return DEFAULT_RECORDING_VIEW;
  }
}

/**
 * The recording view preference (WORK-757), shared by the recording page's
 * toggle and Settings › AI Assist. A change made in one place reaches every
 * mounted user at once through RECORDING_VIEW_EVENT.
 */
export function useRecordingView(): [RecordingView, (next: RecordingView) => void] {
  const [view, setViewState] = useState<RecordingView>(DEFAULT_RECORDING_VIEW);

  useEffect(() => {
    setViewState(readStored());
    const onChange = () => setViewState(readStored());
    window.addEventListener(RECORDING_VIEW_EVENT, onChange);
    window.addEventListener('storage', onChange);
    return () => {
      window.removeEventListener(RECORDING_VIEW_EVENT, onChange);
      window.removeEventListener('storage', onChange);
    };
  }, []);

  const setView = useCallback((next: RecordingView) => {
    try {
      localStorage.setItem(RECORDING_VIEW_KEY, next);
    } catch (e) {
      console.error('recording view: could not save the setting', e);
    }
    setViewState(next);
    window.dispatchEvent(new Event(RECORDING_VIEW_EVENT));
  }, []);

  return [view, setView];
}
