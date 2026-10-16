import { describe, expect, test } from 'bun:test';
import { toUiTranscriptProvider } from '../../src/lib/transcript-providers';

describe('toUiTranscriptProvider', () => {
  test('keeps providers this build runs', () => {
    expect(toUiTranscriptProvider('deepgram')).toBe('deepgram');
    expect(toUiTranscriptProvider('parakeet')).toBe('parakeet');
    expect(toUiTranscriptProvider('localWhisper')).toBe('localWhisper');
  });

  test('shows retired or unknown saved providers as Parakeet, like the migration', () => {
    for (const saved of ['groq', 'openai', 'elevenLabs', '', null, undefined]) {
      expect(toUiTranscriptProvider(saved)).toBe('parakeet');
    }
  });
});
