'use client';

import { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { Pin, Sparkles } from 'lucide-react';
import { Button } from '@/components/ui/button';
import {
  ASSIST_AFTER_CALL_EVENT,
  AssistAfterCallEvent,
  AssistCard,
  AssistRecordData,
  BRIEF_ORIGIN,
  KIND_LABEL,
  RecordedAssistCard,
  clockFromSeconds,
} from '@/lib/live-assist';

/**
 * What Live Assist kept with this meeting (WORK-672), from its folder: the
 * after-call summary, the cards pinned during the call (live-assist.json),
 * and every card it showed (live-assist-cards.jsonl). Renders nothing for a
 * meeting Live Assist was not used on.
 */
export function AssistRecord({ meetingId }: { meetingId: string }) {
  const [pinned, setPinned] = useState<AssistCard[]>([]);
  const [record, setRecord] = useState<AssistRecordData | null>(null);
  const [writing, setWriting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    const [pins, rec] = await Promise.all([
      invoke<AssistCard[]>('live_assist_get_pinned', { meetingId }).catch((e) => {
        console.error('live assist: could not load pinned cards', e);
        return [] as AssistCard[];
      }),
      invoke<AssistRecordData>('live_assist_get_record', { meetingId }).catch((e) => {
        console.error('live assist: could not load the call record', e);
        return null;
      }),
    ]);
    return { pins, rec };
  }, [meetingId]);

  useEffect(() => {
    let cancelled = false;
    setPinned([]);
    setRecord(null);
    setError(null);
    load().then(({ pins, rec }) => {
      if (cancelled) return;
      setPinned(pins);
      setRecord(rec);
      setWriting(rec?.after_call_running ?? false);
    });
    return () => {
      cancelled = true;
    };
  }, [load]);

  // The automatic after-call call finishes after the recording stopped,
  // often while this page is already open.
  const folder = record?.folder ?? null;
  useEffect(() => {
    if (!folder) return;
    let cancelled = false;
    const unlisten = listen<AssistAfterCallEvent>(ASSIST_AFTER_CALL_EVENT, (event) => {
      if (cancelled || event.payload.folder !== folder) return;
      if (event.payload.state === 'running') {
        setWriting(true);
        setError(null);
        return;
      }
      setWriting(false);
      if (event.payload.state === 'error') setError(event.payload.message ?? 'The after-call summary failed.');
      load().then(({ pins, rec }) => {
        if (cancelled) return;
        setPinned(pins);
        if (rec) setRecord(rec);
      });
    });
    return () => {
      cancelled = true;
      unlisten.then((fn) => fn());
    };
  }, [folder, load]);

  const writeNow = async () => {
    setWriting(true);
    setError(null);
    try {
      await invoke('live_assist_run_after_call', { meetingId });
    } catch (e) {
      setError(String(e));
    } finally {
      setWriting(false);
      const { pins, rec } = await load();
      setPinned(pins);
      if (rec) setRecord(rec);
    }
  };

  const cards = record?.cards ?? [];
  const after = record?.after_call ?? null;
  const used = pinned.length > 0 || cards.length > 0 || after !== null || writing || (record?.has_brief ?? false);
  if (!record?.folder || !used) return null;

  return (
    <section
      className="mx-6 mt-4 rounded-lg border border-blue-200 bg-blue-50 p-4 text-gray-900"
      aria-label="Live Assist"
    >
      <div className="flex items-center justify-between gap-2 mb-2">
        <h3 className="flex items-center gap-2 text-sm font-semibold text-blue-900">
          <Sparkles className="w-4 h-4" />
          Live Assist after the call
        </h3>
        {!writing && (
          <Button variant="outline" size="sm" onClick={writeNow}>
            {after?.status === 'done' ? 'Write again' : 'Write the after-call summary'}
          </Button>
        )}
      </div>

      {writing && <p className="text-sm text-gray-600">Writing the after-call summary from the whole transcript…</p>}
      {error && !writing && <p className="text-sm text-red-700">{error}</p>}
      {!writing && after?.status === 'done' && (
        <>
          <div className="prose prose-sm max-w-none text-gray-900">
            <ReactMarkdown remarkPlugins={[remarkGfm]}>{after.output}</ReactMarkdown>
          </div>
          <p className="mt-2 text-xs text-gray-500">
            Written {new Date(after.created_at_ms).toLocaleString()} by {after.model} from {after.transcript_lines}{' '}
            transcript lines{after.transcript_clipped ? ' (the earliest part did not fit)' : ''}
            {after.had_brief ? ' and the meeting brief' : ''}. Check it against the transcript.
          </p>
        </>
      )}
      {!writing && !error && after?.status === 'error' && (
        <p className="text-sm text-red-700">The after-call summary failed: {after.error}</p>
      )}

      {pinned.length > 0 && (
        <div className="mt-4">
          <h4 className="flex items-center gap-2 text-xs font-semibold uppercase tracking-wide text-blue-900 mb-1">
            <Pin className="w-3 h-3" />
            Pinned during the call
          </h4>
          <ul className="space-y-2">
            {pinned.map((card) => (
              <li key={card.id} className="text-sm text-gray-900">
                <span className="text-[10px] font-semibold uppercase tracking-wide text-blue-700 mr-2">
                  {KIND_LABEL[card.kind]}
                </span>
                {card.text}
                {card.quote && <span className="block text-xs text-gray-500 italic">“{card.quote}”</span>}
              </li>
            ))}
          </ul>
        </div>
      )}

      {cards.length > 0 && (
        <details className="mt-4">
          <summary className="cursor-pointer text-xs font-semibold uppercase tracking-wide text-blue-900">
            All {cards.length} card{cards.length === 1 ? '' : 's'} shown during the call
          </summary>
          <ul className="mt-2 space-y-2">
            {cards.map((c) => (
              <RecordedCardRow key={c.card.id} recorded={c} />
            ))}
          </ul>
          {record.skipped > 0 && (
            <p className="mt-2 text-xs text-gray-500">
              {record.skipped} unreadable entr{record.skipped === 1 ? 'y was' : 'ies were'} left out.
            </p>
          )}
        </details>
      )}
    </section>
  );
}

function RecordedCardRow({ recorded }: { recorded: RecordedAssistCard }) {
  const { card, source } = recorded;
  const when = card.origin === BRIEF_ORIGIN ? 'brief' : source ? clockFromSeconds(source.start_s) : null;
  return (
    <li className={`text-sm ${recorded.retracted ? 'text-gray-500' : 'text-gray-900'}`}>
      {when && <span className="mr-2 font-mono text-xs text-gray-500">{when}</span>}
      <span className="text-[10px] font-semibold uppercase tracking-wide text-blue-700 mr-2">{KIND_LABEL[card.kind]}</span>
      <span className={recorded.retracted ? 'line-through' : undefined}>{card.text}</span>
      {recorded.pinned && <Pin className="inline w-3 h-3 ml-1 text-blue-700" aria-label="pinned" />}
      {card.quote && <span className="block text-xs text-gray-500 italic">“{card.quote}”</span>}
      {recorded.retracted && (
        <span className="block text-xs text-gray-500">
          Taken back: its line was the other side&apos;s audio picked up by the mic.
        </span>
      )}
    </li>
  );
}
