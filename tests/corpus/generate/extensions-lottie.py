"""Writes the Lottie corpus: small hand-built animations whose pixels can be
checked from their geometry alone, a feature report fixture, a refusal
fixture, and a dotLottie package of two animations. Deterministic."""

import json
import zipfile

LINEAR_IN = {"x": [1], "y": [1]}
LINEAR_OUT = {"x": [0], "y": [0]}


def static(value):
    return {"a": 0, "k": value}


def transform(position, opacity=100):
    return {
        "o": static(opacity),
        "r": static(0),
        "p": position,
        "a": static([0, 0, 0]),
        "s": static([100, 100, 100]),
    }


def box_layer(name, position, color, size=16, index=1):
    return {
        "ddd": 0,
        "ind": index,
        "ty": 4,
        "nm": name,
        "sr": 1,
        "ks": transform(position),
        "ao": 0,
        "shapes": [
            {
                "ty": "gr",
                "nm": "group",
                "it": [
                    {"ty": "rc", "nm": "rect", "d": 1, "s": static([size, size]), "p": static([0, 0]), "r": static(0)},
                    {"ty": "fl", "nm": "fill", "c": static(color), "o": static(100), "r": 1},
                    {
                        "ty": "tr",
                        "p": static([0, 0]),
                        "a": static([0, 0]),
                        "s": static([100, 100]),
                        "r": static(0),
                        "o": static(100),
                    },
                ],
            }
        ],
        "ip": 0,
        "op": 10,
        "st": 0,
        "bm": 0,
    }


def animation(name, layers, rate=10, frames=10, width=64, height=48):
    return {
        "v": "5.7.4",
        "fr": rate,
        "ip": 0,
        "op": frames,
        "w": width,
        "h": height,
        "nm": name,
        "ddd": 0,
        "assets": [],
        "layers": layers,
    }


# A red 16x16 box moving from x = 16 to x = 48 over one second at 10 fps.
moving = {
    "a": 1,
    "k": [
        {"t": 0, "s": [16, 24, 0], "i": LINEAR_IN, "o": LINEAR_OUT},
        {"t": 10, "s": [48, 24, 0]},
    ],
}
shapes = animation("moving box", [box_layer("box", moving, [1, 0, 0, 1])])

# What the backend leaves out: a text layer and a layer effect, reported with where they are.
text_layer = {
    "ddd": 0, "ind": 2, "ty": 5, "nm": "title", "sr": 1, "ks": transform(static([32, 24, 0])), "ao": 0,
    "t": {"d": {"k": [{"s": {"s": 12, "f": "Sans", "t": "Hi", "j": 0, "tr": 0, "lh": 14, "ls": 0, "fc": [1, 1, 1]}, "t": 0}]}},
    "ip": 0, "op": 10, "st": 0, "bm": 0,
}
effected = box_layer("glow", static([32, 24, 0]), [0, 0, 1, 1], index=3)
effected["ef"] = [{"ty": 29, "nm": "Gaussian Blur", "ef": []}]
unsupported = animation("unsupported", [text_layer, effected])

# What would stop the backend: a group's split position, refused before it is read. (A layer's or a
# repeater's split position, velato reads.)
split = animation("split", [box_layer("split", static([16, 24, 0]), [0, 1, 0, 1])])
split["layers"][0]["shapes"][0]["it"][2]["p"] = {"s": True, "x": static(0), "y": static(0)}

# A dotLottie package (version 2) of two animations: the moving box and a still blue box.
still = animation("still box", [box_layer("still", static([32, 24, 0]), [0, 0, 1, 1])])


# What velato would panic on, recurse into without end, or expand without bound, and what the
# provider rewrites so it can read it (lottie.rs normalize). Each case says what opening it does:
# "opens" (and frame 0 shows red at the canvas centre), or the error kind and a part of its message.
def red_box(transform_override=None, shapes_override=None, index=1, **fields):
    layer = box_layer("box", static([32, 24, 0]), [1, 0, 0, 1], index=index)
    if transform_override is not None:
        layer["ks"] = transform_override
    if shapes_override is not None:
        layer["shapes"] = shapes_override
    layer.update(fields)
    return layer


def keyframes(*values, easing=None):
    out = []
    for index, value in enumerate(values):
        frame = {"t": index * 10, "s": value}
        if easing is not None and index + 1 < len(values):
            frame.update(easing)
        out.append(frame)
    return {"a": 1, "k": out}


