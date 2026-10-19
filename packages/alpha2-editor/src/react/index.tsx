import {
  createContext,
  createElement,
  useContext,
  useEffect,
  useRef,
  useSyncExternalStore,
} from "react";
import type { ReactNode, CSSProperties } from "react";
import type { EditorSession, EditorSnapshot } from "../core/session.js";
import type { BrowserBridge } from "../browser/types.js";

const Context = createContext<EditorSession | null>(null);
/**
 * Makes `editor` available to {@link useEditor} and {@link useEditorSelector} below it. The caller
 * owns the session; mounting or unmounting a provider never disposes it.
 */
export function EditorProvider({
  editor,
  children,
}: {
  editor: EditorSession;
  children: ReactNode;
}) {
  return createElement(Context.Provider, { value: editor }, children);
}
/** The session of the nearest {@link EditorProvider}. Throws when there is none. */
export function useEditor(): EditorSession {
  const editor = useContext(Context);
  if (!editor) throw new Error("EditorProvider is missing");
  return editor;
}
/**
 * Subscribes to one value derived from the session's snapshot; the component re-renders when it
 * changes by `equal` (`Object.is` by default). Inline selectors are fine: the result is cached per
 * snapshot and an `equal` result keeps the previous reference, even when the selector function
 * itself is new on every render, so a selector that builds arrays or objects does not re-render
 * in a loop. Throws without an {@link EditorProvider}.
 */
export function useEditorSelector<T>(
  selector: (snapshot: EditorSnapshot) => T,
  equal: (a: T, b: T) => boolean = Object.is,
): T {
  const editor = useEditor();
  const cache = useRef<{
    editor: EditorSession;
    snapshot: EditorSnapshot;
    selector: (snapshot: EditorSnapshot) => T;
    value: T;
  } | null>(null);
  const getSelection = () => {
    const snapshot = editor.getSnapshot();
    const previous = cache.current;
    if (
      previous &&
      previous.editor === editor &&
      previous.snapshot === snapshot &&
      previous.selector === selector
    )
      return previous.value;
    const next = selector(snapshot);
    const value =
      previous && previous.editor === editor && equal(previous.value, next)
        ? previous.value
        : next;
    cache.current = { editor, snapshot, selector, value };
    return value;
  };
  return useSyncExternalStore(editor.subscribe, getSelection, getSelection);
}

// Binds and unbinds are serialized per bridge, so a second surface can mount while the first is
// still initializing. Each effect run owns a fresh canvas, including StrictMode's double run.
const bindings = new WeakMap<BrowserBridge, Promise<void>>();
/**
 * Shows `bridge`'s preview in a canvas filling a `div` (styled by `className` and `style`). The
 * canvas is bound on mount and released on unmount; binds of one bridge run one at a time.
 * `onReady` fires once the canvas is bound; `onError` receives a bind or release failure.
 */
export function EditorPreview({
  bridge,
  className,
  style,
  onReady,
  onError,
}: {
  bridge: BrowserBridge;
  className?: string;
  style?: CSSProperties;
  onReady?: () => void;
  onError?: (error: unknown) => void;
}) {
  const host = useRef<HTMLDivElement>(null);
  const callbacks = useRef({ onReady, onError });
  callbacks.current = { onReady, onError };
  useEffect(() => {
    const container = host.current;
    if (!container) return;
    const canvas = document.createElement("canvas");
    canvas.setAttribute("aria-label", "Sequence preview");
    canvas.style.cssText =
      "display:block;width:100%;height:100%;object-fit:contain";
    container.append(canvas);
    let cancelled = false;
    let binding: Awaited<ReturnType<BrowserBridge["bindPreview"]>> | undefined;
    const setup = (bindings.get(bridge) ?? Promise.resolve())
      .then(async () => {
        if (cancelled) return;
        binding = await bridge.bindPreview(canvas);
        if (cancelled) {
          await binding.dispose();
          binding = undefined;
        } else callbacks.current.onReady?.();
      })
      .catch((error: unknown) => {
        if (!cancelled) callbacks.current.onError?.(error);
      });
    bindings.set(bridge, setup);
    return () => {
      cancelled = true;
      canvas.remove();
      const cleanup = setup.then(async () => {
        await binding?.dispose();
      });
      bindings.set(
        bridge,
        cleanup.catch((error: unknown) => callbacks.current.onError?.(error)),
      );
    };
  }, [bridge]);
  return createElement("div", { ref: host, className, style });
}
