import { useCallback, useRef } from 'react';

/** How close to the bottom (px) still counts as following the transcript. */
const AT_BOTTOM_TOLERANCE_PX = 10;

function isAtBottom(el: HTMLElement): boolean {
  return el.scrollTop + el.clientHeight >= el.scrollHeight - AT_BOTTOM_TOLERANCE_PX;
}

/**
 * Whether the reader is following the bottom of the live transcript panel,
 * and a way to keep them there as content grows.
 *
 * `setContainer` is a callback ref: it moves the scroll listener to whichever
 * panel element is mounted now. A newly mounted panel keeps the previous
 * answer until the reader scrolls it: it mounts at the top, which is where
 * the app put it, not where the reader went. DO NOT go back to binding it once in an
 * effect: the provider outlives the panel, so after Settings and back the
 * listener stayed on the old, detached element and the at-bottom answer
 * froze, pulling a reader who had scrolled up back to the bottom on every
 * update (WORK-580).
 */
export function useScrollFollow() {
  const isUserAtBottomRef = useRef<boolean>(true);
  const containerRef = useRef<HTMLDivElement | null>(null);
  const detachRef = useRef<(() => void) | null>(null);

  const setContainer = useCallback((el: HTMLDivElement | null) => {
    detachRef.current?.();
    detachRef.current = null;
    containerRef.current = el;
    if (!el) return;
    const onScroll = () => {
      isUserAtBottomRef.current = isAtBottom(el);
    };
    el.addEventListener('scroll', onScroll);
    detachRef.current = () => el.removeEventListener('scroll', onScroll);
  }, []);

  // Content grew below the rows: keep up with it only if the reader was at
  // the bottom before it did. Growth fires no scroll event, so
  // isUserAtBottomRef still holds the pre-update answer.
  const followBottom = useCallback(() => {
    const el = containerRef.current;
    if (isUserAtBottomRef.current && el) {
      el.scrollTop = el.scrollHeight;
    }
  }, []);

  return { containerRef, setContainer, isUserAtBottomRef, followBottom };
}