def precomposition_layer(ref, index=1):
    return {"ddd": 0, "ind": index, "ty": 0, "nm": ref, "refId": ref, "ks": transform(static([0, 0, 0])),
            "w": 64, "h": 48, "ip": 0, "op": 10, "st": 0}


def group(*items):
    return {"ty": "gr", "nm": "group", "it": list(items)}


RECT = {"ty": "rc", "nm": "rect", "d": 1, "s": static([16, 16]), "p": static([0, 0]), "r": static(0)}
FILL = {"ty": "fl", "nm": "fill", "c": static([1, 0, 0, 1]), "o": static(100), "r": 1}
SQUARE = {"c": True, "v": [[-8, -8], [8, -8], [8, 8], [-8, 8]], "i": [[0, 0]] * 4, "o": [[0, 0]] * 4}
# The whole 64x48 canvas, from a layer at its centre; a 1x1 dot; a gradient across the canvas.
CANVAS = {"ty": "rc", "nm": "canvas", "d": 1, "s": static([64, 48]), "p": static([0, 0]), "r": static(0)}
DOT = {"ty": "rc", "nm": "dot", "d": 1, "s": static([1, 1]), "p": static([0, 0]), "r": static(0)}
GRADIENT = {"ty": "gf", "nm": "gradient", "o": static(100), "r": 1, "t": 1, "s": static([-32, -24]), "e": static([32, 24]),
            "g": {"p": 2, "k": static([0, 1, 0, 0, 1, 1, 0, 0])}}
TWO_VALUE_EASING = {"i": {"x": [0.5, 0.5], "y": [1, 1]}, "o": {"x": [0.5, 0.5], "y": [0, 0]}}


def repeater(copies, end_opacity=100):
    return {"ty": "rp", "nm": "repeat", "c": copies, "o": static(0), "m": 1,
            "tr": {"ty": "tr", "p": static([0, 0]), "a": static([0, 0]), "s": static([100, 100]), "r": static(0),
                   "so": static(100), "eo": static(end_opacity)}}


def group_transform(scale):
    return {"ty": "tr", "p": static([0, 0]), "a": static([0, 0]), "s": static(scale), "r": static(0), "o": static(100)}


def layer_transform(**fields):
    ks = transform(static([32, 24, 0]))
    ks.update(fields)
    return {key: value for key, value in ks.items() if value is not None}


def nested_precompositions(depth, instances):
    assets = []
    for level in range(depth):
        if level + 1 < depth:
            layers = [precomposition_layer(f"p{level + 1}", index) for index in range(1, instances + 1)]
        else:
            layers = [red_box()]
        assets.append({"id": f"p{level}", "layers": layers})
    document = animation("nested", [precomposition_layer("p0")])
    document["assets"] = assets
    return document


def with_assets(layers, assets):
    document = animation("precompositions", layers)
    document["assets"] = assets
    return document


