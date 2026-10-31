# @alpha2-sh/react

A React canvas for the [`alpha2`](https://github.com/alpha2-sh/alpha2/blob/main/packages/alpha2/README.md) browser player.

## Install

```sh
npm install @alpha2-sh/react alpha2
```

`alpha2` (at the same version as this package) and React 18 or later are peer dependencies.

## `Alpha2Canvas`

```jsx
import { useCallback } from "react";
import { Alpha2Canvas } from "@alpha2-sh/react";
import { Alpha2Player } from "alpha2/player";
import { WorkerRenderBackend } from "alpha2/worker";

function Preview({ workerUrl, refill, time }) {
  // A new function makes a new player: keep it stable.
  const createPlayer = useCallback(
    (replaceSurface) =>
      new Alpha2Player(new WorkerRenderBackend(new Worker(workerUrl, { type: "module" })), {
        onRefill: refill,
        replaceSurface,
      }),
    [workerUrl, refill],
  );
  return <Alpha2Canvas createPlayer={createPlayer} timeTicks={time} width={1280} height={720} />;
}
```

`createPlayer` makes an `Alpha2Player` from `alpha2/player`, here over a render worker (`alpha2/worker`), and `refill` decodes and uploads the frames a presentation lacks. `replaceSurface` lets the player recover from a lost GPU onto a new canvas (a lost WebGL2 context, or the CPU renderer after the browser withdrew GPU access): the component swaps a new canvas element in for the old one, and the same player draws on it. The component makes a canvas of `width` x `height`, the project's size, and hands it to a new player; it presents `timeTicks` whenever it changes, and disposes the player when the canvas goes. A player belongs to one canvas: React may set the component up more than once (`<StrictMode>`, `<Activity>`), and each time gets a new canvas and a new player.

The element keeps the project's aspect ratio and fills its container's width. While the page is hidden it presents nothing, and it shows the latest time when the page is visible again. Failures go to `onError`, or to the page's `reportError` without it; `onReady` and `onPresented` report the player's start and each presentation.

## `bindCanvas`

`bindCanvas(player, canvas, options)` is the same wiring without React: it transfers the canvas to the player and presents the times it is shown (`binding.show(ticks)`). It neither makes nor disposes the player. Set the canvas's `width` and `height` to the project's size before binding it. With `options.replaceCanvas` (put a new canvas of the same size where the old one is, and resolve with it), `binding.replaceSurface()` is what the player's `replaceSurface` option needs.

## License

MIT, copyright Alpha2 contributors.
