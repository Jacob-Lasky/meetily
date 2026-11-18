'use client';

import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { toast } from 'sonner';
import { Label } from './ui/label';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from './ui/select';
import {
  LIVE_ENGINE_PREPARE_EVENT,
  type LiveEngine,
  type LiveEnginePrepareStatus,
} from '@/lib/live-engine';

/**
 * Settings > Transcription > Deepgram: which engine transcribes live
 * recordings. Direct (the default) streams to Deepgram as before; Pipecat
 * runs Deepgram Flux per channel in a local Pipecat sidecar and triggers live
 * assist on Flux's end-of-turn events. Switching back to Direct is the
 * rollback. The choice applies from the next recording.
 */
export function LiveEngineSettings() {
  const [engine, setEngine] = useState<LiveEngine | null>(null);
  const [prepare, setPrepare] = useState<LiveEnginePrepareStatus | null>(null);

  useEffect(() => {
    invoke<LiveEngine>('live_engine_get')
      .then(setEngine)
      .catch((e) => {
        console.error('live engine: could not read the setting', e);
        setEngine('direct');
      });
    const unlisten = listen<LiveEnginePrepareStatus>(LIVE_ENGINE_PREPARE_EVENT, (e) => setPrepare(e.payload));
    return () => {
      unlisten.then((un) => un());
    };
  }, []);

  const onChange = async (next: string) => {
    const value = next as LiveEngine;
    const previous = engine;
    setEngine(value);
    try {
      await invoke('live_engine_set', { engine: value });
    } catch (e) {
      setEngine(previous);
      toast.error('Could not save the live engine', { description: String(e) });
    }
  };

  return (
    <div>
      <Label className="block text-sm font-medium text-gray-700 mb-1">Live engine</Label>
      <Select value={engine ?? undefined} onValueChange={onChange} disabled={engine === null}>
        <SelectTrigger className="focus:ring-1 focus:ring-blue-500 focus:border-blue-500" aria-label="Live engine">
          <SelectValue placeholder="Loading…" />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="direct">Direct (nova-3)</SelectItem>
          <SelectItem value="pipecat">Pipecat (Flux)</SelectItem>
        </SelectContent>
      </Select>
      <p className="text-xs text-gray-500 mt-1 mx-1">
        {engine === 'pipecat'
          ? 'Live recordings go through a local Pipecat pipeline running Deepgram Flux on each channel; live assist answers when the other side finishes a turn. Needs uv installed. If the pipeline stops, the recording continues on Direct.'
          : 'Live recordings stream straight to Deepgram with the model above.'}{' '}
        Applies from the next recording.
      </p>
      {engine === 'pipecat' && prepare && (
        <p
          className={`text-xs mt-1 mx-1 ${prepare.state === 'error' ? 'text-red-600' : 'text-gray-500'}`}
          role="status"
        >
          {prepare.state === 'preparing' && 'Setting up the Pipecat engine (first time only)…'}
          {prepare.state === 'ready' && 'Pipecat engine ready.'}
          {prepare.state === 'error' && `Pipecat engine setup failed: ${prepare.message ?? 'unknown error'}`}
        </p>
      )}
    </div>
  );
}