leftovers = group(RECT, {"ty": "zz", "nm": "zig", "r": static(4), "s": static(2), "pt": static(1)}, {"ty": "no", "nm": "none"}, FILL)
cases = [
    {"name": "a layer transform without a rotation is drawn unrotated", "outcome": "opens",
     "animation": animation("no rotation", [red_box(layer_transform(r=None))])},
    {"name": "a 3D rotation without an anchor is refused", "outcome": "unsupported", "message": "3D rotations",
     "animation": animation("3d", [red_box(layer_transform(r=None, a=None, rx=static(0), ry=static(0), rz=static(0), **{"or": static([0, 0, 0])}))])},
    {"name": "a split rotation is refused", "outcome": "unsupported", "message": "split rotations",
     "animation": animation("split rotation", [red_box(layer_transform(r={"x": static(0), "y": static(0), "z": static(0), "or": static([0, 0, 0])}))])},
    {"name": "a group's split rotation is refused", "outcome": "unsupported", "message": "split rotations",
     "animation": animation("group split rotation", [red_box(shapes_override=[group(RECT, FILL, {"ty": "tr", "p": static([0, 0]), "a": static([0, 0]), "s": static([100, 100]), "o": static(100), "r": {"x": static(0), "y": static(0), "z": static(0), "or": static([0, 0, 0])}})])])},
    {"name": "a layer's split position is drawn", "outcome": "opens",
     "animation": animation("layer split position", [red_box(layer_transform(p={"s": True, "x": keyframes([32], [32], easing=TWO_VALUE_EASING), "y": static(24)}))])},
    {"name": "a repeater's split position is drawn", "outcome": "opens",
     "animation": animation("repeater split position", [red_box(shapes_override=[group(RECT, FILL, dict(repeater(static(3)), tr=dict(repeater(static(3))["tr"], p={"s": True, "x": static(0), "y": static(0)})))])])},
    {"name": "a group's split position is refused", "outcome": "unsupported", "message": "split positions",
     "animation": animation("group split position", [red_box(shapes_override=[group(RECT, FILL, {"ty": "tr", "p": {"s": True, "x": static(0), "y": static(0)}, "a": static([0, 0]), "s": static([100, 100]), "o": static(100), "r": static(0)})])])},
    {"name": "a scalar easing handle of two values keeps its first", "outcome": "opens",
     "animation": animation("two-value easing", [red_box(layer_transform(o=keyframes([100], [100], easing=TWO_VALUE_EASING)))])},
    {"name": "a shape path's easing handle of two values keeps its first", "outcome": "opens",
     "animation": animation("path easing", [red_box(shapes_override=[group({"ty": "sh", "nm": "path", "ks": keyframes([SQUARE], [SQUARE], easing=TWO_VALUE_EASING)}, FILL)])])},
    {"name": "an easing handle without a value is refused", "outcome": "unsupported", "message": "easing handles without a value",
     "animation": animation("empty easing", [red_box(layer_transform(r=keyframes([0], [0], easing={"i": {"x": [], "y": [1]}, "o": {"x": [0], "y": [0]}})))])},
    {"name": "a keyframe without a value is refused", "outcome": "unsupported", "message": "keyframes without a value",
     "animation": animation("empty keyframe", [red_box(layer_transform(r=keyframes([], [0])))])},
    {"name": "layers and items the backend does not draw are left out and reported", "outcome": "opens",
     "diagnostics": [["audio layers", "/layers/1", 1], ["camera layers", "/layers/0", 1], ["text layers", "/layers/2", 1], ["zig zag", "/layers/3/shapes/0/it/1", 1]],
     "animation": animation("left out", [
         {"ddd": 0, "ind": 5, "ty": 13, "nm": "camera", "ks": transform(static([0, 0, 0])), "ip": 0, "op": 10, "st": 0, "pe": static(600)},
         {"ddd": 0, "ind": 4, "ty": 6, "nm": "audio", "ks": transform(static([0, 0, 0])), "ip": 0, "op": 10, "st": 0, "au": {"lv": static([0, 0])}},
         dict(text_layer, ind=3),
         red_box(shapes_override=[leftovers], parent=5),
     ])},
    {"name": "blend modes, luma mattes, and even-odd fills velato draws otherwise are reported", "outcome": "opens",
     "diagnostics": [["even-odd fills", "/layers/1/shapes/0/it/1", 1], ["layer blend modes", "/layers/1", 1], ["luma mattes", "/layers/1", 1]],
     "animation": animation("drawn otherwise", [
         red_box(index=2, td=1),
         red_box(index=1, tt=3, bm=2, shapes_override=[group(RECT, dict(FILL, r=2))]),
     ])},
    {"name": "a solid layer is drawn as a rectangle of its color", "outcome": "opens",
     "animation": animation("solid", [{"ddd": 0, "ind": 1, "ty": 1, "nm": "solid", "sr": 1, "ks": transform(static([0, 0, 0])), "ao": 0,
                                       "sw": 64, "sh": 48, "sc": "#ff0000", "ip": 0, "op": 10, "st": 0, "bm": 0}])},
    {"name": "a layer matted by itself is refused", "outcome": "malformed", "message": "mattes itself",
     "animation": animation("self matte", [red_box(tt=1, tp=1)])},
    {"name": "two mattes matted by each other are refused", "outcome": "malformed", "message": "mattes itself",
     "animation": animation("matte cycle", [red_box(index=1, td=1, tt=1, tp=2), red_box(index=2, td=1, tt=1, tp=1), red_box(index=3, tt=1, tp=1)])},
    {"name": "a precomposition that draws itself is refused", "outcome": "malformed", "message": "draws itself",
     "animation": with_assets([precomposition_layer("a")], [{"id": "a", "layers": [precomposition_layer("a")]}])},
    {"name": "precompositions that draw each other are refused", "outcome": "malformed", "message": "draws itself",
     "animation": with_assets([precomposition_layer("a")], [{"id": "a", "layers": [precomposition_layer("b")]}, {"id": "b", "layers": [precomposition_layer("a")]}])},
    {"name": "precompositions within the budget are drawn", "outcome": "opens", "animation": nested_precompositions(3, 4)},
    {"name": "precompositions that fan out past the budget are refused", "outcome": "budget", "message": "draws more than",
     "animation": nested_precompositions(9, 10)},
    {"name": "precompositions nested past the budget are refused", "outcome": "budget", "message": "nest more than",
     "animation": nested_precompositions(70, 1)},
    {"name": "masks held past the nesting budget are refused", "outcome": "budget", "message": "nest more than",
     "animation": animation("masks", [red_box(masksProperties=[{"mode": "a", "inv": False, "pt": static(SQUARE), "o": static(100), "x": static(0)}] * 70)])},
    {"name": "a repeater within the budget is drawn", "outcome": "opens",
     "animation": animation("repeated", [red_box(shapes_override=[group(RECT, FILL, repeater(static(10)))])])},
    {"name": "a repeater of too many copies is refused", "outcome": "budget", "message": "repeater makes more than",
     "animation": animation("repeater", [red_box(shapes_override=[group(RECT, FILL, repeater(static(1000000000)))])])},
    {"name": "a keyframed repeater easing past the budget is refused", "outcome": "budget", "message": "repeater makes more than",
     "animation": animation("eased repeater", [red_box(shapes_override=[group(RECT, FILL, repeater(keyframes([1], [2], easing={"i": {"x": [0.5], "y": [1]}, "o": {"x": [0.5], "y": [1000000]}})))])])},
    {"name": "nested repeaters that multiply past the budget are refused", "outcome": "budget", "message": "draws more than",
     "animation": animation("nested repeaters", [red_box(shapes_override=[group(group(group(RECT, FILL, repeater(static(100))), repeater(static(100))), repeater(static(100)))])])},
    {"name": "a fading repeater within the budget is drawn", "outcome": "opens",
     "animation": animation("fading", [red_box(shapes_override=[group(RECT, FILL, repeater(static(10), end_opacity=100)), group(RECT, FILL, repeater(static(10), end_opacity=50))])])},
    {"name": "a fading repeater past the offscreen budget is refused", "outcome": "budget", "message": "offscreen layers",
     "animation": animation("fading too many", [red_box(shapes_override=[group(RECT, FILL, repeater(static(600), end_opacity=50))])])},
    {"name": "a polystar of too many points is refused", "outcome": "budget", "message": "polystar has more than",
     "animation": animation("polystar", [red_box(shapes_override=[group({"ty": "sr", "nm": "star", "sy": 2, "p": static([0, 0]), "or": static(8), "os": static(0), "r": static(0), "pt": static(1000000000)}, FILL)])])},
    # velato also reads a numeric shape type as an index into its own list, which the budgets above
    # would not see: what is no name is refused.
    {"name": "a repeater whose type is a number is refused", "outcome": "malformed", "message": "type is not a string",
     "animation": animation("numbered repeater", [red_box(shapes_override=[group(RECT, FILL, dict(repeater(static(1000000)), ty=7))])])},
    {"name": "a polystar whose type is a number is refused", "outcome": "malformed", "message": "type is not a string",
     "animation": animation("numbered polystar", [red_box(shapes_override=[group({"ty": 16, "nm": "star", "sy": 2, "p": static([0, 0]), "or": static(8), "os": static(0), "r": static(0), "pt": static(1000000)}, FILL)])])},
    {"name": "a group whose type is a number is refused", "outcome": "malformed", "message": "type is not a string",
     "animation": animation("numbered group", [red_box(shapes_override=[group(dict(group(group(RECT, FILL, repeater(static(1000))), repeater(static(1000))), ty=0))])])},
    {"name": "a group's split position in a transform whose type is a number is refused", "outcome": "malformed", "message": "type is not a string",
     "animation": animation("numbered transform", [red_box(shapes_override=[group(RECT, FILL, {"ty": 3, "p": {"s": True, "x": static(0), "y": static(0)}, "a": static([0, 0]), "s": static([100, 100]), "o": static(100), "r": static(0)})])])},
    {"name": "a layer's shape whose type is a number is refused", "outcome": "malformed", "message": "type is not a string",
     "animation": animation("numbered layer shape", [red_box(shapes_override=[dict(group(RECT, FILL), ty=0)])])},
    # What a frame paints: each fill or stroke covers at most the canvas, and as much as its shapes'
    # bounds, through every transform and repeater copy, say.
    {"name": "gradients over the canvas repeated past the paint budget are refused", "outcome": "budget", "message": "paints more than",
     "animation": animation("painted gradients", [red_box(shapes_override=[group(group(CANVAS, GRADIENT, repeater(static(30))), repeater(static(30)))])])},
    {"name": "fills over the canvas repeated past the paint budget are refused", "outcome": "budget", "message": "paints more than",
     "animation": animation("painted fills", [red_box(shapes_override=[group(group(CANVAS, FILL, repeater(static(450))), repeater(static(100)))])])},
    {"name": "a small shape a group scales over the canvas paints the canvas", "outcome": "budget", "message": "paints more than",
     "animation": animation("scaled group", [red_box(shapes_override=[group(group(DOT, GRADIENT, repeater(static(30)), group_transform([6400, 6400])), repeater(static(30)))])])},
    {"name": "a small shape a parent layer scales over the canvas paints the canvas", "outcome": "budget", "message": "paints more than",
     "animation": animation("scaled parent", [
         red_box(layer_transform(p=static([0, 0, 0])), index=1, parent=2, shapes_override=[group(group(DOT, GRADIENT, repeater(static(30))), repeater(static(30)))]),
         {"ddd": 0, "ind": 2, "ty": 3, "nm": "parent", "ks": layer_transform(s=static([6400, 6400, 100])), "ip": 0, "op": 10, "st": 0},
     ])},
    {"name": "a small shape a precomposition scales over the canvas paints the canvas", "outcome": "budget", "message": "paints more than",
     "animation": with_assets([dict(precomposition_layer("small"), ks=layer_transform(s=static([6400, 6400, 100])), w=1, h=1)],
                              [{"id": "small", "layers": [red_box(layer_transform(p=static([0, 0, 0])), shapes_override=[group(group(DOT, GRADIENT, repeater(static(30))), repeater(static(30)))])]}])},
    {"name": "a background past the canvas and many small shapes are drawn", "outcome": "opens",
     "animation": animation("painted within the budget", [red_box(shapes_override=[group(RECT, FILL, repeater(static(200))), group(dict(CANVAS, s=static([6400, 4800])), FILL)])])},
]

