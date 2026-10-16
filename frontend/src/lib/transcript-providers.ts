/**
 * Transcription providers this build can run. Earlier builds also listed
 * groq, openai and elevenLabs, which never had a Rust engine, so a database
 * can still hold those names.
 */
export const TRANSCRIPT_PROVIDERS = ['localWhisper', 'parakeet', 'deepgram'] as const;
export type TranscriptProvider = (typeof TRANSCRIPT_PROVIDERS)[number];

/**
 * Map a saved provider onto one the settings UI can show. Anything unknown
 * becomes Local Whisper because that is what the Rust engine runs for an
 * unknown provider (`"localWhisper" | _` in transcription/engine.rs).
 */
export function toUiTranscriptProvider(provider: string | null | undefined): TranscriptProvider {
  return (TRANSCRIPT_PROVIDERS as readonly string[]).includes(provider ?? '')
    ? (provider as TranscriptProvider)
    : 'localWhisper';
}
