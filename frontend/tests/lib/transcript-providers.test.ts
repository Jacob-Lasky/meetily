import { describe, expect, test } from 'bun:test';
import { toUiTranscriptProvider } from '../../src/lib/transcript-providers';

describe('toUiTranscriptProvider', () => {
  test('keeps providers this build runs', () => {
    expect(toUiTranscriptProvider('deepgram')).toBe('deepgram');
    expect(toUiTranscriptProvider('parakeet')).toBe('parakeet');
    expect(toUiTranscriptProvider('localWhisper')).toBe('localWhisper');
  });

  test('shows retired or unknown saved providers as Local Whisper, like the engine', () => {
    for (const saved of ['groq', 'openai', 'elevenLabs', '', null, undefined]) {
      expect(toUiTranscriptProvider(saved)).toBe('localWhisper');
    }
  });
});
