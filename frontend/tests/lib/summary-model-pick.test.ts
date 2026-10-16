import { describe, expect, test } from 'bun:test';
import { modelToApply, withModelIfProvider } from '../../src/lib/summary-model-pick';

const NO_KEY = ['gpt-6-astra', 'gpt-6.1-sol', 'gpt-6-luna'];
const LIVE = ['gpt-6.1-sol', 'gpt-6-luna', 'gpt-5.5'];

describe('modelToApply', () => {
  test('leaves a listed model alone', () => {
    expect(modelToApply('gpt-6-luna', LIVE, 'gpt-5.5', null)).toBeNull();
  });

  test('restores the cached model when it is listed', () => {
    expect(modelToApply('', LIVE, 'gpt-5.5', null)).toEqual({ model: 'gpt-5.5', auto: false });
  });

  test('fills an empty model with the newest', () => {
    expect(modelToApply('', NO_KEY, undefined, null)).toEqual({ model: 'gpt-6-astra', auto: true });
  });

  test('replaces an auto-picked no-key model the live list lacks', () => {
    expect(modelToApply('gpt-6-astra', LIVE, undefined, 'gpt-6-astra')).toEqual({
      model: 'gpt-6.1-sol',
      auto: true,
    });
  });

  test('never replaces a model the user chose, even if unlisted', () => {
    expect(modelToApply('gpt-6-astra', LIVE, undefined, null)).toBeNull();
  });

  test('does nothing for an empty list', () => {
    expect(modelToApply('', [], 'gpt-5.5', null)).toBeNull();
  });
});

describe('withModelIfProvider', () => {
  test('applies the model when the provider still matches', () => {
    expect(withModelIfProvider({ provider: 'builtin-ai', model: '' }, 'builtin-ai', 'qwen3.5:4b')).toEqual({
      provider: 'builtin-ai',
      model: 'qwen3.5:4b',
    });
  });

  test('ignores a late result after the provider changed', () => {
    const saved = { provider: 'openai', model: 'gpt-6-luna' };
    expect(withModelIfProvider(saved, 'builtin-ai', 'qwen3.5:4b')).toBe(saved);
  });
});
