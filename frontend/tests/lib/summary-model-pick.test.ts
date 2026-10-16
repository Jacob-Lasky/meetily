import { describe, expect, test } from 'bun:test';
import { modelToApply, withModelIfProvider, withModelIfUnset } from '../../src/lib/summary-model-pick';

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

describe('modelToApply candidates', () => {
  const CATALOG = ['qwen3.5:2b', 'qwen3.5:4b', 'gemma4:e2b'];
  const ON_DISK = ['qwen3.5:4b'];

  test('auto-picks only from candidates', () => {
    expect(modelToApply('', CATALOG, undefined, null, ON_DISK)).toEqual({ model: 'qwen3.5:4b', auto: true });
  });

  test('keeps a saved model that is listed but not a candidate', () => {
    expect(modelToApply('gemma4:e2b', CATALOG, undefined, null, ON_DISK)).toBeNull();
  });

  test('picks nothing when there are no candidates', () => {
    expect(modelToApply('', CATALOG, undefined, null, [])).toBeNull();
  });
});

describe('withModelIfUnset', () => {
  test('fills an empty model on the same provider', () => {
    expect(withModelIfUnset({ provider: 'builtin-ai', model: '' }, 'builtin-ai', 'qwen3.5:4b').model).toBe('qwen3.5:4b');
  });

  test('does not replace a saved model that loaded while the fetch was in flight', () => {
    const saved = { provider: 'builtin-ai', model: 'gemma4:e4b' };
    expect(withModelIfUnset(saved, 'builtin-ai', 'qwen3.5:4b')).toBe(saved);
  });

  test('does not touch another provider', () => {
    const saved = { provider: 'openai', model: '' };
    expect(withModelIfUnset(saved, 'builtin-ai', 'qwen3.5:4b')).toBe(saved);
  });
});
