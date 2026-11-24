'use client';

import { useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { toast } from 'sonner';
import { ChevronDown, ChevronRight, ClipboardList, Plus, Search } from 'lucide-react';
import { briefEntry, type EarlierMeeting } from '@/lib/ai-assist-settings';
import { ASSIST_BRIEF_CLEARED_EVENT, ASSIST_BRIEF_LOADED_EVENT } from '@/lib/live-assist';

const SAVE_DELAY_MS = 600;

/**
 * This call's agenda, account and goals, sent with every assist call
 * (`ai_assist_set_brief`). Earlier meetings can be pulled in by searching
 * their titles and summaries. When the recording stops the brief is saved
 * with that meeting and the box is cleared for the next call.
 *
 * `autoOpen` (default true) opens the box when it holds a brief; AI Assist
 * mode turns it off during a call so the brief does not push the cards down
 * (WORK-757).
 */
export function MeetingBrief({ defaultOpen, autoOpen = true }: { defaultOpen: boolean; autoOpen?: boolean }) {
  const [brief, setBrief] = useState('');
  const [open, setOpen] = useState(defaultOpen);
  const [query, setQuery] = useState('');
  const [results, setResults] = useState<EarlierMeeting[] | null>(null);
  const saveTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const latest = useRef('');
  const autoOpenRef = useRef(autoOpen);
  autoOpenRef.current = autoOpen;

  useEffect(() => {
    invoke<string>('ai_assist_get_brief')
      .then((b) => {
        latest.current = b;
        setBrief(b);
        if (b && autoOpenRef.current) setOpen(true);
      })
      .catch((e) => console.error('ai assist: could not load the brief', e));
    // Only the brief that was kept with the stopped meeting is cleared: a
    // box already holding a brief typed for the next call keeps it (and its
    // pending save).
    const unlisten = listen<{ brief: string }>(ASSIST_BRIEF_CLEARED_EVENT, (e) => {
      if (latest.current !== e.payload.brief) return;
      if (saveTimer.current) {
        clearTimeout(saveTimer.current);
        saveTimer.current = null;
      }
      latest.current = '';
      setBrief('');
    });
    // A brief prepared from the calendar went into the empty box as a
    // recording started (WORK-677). The backend only loads it into an empty
    // box; one typed here but not yet saved still wins, and its pending save
    // replaces the prepared one.
    const unlistenLoaded = listen<{ brief: string }>(ASSIST_BRIEF_LOADED_EVENT, (e) => {
      if (latest.current.trim() !== '') return;
      latest.current = e.payload.brief;
      setBrief(e.payload.brief);
      if (autoOpenRef.current) setOpen(true);
    });
    return () => {
      unlisten.then((un) => un());
      unlistenLoaded.then((un) => un());
      if (saveTimer.current) {
        clearTimeout(saveTimer.current);
        flush(latest.current);
      }
    };
  }, []);

  const flush = (text: string) =>
    invoke('ai_assist_set_brief', { brief: text }).catch((e) =>
      toast.error('Could not save the meeting brief', { description: String(e) }),
    );

  const change = (text: string) => {
    latest.current = text;
    setBrief(text);
    if (saveTimer.current) clearTimeout(saveTimer.current);
    saveTimer.current = setTimeout(() => {
      saveTimer.current = null;
      flush(text);
    }, SAVE_DELAY_MS);
  };

  // Earlier meetings matching the words typed (title or summary).
  useEffect(() => {
    const q = query.trim();
    if (q.length < 2) {
      setResults(null);
      return;
    }
    let current = true;
    const t = setTimeout(() => {
      invoke<EarlierMeeting[]>('ai_assist_find_meetings', { query: q })
        .then((r) => current && setResults(r))
        .catch((e) => console.error('ai assist: meeting search failed', e));
    }, 250);
    return () => {
      current = false;
      clearTimeout(t);
    };
  }, [query]);

  const add = (m: EarlierMeeting) => {
    change(brief.trim() ? `${brief.trimEnd()}\n\n${briefEntry(m)}` : briefEntry(m));
  };

  return (
    <div className="border-b border-gray-200 bg-white" data-testid="meeting-brief">
      <button
        className="w-full flex items-center gap-2 px-3 py-2 text-left text-sm font-medium text-gray-800 hover:bg-gray-50"
        onClick={() => setOpen((o) => !o)}
      >
        {open ? <ChevronDown className="w-4 h-4" /> : <ChevronRight className="w-4 h-4" />}
        <ClipboardList className="w-4 h-4 text-gray-500" />
        <span className="flex-1">Meeting brief</span>
        {!open && brief && <span className="text-xs text-gray-500 truncate max-w-[9rem]">{brief.split('\n')[0]}</span>}
      </button>
      {open && (
        <div className="px-3 pb-3 space-y-2">
          <textarea
            aria-label="Meeting brief"
            rows={5}
            className="w-full rounded-md border border-gray-300 bg-white p-2 text-xs text-gray-900 placeholder:text-gray-500 focus:outline-none focus:ring-1 focus:ring-blue-400"
            placeholder="Agenda, account, who is on the call, what you want out of it…"
            value={brief}
            onChange={(e) => change(e.target.value)}
          />
          <div className="relative">
            <Search className="w-3.5 h-3.5 text-gray-400 absolute left-2 top-2" />
            <input
              aria-label="Find earlier meetings"
              className="w-full rounded-md border border-gray-300 bg-white pl-7 pr-2 py-1 text-xs text-gray-900 placeholder:text-gray-500"
              placeholder="Add earlier meetings: account, person, topic"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
          </div>
          {results && (
            <ul className="max-h-48 overflow-y-auto space-y-1">
              {results.length === 0 && <li className="text-xs text-gray-500">No earlier meeting mentions that.</li>}
              {results.map((m) => (
                <li key={m.id} className="flex items-start gap-2 rounded border border-gray-200 p-2">
                  <div className="flex-1 min-w-0">
                    <p className="text-xs font-medium text-gray-800 truncate" title={m.title}>
                      {m.title}
                    </p>
                    <p className="text-[11px] text-gray-500">
                      {m.created_at.slice(0, 10)}
                      {m.summary ? '' : ' · no summary yet'}
                    </p>
                  </div>
                  <button
                    className="text-blue-600 hover:text-blue-800 disabled:text-gray-300"
                    title="Add its summary to the brief"
                    disabled={brief.includes(briefEntry(m))}
                    onClick={() => add(m)}
                  >
                    <Plus className="w-4 h-4" />
                  </button>
                </li>
              ))}
            </ul>
          )}
          <p className="text-[11px] text-gray-500">
            Sent with every card. Saved with the meeting and cleared when the recording stops.
          </p>
        </div>
      )}
    </div>
  );
}