for name, document in [("moving.json", shapes), ("unsupported.json", unsupported), ("split.json", split), ("normalize-cases.json", cases)]:
    with open(name, "w") as out:
        json.dump(document, out, separators=(",", ":"))
        out.write("\n")

with zipfile.ZipFile("pair.lottie", "w", compression=zipfile.ZIP_DEFLATED) as package:
    for entry, document in [
        ("manifest.json", {"version": "2", "generator": "alpha2 corpus", "animations": [{"id": "moving"}, {"id": "still"}]}),
        ("a/moving.json", shapes),
        ("a/still.json", still),
    ]:
        info = zipfile.ZipInfo(entry, date_time=(2026, 9, 28, 0, 0, 0))
        info.compress_type = zipfile.ZIP_DEFLATED
        package.writestr(info, json.dumps(document, separators=(",", ":")))

# A package whose first animation is refused (a group's split position) and whose second opens: a
# source can choose the second, and a probe describes the package by it.
with zipfile.ZipFile("first-refused.lottie", "w", compression=zipfile.ZIP_DEFLATED) as package:
    for entry, document in [
        ("manifest.json", {"version": "2", "generator": "alpha2 corpus", "animations": [{"id": "split"}, {"id": "moving"}]}),
        ("a/split.json", split),
        ("a/moving.json", shapes),
    ]:
        info = zipfile.ZipInfo(entry, date_time=(2026, 10, 6, 0, 0, 0))
        info.compress_type = zipfile.ZIP_DEFLATED
        package.writestr(info, json.dumps(document, separators=(",", ":")))
