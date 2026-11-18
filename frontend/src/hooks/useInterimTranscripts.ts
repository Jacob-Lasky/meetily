import { useEffect, useState } from 'react';
import { transcriptService } from '@/services/transcriptService';
import { applyInterim, type InterimLine } from '@/lib/transcript-interim';

/**
 * How long a channel's update waits for the final it may follow to render
 * (that buffer is 10 ms). Deepgram sends a channel's updates 200+ ms
 * apart, so the delay never holds a channel's line back by more than this.
 */
const UPDATE_DELAY_MS = 30;

export interface InterimTranscripts {
  /** Each channel's not-yet-final words, in speaking order. */
  lines: InterimLine[];
  /**
   * True once this recording has shown any interim words. The transcript then
   * shows finals at once instead of typing them out: the reader already saw
   * the words greyed, and retyping them would make the line blink out and
   * crawl back.
   */
  receiving: boolean;
}

/**
 * The live greyed lines (Deepgram interim words per channel) while recording.
 * Lines clear when recording stops or pauses, so no half-sentence lingers
 * after the last final.
 */
export function useInterimTranscripts(isRecording: boolean, isPaused: boolean): InterimTranscripts {
  const [lines, setLines] = useState<InterimLine[]>([]);
  const [receiving, setReceiving] = useState(false);
  const active = isRecording && !isPaused;

  useEffect(() => {
    if (!isRecording) {
      setReceiving(false);
    }
  }, [isRecording]);

  useEffect(() => {
    if (!active) {
      setLines([]);
      return;
    }
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    const pending = new Map<string | null, ReturnType<typeof setTimeout>>();
    transcriptService
      .onTranscriptInterim((update) => {
        if (cancelled) return;
        // Every update for a channel waits a moment, and a newer one replaces
        // it: the final an update follows reaches the transcript through a
        // 10 ms buffer (TranscriptContext), so applying the update first
        // would blank or duplicate the line for that moment.
        const key = update.speaker ?? null;
        clearTimeout(pending.get(key));
        pending.set(key, setTimeout(() => {
          pending.delete(key);
          if (cancelled) return;
          if (update.text.trim()) setReceiving(true);
          setLines((current) => applyInterim(current, update));
        }, UPDATE_DELAY_MS));
      })
      .then((fn) => {
        if (cancelled) {
          fn();
        } else {
          unlisten = fn;
        }
      })
      .catch((e) => console.error('Failed to listen for interim transcripts:', e));
    return () => {
      cancelled = true;
      pending.forEach((timer) => clearTimeout(timer));
      unlisten?.();
      setLines([]);
    };
  }, [active]);

  return { lines, receiving };
}
