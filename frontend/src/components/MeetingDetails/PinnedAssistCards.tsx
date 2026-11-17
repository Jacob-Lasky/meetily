'use client';

import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Pin } from 'lucide-react';
import { AssistCard, KIND_LABEL } from '@/lib/live-assist';

/**
 * Live-assist cards pinned during the recording (saved in the meeting folder
 * as live-assist.json). Renders nothing when the meeting has none.
 */
export function PinnedAssistCards({ meetingId }: { meetingId: string }) {
  const [cards, setCards] = useState<AssistCard[]>([]);

  useEffect(() => {
    let cancelled = false;
    setCards([]);
    invoke<AssistCard[]>('live_assist_get_pinned', { meetingId })
      .then((pinned) => {
        if (!cancelled) setCards(pinned);
      })
      .catch((e) => console.error('live assist: could not load pinned cards', e));
    return () => {
      cancelled = true;
    };
  }, [meetingId]);

  if (cards.length === 0) return null;

  return (
    <section className="mx-6 mt-4 rounded-lg border border-blue-200 bg-blue-50 p-4" aria-label="Pinned during the call">
      <h3 className="flex items-center gap-2 text-sm font-semibold text-blue-900 mb-2">
        <Pin className="w-4 h-4" />
        Pinned during the call
      </h3>
      <ul className="space-y-2">
        {cards.map((card) => (
          <li key={card.id} className="text-sm text-gray-900">
            <span className="text-[10px] font-semibold uppercase tracking-wide text-blue-700 mr-2">{KIND_LABEL[card.kind]}</span>
            {card.text}
            {card.quote && <span className="block text-xs text-gray-500 italic">“{card.quote}”</span>}
          </li>
        ))}
      </ul>
    </section>
  );
}
