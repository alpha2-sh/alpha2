// Compile-time only; `npm run typecheck` fails when a `Canonical<T>` mapping changes.
import type {
  Canonical,
  CanonicalInsets,
  CanonicalOffset,
  CanonicalPaint,
  Insets,
  LayoutOffset,
  MergeLayer,
  Paint,
  Rgba,
  RgbaObject,
  VisualProperties,
} from "../src/project.js";

type Equal<A, B> =
  (<G>() => G extends A ? 1 : 2) extends <G>() => G extends B ? 1 : 2
    ? true
    : false;
type Expect<T extends true> = T;

export type Checks = [
  Expect<
    Equal<keyof Canonical<MergeLayer>, "blend_mode" | "input" | "opacity">
  >,
  Expect<Equal<Canonical<Rgba>, RgbaObject>>,
  Expect<Equal<Canonical<Rgba | null>, RgbaObject | null>>,
  Expect<Equal<Canonical<Paint>, CanonicalPaint>>,
  Expect<Equal<Canonical<Insets>, CanonicalInsets>>,
  Expect<Equal<Canonical<LayoutOffset>, CanonicalOffset>>,
  Expect<
    Equal<keyof Canonical<VisualProperties>, keyof Required<VisualProperties>>
  >,
];
