/**
 * Live engine setting (WORK-559). Contract with src-tauri/src/audio/pipecat.rs:
 * `live_engine_get` / `live_engine_set` / `live_engine_prepare`, and the
 * prepare-status event below (`pipecat::tests::frontend_matches_the_live_engine_contract`
 * checks both sides).
 */
export type LiveEngine = 'direct' | 'pipecat';

export const LIVE_ENGINE_PREPARE_EVENT = 'live-engine-prepare';

export interface LiveEnginePrepareStatus {
  state: 'preparing' | 'ready' | 'error';
  message?: string;
}
