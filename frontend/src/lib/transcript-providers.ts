/**
 * Transcription providers this build can run. Earlier builds also listed
 * groq, openai and elevenLabs, which never had a Rust engine, so a database
 * can still hold those names.
 */
export const TRANSCRIPT_PROVIDERS = ['localWhisper', 'parakeet', 'deepgram'] as const;
export type TranscriptProvider = (typeof TRANSCRIPT_PROVIDERS)[number];

/**
 * Map a saved provider onto one the settings UI can show. A migration
 * (20261016000000) already moves unsupported saved providers to Parakeet, the
 * fresh-install default; this mirrors it so a stray value can never crash the
 * view or show a provider the database does not hold.
 */
export function toUiTranscriptProvider(provider: string | null | undefined): TranscriptProvider {
  return (TRANSCRIPT_PROVIDERS as readonly string[]).includes(provider ?? '')
    ? (provider as TranscriptProvider)
    : 'parakeet';
}
