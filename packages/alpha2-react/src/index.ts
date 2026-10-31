/**
 * `<Alpha2Canvas>` makes a browser player for each canvas it mounts, presents the requested time,
 * and disposes the player when the canvas goes.
 *
 * The player renders offscreen at project size and the element keeps the project's aspect ratio
 * while scaling with its container. Hidden pages pause presentation and show the latest time when
 * visible again. When device-loss recovery needs a new canvas (a lost WebGL2 context, or the CPU
 * renderer after GPU loss), the component swaps a new element in place.
 *
 * {@link bindCanvas} is the same wiring without React.
 */

import {
  createElement,
  useEffect,
  useRef,
  type CSSProperties,
  type ReactElement,
} from "react";

/** A presentation's outcome, as `alpha2/player` reports it. */
export interface PresentedFrame {
  readonly requestedTimeTicks: bigint;
  /** The time whose picture is on screen; absent when nothing was presented. */
  readonly presentedTimeTicks?: bigint;
  /** Sources the presentation lacked a frame of. */
  readonly missingSources: readonly string[];
  /** Whether the previous picture was shown again instead of a new one. */
  readonly repeated: boolean;
}

/** The part of `Alpha2Player` (`alpha2/player`) a canvas binding drives. */
export interface CanvasPresenter {
  /** Takes over `canvas`. Rejects when the player cannot draw to it. */
  initialize(canvas: OffscreenCanvas): Promise<void>;
  /** Draws the frame at `timeTicks`; `undefined` when a later request superseded it. */
  present(timeTicks: bigint): Promise<PresentedFrame | undefined>;
}

/** A player `<Alpha2Canvas>` makes and disposes: `Alpha2Player` from `alpha2/player`. */
export interface CanvasPlayer extends CanvasPresenter {
  dispose(): void;
}

/** What the page's visibility is read from; `document` by default. */
export interface VisibilitySource {
  readonly visibilityState: DocumentVisibilityState;
  addEventListener(type: "visibilitychange", listener: () => void): void;
  removeEventListener(type: "visibilitychange", listener: () => void): void;
}

/** Options of {@link bindCanvas}. */
export interface CanvasBindingOptions {
  /** Called with each presentation that was not superseded. */
  readonly onPresented?: (frame: PresentedFrame) => void;
  /**
   * Called when initialization or a presentation fails, until the binding is disposed. Without it
   * a failure goes to the page's `reportError`.
   */
  readonly onError?: (error: unknown) => void;
  /** The page's visibility; `document` by default. */
  readonly visibility?: VisibilitySource;
  /**
   * Puts a new canvas of the same drawing-buffer size where the bound one is, and resolves with it.
   * Without it {@link CanvasBinding.replaceSurface} rejects.
   */
  readonly replaceCanvas?: () => Promise<{
    transferControlToOffscreen(): OffscreenCanvas;
  }>;
}

/** A canvas bound to a player. */
export interface CanvasBinding {
  /** Resolves once the player has its canvas; rejects if it cannot have it. */
  readonly ready: Promise<void>;
  /**
   * Shows the frame at `timeTicks`: now, once initialized, or when the page is next visible. Times
   * given while a presentation runs coalesce into one presentation of the latest. Showing the same
   * time again presents it again.
   */
  show(timeTicks: bigint): void;
  /**
   * A new surface from `replaceCanvas`, transferred to an `OffscreenCanvas`, for `Alpha2Player`'s
   * `replaceSurface` option. Rejects without `replaceCanvas`, after {@link CanvasBinding.dispose},
   * or when the new canvas cannot be transferred.
   */
  replaceSurface(): Promise<OffscreenCanvas>;
  /**
   * Stops presenting. The player stays its owner's to dispose; failures of work still in flight are
   * not reported. Idempotent.
   */
  dispose(): void;
}

function reportUnhandled(error: unknown): void {
  if (typeof reportError === "function") reportError(error);
  else console.error(error);
}

/**
 * Gives `player` the canvas, which it takes over, and presents the times it is shown.
 *
 * The player draws at the canvas's drawing-buffer size, which cannot change after transfer, so set
 * `canvas.width` and `canvas.height` to the project's size first.
 *
 * Never throws: a canvas that cannot be transferred, or a failed initialization, rejects `ready`
 * and is reported as `onError` describes.
 */
