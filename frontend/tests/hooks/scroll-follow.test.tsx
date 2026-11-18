import { describe, expect, test } from 'bun:test';
import { act, create } from 'react-test-renderer';
import { useScrollFollow } from '../../src/hooks/useScrollFollow';

/** A stand-in for the panel's scroll element: 1000 px of content in a 200 px viewport. */
function fakePanel() {
  const listeners = new Set<() => void>();
  return {
    scrollTop: 800,
    scrollHeight: 1000,
    clientHeight: 200,
    addEventListener: (type: string, fn: () => void) => { if (type === 'scroll') listeners.add(fn); },
    removeEventListener: (type: string, fn: () => void) => { if (type === 'scroll') listeners.delete(fn); },
    scrollUp() { this.scrollTop = 100; listeners.forEach((fn) => fn()); },
  };
}

describe('transcript scroll follow', () => {
  test('tracks the panel mounted now, not the first one (Settings and back)', () => {
    const panels = [fakePanel(), fakePanel()];
    let mounted = 0;
    let api: ReturnType<typeof useScrollFollow> | undefined;
    // The provider stays mounted; only the panel under it comes and goes.
    function Provider({ showPanel }: { showPanel: boolean }) {
      api = useScrollFollow();
      return showPanel ? <div ref={api.setContainer} /> : null;
    }
    const options = { createNodeMock: () => panels[mounted++] };
    let renderer: ReturnType<typeof create>;
    act(() => { renderer = create(<Provider showPanel />, options); });
    act(() => { renderer.update(<Provider showPanel={false} />); });
    act(() => { renderer.update(<Provider showPanel />); });

    const current = panels[1];
    current.scrollUp();
    api!.followBottom();
    expect(current.scrollTop).toBe(100);

    act(() => { renderer.unmount(); });
  });

  test('a reader who was following still follows on the remounted panel, which mounts at the top', () => {
    const panels = [fakePanel(), { ...fakePanel(), scrollTop: 0 }];
    let mounted = 0;
    let api: ReturnType<typeof useScrollFollow> | undefined;
    function Provider({ showPanel }: { showPanel: boolean }) {
      api = useScrollFollow();
      return showPanel ? <div ref={api.setContainer} /> : null;
    }
    const options = { createNodeMock: () => panels[mounted++] };
    let renderer: ReturnType<typeof create>;
    act(() => { renderer = create(<Provider showPanel />, options); });
    act(() => { renderer.update(<Provider showPanel={false} />); });
    act(() => { renderer.update(<Provider showPanel />); });
    api!.followBottom();
    expect(panels[1].scrollTop).toBe(1000);
    act(() => { renderer.unmount(); });
  });

  test('follows when the reader is at the bottom of the current panel', () => {
    const panel = fakePanel();
    let api: ReturnType<typeof useScrollFollow> | undefined;
    function Provider() {
      api = useScrollFollow();
      return <div ref={api.setContainer} />;
    }
    let renderer: ReturnType<typeof create>;
    act(() => { renderer = create(<Provider />, { createNodeMock: () => panel }); });
    panel.scrollHeight = 1300; // the grey line grew; no scroll event
    api!.followBottom();
    expect(panel.scrollTop).toBe(1300);
    act(() => { renderer.unmount(); });
  });
});
