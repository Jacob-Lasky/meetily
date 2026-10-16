/**
 * Which model the summary picker should switch to when a provider's model
 * list arrives or changes, or null to leave the current one alone.
 *
 * - a current model that is in the list stays;
 * - otherwise the model last used with this provider (cached) wins if listed;
 * - otherwise, if nothing is chosen yet or the current model was auto-picked
 *   from an earlier list (the no-key lineup before the account's live list),
 *   take the first candidate (lists put the newest or preferred first);
 * - a model the user chose or saved is never replaced, even when the list
 *   lacks it, so they still see what they picked.
 */
export function modelToApply(
  current: string,
  list: readonly string[],
  cached: string | undefined,
  autoPicked: string | null,
  // What an automatic pick may choose. Defaults to the whole list; Built-in AI
  // passes only downloaded models, while a saved model stays valid if listed.
  candidates: readonly string[] = list,
): { model: string; auto: boolean } | null {
  if (list.length === 0) return null;
  if (current && list.includes(current)) return null;
  if (cached && list.includes(cached)) return { model: cached, auto: false };
  if ((!current || current === autoPicked) && candidates.length > 0) {
    return { model: candidates[0], auto: true };
  }
  return null;
}

/**
 * Set `model` only if the config still has `provider`. Model lists resolve
 * asynchronously; by then the saved config may have loaded or the user may
 * have switched, and a model must never land on another provider.
 */
export function withModelIfProvider<T extends { provider: string; model: string }>(
  config: T,
  provider: string,
  model: string,
): T {
  return config.provider === provider ? { ...config, model } : config;
}

/**
 * Set `model` only if the config still has `provider` AND no model yet. For
 * automatic selections: evaluated inside the state update, it sees a saved
 * model that loaded while the list fetch was in flight, which a value read
 * at render time can miss.
 */
export function withModelIfUnset<T extends { provider: string; model: string }>(
  config: T,
  provider: string,
  model: string,
): T {
  return config.provider === provider && !config.model ? { ...config, model } : config;
}
