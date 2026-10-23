// Generated sources: pictures the engine draws itself (`kind: "generated"`). Positions are
// normalized ((0, 0) top-left, (1, 1) bottom-right); radii and strokes are fractions of the shorter
// side.

import { rgba } from "./color.js";

export const GENERATOR_KINDS = [
  { value: "solid", label: "Solid color" },
  { value: "linear", label: "Linear gradient" },
  { value: "radial", label: "Radial gradient" },
  { value: "conic", label: "Conic gradient" },
  { value: "rectangle", label: "Rectangle" },
  { value: "ellipse", label: "Ellipse" },
];

export const isShape = (kind) => kind === "rectangle" || kind === "ellipse";
export const isGradient = (kind) =>
  kind === "linear" || kind === "radial" || kind === "conic";

export const generatorLabel = (kind) =>
  GENERATOR_KINDS.find((item) => item.value === kind)?.label ?? "Generated";

export function defaultGenerator(kind = "linear") {
  return {
    kind,
    colors: ["#6d78ff", "#12b5a4"],
    // Degrees clockwise from up, as CSS gradients measure them.
    angle: 135,
    radius: 0.9,
    size: { x: 0.6, y: 0.35 },
    cornerRadius: 0.04,
    rotation: 0,
    fill: true,
    stroke: { enabled: false, color: "#ffffff", width: 0.012 },
  };
}

/** The `alpha2_model::Generator` for an editor description. */
export function encodeGenerator(generator) {
  const [first, second] = generator.colors;
  const stops = [
    { offset: 0, color: rgba(first) },
    { offset: 1, color: rgba(second) },
  ];
  const center = { x: 0.5, y: 0.5 };
  switch (generator.kind) {
    case "solid":
      return { type: "fill", paint: { type: "solid", color: rgba(first) } };
    case "linear": {
      const radians = (generator.angle * Math.PI) / 180;
      const dx = 0.5 * Math.sin(radians);
      const dy = -0.5 * Math.cos(radians);
      return {
        type: "fill",
        paint: {
          type: "linear_gradient",
          start: { x: 0.5 - dx, y: 0.5 - dy },
          end: { x: 0.5 + dx, y: 0.5 + dy },
          stops,
        },
      };
    }
    case "radial":
      return {
        type: "fill",
        paint: {
          type: "radial_gradient",
          center,
          radius: Math.max(0.01, generator.radius),
          stops,
        },
      };
    case "conic":
      return {
        type: "fill",
        paint: {
          type: "conic_gradient",
          center,
          angle_degrees: generator.angle,
          stops,
        },
      };
    default: {
      const stroked = generator.stroke.enabled;
      const filled = generator.fill || !stroked;
      const constant = (value) => ({ mode: "constant", value });
      const geometry = {
        type: generator.kind === "rectangle" ? "rectangle" : "ellipse",
        center: constant(center),
        size: constant({
          x: Math.max(0, generator.size.x),
          y: Math.max(0, generator.size.y),
        }),
        ...(generator.kind === "rectangle"
          ? { corner_radius: constant(Math.max(0, generator.cornerRadius)) }
          : {}),
        rotation_degrees: constant(generator.rotation),
      };
      // The stroke is listed first, so it is drawn over the fill.
      return {
        type: "shapes",
        items: [
          geometry,
          ...(stroked
            ? [
                {
                  type: "stroke",
                  width: constant(Math.max(0.001, generator.stroke.width)),
                  paint: constant({
                    type: "solid",
                    color: rgba(generator.stroke.color),
                  }),
                },
              ]
            : []),
          ...(filled
            ? [
                {
                  type: "fill",
                  paint: constant({ type: "solid", color: rgba(first) }),
                },
              ]
            : []),
        ],
      };
    }
  }
}

/**
 * Draws an approximation of the generator for a library thumbnail. Canvas gradients interpolate in
 * sRGB where the engine uses linear light, so the middle of a gradient is a little darker here than
 * in the preview.
 */
export function drawGenerator(context, generator, width, height) {
  const [first, second] = generator.colors;
  const short = Math.min(width, height);
  const gradient = (paint) => {
    paint.addColorStop(0, first);
    paint.addColorStop(1, second);
    return paint;
  };
  context.clearRect(0, 0, width, height);
  switch (generator.kind) {
    case "solid":
      context.fillStyle = first;
      context.fillRect(0, 0, width, height);
      return;
    case "linear": {
      const radians = (generator.angle * Math.PI) / 180;
      const dx = 0.5 * Math.sin(radians) * width;
      const dy = -0.5 * Math.cos(radians) * height;
      context.fillStyle = gradient(
        context.createLinearGradient(
          width / 2 - dx,
          height / 2 - dy,
          width / 2 + dx,
          height / 2 + dy,
        ),
      );
      context.fillRect(0, 0, width, height);
      return;
    }
    case "radial":
      context.fillStyle = gradient(
        context.createRadialGradient(
          width / 2,
          height / 2,
          0,
          width / 2,
          height / 2,
          generator.radius * short,
        ),
      );
      context.fillRect(0, 0, width, height);
      return;
    case "conic":
      // Canvas measures from the +x axis; the engine, like CSS, from up.
      context.fillStyle = gradient(
        context.createConicGradient(
          ((generator.angle - 90) * Math.PI) / 180,
          width / 2,
          height / 2,
        ),
      );
      context.fillRect(0, 0, width, height);
      return;
    default: {
      const w = generator.size.x * width;
      const h = generator.size.y * height;
      context.save();
      context.translate(width / 2, height / 2);
      context.rotate((generator.rotation * Math.PI) / 180);
      context.beginPath();
      if (generator.kind === "rectangle")
        context.roundRect(
          -w / 2,
          -h / 2,
          w,
          h,
          Math.min(generator.cornerRadius * short, w / 2, h / 2),
        );
      else context.ellipse(0, 0, w / 2, h / 2, 0, 0, Math.PI * 2);
      if (generator.fill || !generator.stroke.enabled) {
        context.fillStyle = first;
        context.fill();
      }
      if (generator.stroke.enabled) {
        context.strokeStyle = generator.stroke.color;
        context.lineWidth = Math.max(1, generator.stroke.width * short);
        context.stroke();
      }
      context.restore();
    }
  }
}

/** Checks a generator read from a project file. */
export function isGenerator(value) {
  return (
    value !== null &&
    typeof value === "object" &&
    GENERATOR_KINDS.some((item) => item.value === value.kind) &&
    Array.isArray(value.colors) &&
    value.colors.length === 2 &&
    [value.angle, value.radius, value.cornerRadius, value.rotation].every(
      Number.isFinite,
    ) &&
    typeof value.size === "object" &&
    typeof value.stroke === "object"
  );
}
