'use client';

import { useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { toast } from 'sonner';
import { CALENDAR_AUTO_RECORD_EVENT, type AutoRecordEvent } from '@/lib/calendar';

/**
 * In-app notice for recordings the calendar starts, stops or skips
 * (WORK-677). A start comes with a Stop button; the desktop notification
 * carries the same button on Linux.
 */
export function CalendarAutoRecordNotice() {
  useEffect(() => {
    const unlisten = listen<AutoRecordEvent>(CALENDAR_AUTO_RECORD_EVENT, ({ payload }) => {
      const title = payload.title || 'Calendar meeting';
      switch (payload.state) {
        case 'started':
          toast(`Recording "${title}"`, {
            id: `calendar-${payload.eventId}`,
            description: 'Started from your calendar.',
            duration: 60_000,
            action: {
              label: 'Stop recording',
              onClick: () => {
                invoke('calendar_cancel_auto_record', { eventId: payload.eventId }).catch((e) =>
                  toast.error('Could not stop the recording', { description: String(e) }),
                );
              },
            },
          });
          break;
        case 'stopped':
          toast(`Stopped recording "${title}"`, { id: `calendar-${payload.eventId}`, description: payload.message });
          break;
        case 'skipped':
          toast(`Did not start "${title}"`, { description: payload.message });
          break;
        case 'failed':
          toast.error(`Could not record "${title}"`, { description: payload.message });
          break;
      }
    });
    return () => {
      unlisten.then((f) => f()).catch(() => {});
    };
  }, []);
  return null;
}
