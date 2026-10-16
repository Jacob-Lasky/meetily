/**
 * Which model the summary picker should switch to when a provider's model
 * list arrives or changes, or null to leave the current one alone.
 *
 * - a current model that is in the list stays;
 * - otherwise the model last used with this provider (cached) wins if listed;
 * - otherwise, if nothing is chosen yet or the current model was auto-picked
 *   from an earlier list (the no-key lineup before the account's live list),
 *   take the newest, which lists put first;
 * - a model the user chose or saved is never replaced, even when the list
 *   lacks it, so they still see what they picked.
 */
export function modelToApply(
  current: string,
  list: readonly string[],
  cached: string | undefined,
  autoPicked: string | null,
): { model: string; auto: boolean } | null {
  if (list.length === 0) return null;
  if (current && list.includes(current)) return null;
  if (cached && list.includes(cached)) return { model: cached, auto: false };
  if (!current || current === autoPicked) return { model: list[0], auto: true };
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