export function bindCanvas(
  player: CanvasPresenter,
  canvas: { transferControlToOffscreen(): OffscreenCanvas },
  options: CanvasBindingOptions = {},
): CanvasBinding {
  const visibility = options.visibility ?? document;
  let initialized = false;
  let disposed = false;
  let presenting = false;
  let wanted: bigint | undefined;
  let pending = false;

  const fail = (error: unknown): void => {
    if (disposed) return;
    (options.onError ?? reportUnhandled)(error);
  };
  const pump = async (): Promise<void> => {
    if (presenting || disposed || !initialized) return;
    presenting = true;
    try {
      while (
        !disposed &&
        pending &&
        wanted !== undefined &&
        visibility.visibilityState === "visible"
      ) {
        pending = false;
        try {
          const frame = await player.present(wanted);
          if (frame !== undefined && !disposed) options.onPresented?.(frame);
        } catch (error) {
          // A time requested meanwhile, or from the handler, is still shown.
          fail(error);
        }
      }
    } finally {
      presenting = false;
    }
  };
  const onVisibility = (): void => void pump();

  let ready: Promise<void>;
  try {
    const offscreen = canvas.transferControlToOffscreen();
    visibility.addEventListener("visibilitychange", onVisibility);
    ready = player.initialize(offscreen).then(() => {
      initialized = true;
      void pump();
    });
  } catch (error) {
    ready = Promise.reject(
      error instanceof Error ? error : new Error(String(error)),
    );
  }
  ready.catch(fail);

  return {
    ready,
    show(timeTicks) {
      wanted = timeTicks;
      pending = true;
      void pump();
    },
    async replaceSurface() {
      if (disposed) throw new Error("the canvas binding is disposed");
      if (options.replaceCanvas === undefined)
        throw new Error("the canvas binding was given no replaceCanvas option");
      const next = await options.replaceCanvas();
      return next.transferControlToOffscreen();
    },
    dispose() {
      if (disposed) return;
      disposed = true;
      visibility.removeEventListener("visibilitychange", onVisibility);
    },
  };
}

/** Props of {@link Alpha2Canvas}. */
export interface Alpha2CanvasProps {
  /**
   * Makes the player for a canvas. Called each time the component makes a canvas (React may run an
   * effect more than once, as `<StrictMode>` and `<Activity>` do); what it returns is disposed when
   * that canvas goes. A new function makes a new player, so keep it stable (`useCallback`). Pass
   * `replaceSurface` to `Alpha2Player` so GPU-loss recovery can swap in a new canvas element
   * without making the player again.
   */
  readonly createPlayer: (
    replaceSurface: () => Promise<OffscreenCanvas>,
  ) => CanvasPlayer;
  /** The timeline time to show, in ticks. */
  readonly timeTicks: bigint;
  /** Project size: what the player draws at, and the element's aspect ratio. */
  readonly width: number;
  readonly height: number;
  readonly className?: string;
  readonly style?: CSSProperties;
  /** Called once the player has its canvas. */
  readonly onReady?: () => void;
  /** Called with each presentation that was not superseded. */
  readonly onPresented?: (frame: PresentedFrame) => void;
  /** Called when initialization or a presentation fails; defaults to the page's `reportError`. */
  readonly onError?: (error: unknown) => void;
}

/**
 * A canvas showing `timeTicks`, drawn by a player from `createPlayer`. Each mount, and each new
 * `createPlayer` or size, makes a new canvas and player, since a player takes over its canvas.
 */
export function Alpha2Canvas(props: Alpha2CanvasProps): ReactElement {
  const container = useRef<HTMLDivElement>(null);
  const binding = useRef<CanvasBinding | undefined>(undefined);
  // Callbacks change between renders; the binding reads the latest.
  const callbacks = useRef(props);
  callbacks.current = props;
  const { createPlayer, width, height } = props;

  useEffect(() => {
    const host = container.current;
    if (host === null) return undefined;
    const makeCanvas = (): HTMLCanvasElement => {
      const made = document.createElement("canvas");
      made.width = width;
      made.height = height;
      made.style.width = "100%";
      made.style.height = "100%";
      made.style.display = "block";
      return made;
    };
    let canvas = makeCanvas();
    host.appendChild(canvas);
    // The player asks for a surface only after the binding exists.
    let bound: CanvasBinding | undefined;
    const player = createPlayer(() => {
      if (bound === undefined)
        return Promise.reject(new Error("the canvas is not bound yet"));
      return bound.replaceSurface();
    });
    bound = bindCanvas(player, canvas, {
      onPresented: (frame) => callbacks.current.onPresented?.(frame),
      onError: (error) => (callbacks.current.onError ?? reportUnhandled)(error),
      replaceCanvas: () => {
        const next = makeCanvas();
        canvas.replaceWith(next);
        canvas = next;
        return Promise.resolve(next);
      },
    });
    const current = bound;
    binding.current = current;
    current.show(callbacks.current.timeTicks);
    // A failure has gone to onError already.
    current.ready.then(
      () => {
        if (binding.current === current) callbacks.current.onReady?.();
      },
      () => {},
    );
    return () => {
      current.dispose();
      if (binding.current === current) binding.current = undefined;
      player.dispose();
      canvas.remove();
    };
  }, [createPlayer, width, height]);

  useEffect(() => {
    binding.current?.show(props.timeTicks);
  }, [props.timeTicks]);

  return createElement("div", {
    ref: container,
    className: props.className,
    style: {
      aspectRatio: `${width} / ${height}`,
      width: "100%",
      ...props.style,
    },
  });
}
