'use client';

import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Sparkles } from 'lucide-react';
import { toast } from 'sonner';
import { Switch } from './ui/switch';

/**
 * Settings > Summary: the live-assist switch. Off by default; while off the
 * app makes no live-assist model calls. Stored by the backend
 * (live_assist_set_enabled) so it applies while the window is hidden too.
 */
export function LiveAssistSettings() {
  const [enabled, setEnabled] = useState<boolean | null>(null);

  useEffect(() => {
    invoke<boolean>('live_assist_get_enabled')
      .then(setEnabled)
      .catch((e) => {
        console.error('live assist: could not read the setting', e);
        setEnabled(false);
      });
  }, []);

  const onChange = async (next: boolean) => {
    const previous = enabled;
    setEnabled(next);
    try {
      await invoke('live_assist_set_enabled', { enabled: next });
    } catch (e) {
      setEnabled(previous);
      toast.error('Could not save the live assist setting', { description: String(e) });
    }
  };

  return (
    <div className="bg-white rounded-lg border border-gray-200 p-6 shadow-sm">
      <div className="flex items-center justify-between">
        <div className="flex-1">
          <div className="flex items-center gap-2 mb-2">
            <Sparkles className="h-5 w-5 text-gray-600" />
            <h3 className="text-lg font-semibold text-gray-900">Live assist during calls</h3>
          </div>
          <p className="text-sm text-gray-600">
            While recording, shows short suggestion cards (answers, follow-up questions, facts, to-dos) after the
            other side speaks. Uses the summary model above, at most one call every 5 seconds. Needs the Deepgram
            provider recording system audio, which is how the app tells your microphone from the other side.
          </p>
        </div>
        <div className="ml-6">
          <Switch
            checked={enabled === true}
            disabled={enabled === null}
            onCheckedChange={onChange}
            aria-label="Live assist during calls"
          />
        </div>
      </div>
    </div>
  );
}
