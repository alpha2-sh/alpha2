"""The project builder and the engine's validator, in-process."""

from __future__ import annotations

import pytest
from conftest import corpus, expand_repeats

import alpha2
from alpha2 import ProjectBuilder, _native, sequential_ids, ticks, to_ticks, validate_project


def test_the_builder_writes_the_minimal_one_second_tutorial_project() -> None:
    expected = corpus("builder-tutorial-project.json")
    builder = ProjectBuilder(
        "Tutorial project", id="tutorial-project", width=1280, height=720, frame_rate=30
    )
    image = builder.source("image", "asset://image-1", id="image-1")
    builder.video_track(id="video-1", name="Main video").clip(image, at=0, duration=1)
    built = builder.build()
    # The builder names its generated clip `clip-1`, as the document does.
    assert built == expected
    assert validate_project(built) == []


def test_assets_maps_each_media_source_the_builder_added_to_its_locator() -> None:
    builder = ProjectBuilder("Assets", width=640, height=360, frame_rate=30)
    image = builder.source("image", "title.png")
    music = builder.source("audio", "/media/music.m4a", id="music")
    builder.generated(
        {
            "type": "fill",
            "paint": {"type": "solid", "color": {"red": 0, "green": 0, "blue": 0, "alpha": 1}},
        }
    )
    assert builder.assets() == {image: "title.png", music: "/media/music.m4a"}


def test_seconds_round_to_ticks_ticks_are_exact_and_ids_are_deterministic() -> None:
    assert to_ticks(1 / 3) == 40_000
    assert to_ticks(ticks(40_001)) == 40_001
    assert to_ticks(-0.5 / 120_000) == -1
    assert alpha2.seconds_to_ticks(alpha2.ticks_to_seconds(120_120)) == 120_120
    with pytest.raises(ValueError):
        ticks(0.5)  # type: ignore[arg-type]
    with pytest.raises(ValueError):
        to_ticks(float("inf"))
    ids = sequential_ids()
    assert [ids("clip"), ids("track"), ids("clip")] == ["clip-1", "track-1", "clip-2"]


def test_the_builder_covers_generated_sources_transitions_audio_and_captions() -> None:
    builder = ProjectBuilder("Everything", width=640, height=360, frame_rate=(30_000, 1_001))
    backdrop = builder.generated(
        {
            "type": "fill",
            "paint": {
                "type": "solid",
                "color": {"red": 0.1, "green": 0.1, "blue": 0.2, "alpha": 1},
            },
        }
    )
    video = builder.source("video", "asset://clip.mp4", has_audio=True)
    music = builder.source("audio", "asset://music.m4a")
    track = builder.video_track()
    first = track.clip(backdrop, at=0, duration=2)
    second = track.clip(
        video, at=1.5, duration=3, source_in=10, source_duration=6, blend_mode="screen"
    )
    track.transition(first, second, {"id": "cross_dissolve", "version": 1, "params": {}})
    builder.audio_track().clip(music, at=0, duration=ticks(540_000))
    builder.caption_track(style={"font_size": 0.06}).cue("Second", at=2, duration=1).cue(
        "First", at=0, duration=2
    )
    project = builder.build()
    assert validate_project(project) == []
    video_track, audio_track, captions = project["timeline"]["tracks"]
    assert video_track["transitions"][0]["timeline_range"] == {"start": 180_000, "end": 240_000}
    # Twice as much source as timeline: double speed.
    assert video_track["clips"][1]["source_range"] == {"start": 1_200_000, "end": 1_920_000}
    assert video_track["clips"][1]["blend_mode"] == "screen"
    assert audio_track["kind"] == "audio"
    assert [cue["text"] for cue in captions["captions"]["cues"]] == ["First", "Second"]


def test_an_invalid_document_is_refused_with_the_engines_own_words() -> None:
    builder = ProjectBuilder("Broken", width=64, height=64, frame_rate=30)
    image = builder.source("image", "asset://a.png")
    track = builder.video_track()
    track.clip(image, at=0, duration=2)
    track.clip("missing", at=5, duration=1)
    with pytest.raises(ValueError, match="missing"):
        builder.build()


def test_clips_show_compositions_an_effect_chain_or_a_graph_added_by_id() -> None:
    builder = ProjectBuilder("Looks", width=64, height=64, frame_rate=30)
    image = builder.source("image", "asset://a.png")
    other = builder.source("image", "asset://b.png")
    blur = {"id": "alpha2.effect.gaussian-blur", "version": 1}
    invert = {"id": "alpha2.effect.invert", "version": 1}
    graph = builder.composition(
        [
            {"id": "clip", "op": {"type": "clip"}},
            {"id": "b", "op": {"type": "media", "source_id": other}},
            {
                "id": "out",
                "op": {
                    "type": "merge",
                    "layers": [{"input": "clip"}, {"input": "b", "blend_mode": "multiply"}],
                },
            },
        ],
        "out",
    )
    track = builder.video_track()
    track.clip(image, at=0, duration=1, effects=[blur, invert])
    track.clip(image, at=1, duration=1, composition={"id": graph})
    project = builder.build()
    assert validate_project(project) == []
    chain, merged = project["timeline"]["tracks"][0]["clips"]
    assert merged["composition"]["id"] == graph
    composition = next(c for c in project["compositions"] if c["id"] == chain["composition"]["id"])
    assert [(node["id"], node["op"].get("input")) for node in composition["nodes"]] == [
        ("clip", None),
        ("effect-0", "clip"),
        ("effect-1", "effect-0"),
    ]
    assert composition["output"] == "effect-1"
    assert composition["nodes"][2]["op"]["effect"] == invert


def test_generated_ids_skip_the_ids_already_given_explicitly() -> None:
    builder = ProjectBuilder("Mixed ids", width=64, height=64, frame_rate=30)
    image = builder.source("image", "asset://a.png", id="source-1")
    other = builder.source("image", "asset://b.png")
    assert other == "source-2"
    graph = builder.composition(
        [{"id": "clip", "op": {"type": "clip"}}], "clip", id="composition-1"
    )
    builder.video_track(id="track-2")
    track = builder.video_track()
    assert track.id == "track-1"
    assert builder.caption_track().id == "track-3"
    ids = [
        track.clip(image, id="clip-2", at=0, duration=1),
        track.clip(image, at=1, duration=1),
        track.clip(image, at=2, duration=1),
        # Its effect chain is a composition the builder names, past the explicit `composition-1`.
        track.clip(other, at=3, duration=1, effects=[{"id": "alpha2.effect.invert", "version": 1}]),
        track.clip(other, at=4, duration=1, composition={"id": graph}),
    ]
    assert ids == ["clip-2", "clip-1", "clip-3", "clip-4", "clip-5"]
    project = builder.build()
    assert [c["id"] for c in project["compositions"]] == ["composition-1", "composition-2"]
    assert project["timeline"]["tracks"][1]["clips"][3]["composition"]["id"] == "composition-2"


def test_a_clip_id_is_unique_across_tracks_and_a_generator_that_only_repeats_is_refused() -> None:
    builder = ProjectBuilder(
        "Ids", width=64, height=64, frame_rate=30, ids=lambda kind: f"{kind}-only"
    )
    image = builder.source("image", "asset://a.png")
    builder.video_track(id="a").clip(image, at=0, duration=1)
    second = builder.video_track(id="b")
    with pytest.raises(ValueError, match="clip-only"):
        second.clip(image, at=0, duration=1)
    with pytest.raises(ValueError):
        builder.source("image", "asset://b.png")
    # An explicit id is not checked until build, which reports the repeat.
    second.clip(image, id="clip-only", at=1, duration=1)
    with pytest.raises(ValueError, match="clip-only"):
        builder.build()


def test_effects_and_a_composition_together_are_refused_and_add_nothing() -> None:
    builder = ProjectBuilder("Both", width=64, height=64, frame_rate=30)
    image = builder.source("image", "asset://a.png")
    graph = builder.composition([{"id": "clip", "op": {"type": "clip"}}], "clip")
    track = builder.video_track()
    with pytest.raises(ValueError, match=r"effects.*composition"):
        track.clip(
            image,
            at=0,
            duration=1,
            effects=[{"id": "alpha2.effect.invert", "version": 1}],
            composition={"id": graph, "offset": 5},
        )
    with pytest.raises(ValueError, match="source_range"):
        track.clip(image, at=0, duration=1, source_range={"start": 0, "end": 1})
    project = builder.build()
    assert project["timeline"]["tracks"][0]["clips"] == []
    assert [c["id"] for c in project["compositions"]] == [graph]


def test_validation_is_the_engines_and_names_what_it_refuses() -> None:
    project = corpus("builder-tutorial-project.json")
    assert validate_project(project) == []
    assert validate_project({**project, "schema_version": 999}) != []
    [problem] = validate_project({**project, "settings": {**project["settings"], "width": 0}})
    assert problem == "invalid project: canvas dimensions must be positive"
    assert validate_project({**project, "name": float("nan")})[0].startswith(
        "project cannot be written as JSON"
    )
    # The shared corpus: each case is valid or not as it says, and a case the model rejects names
    # the engine's error, which is the problem reported here.
    # A case is a document, or (`json`) the text of one, for what only text can say.
    for case in expand_repeats(corpus("project-validation-corpus.json")):
        problems = (
            _native.validate_project_json(case["json"])
            if "json" in case
            else validate_project(case["project"])
        )
        assert (problems == []) == case["valid"], case["name"]
        if "error" in case:
            assert case["error"] in problems[0], (case["name"], problems)


def test_caption_cues_take_speakers_styles_and_timed_runs() -> None:
    builder = ProjectBuilder("Captions", width=640, height=360, frame_rate=(30, 1))
    track = builder.caption_track(
        style={"font_size": 0.08},
        styles={"Ann": {"fill": "#ffd040"}, "loud": {"font_size": 0.12}},
    )
    second = 120_000
    track.cue(
        [
            {"text": "Hello ", "time": {"start": 0, "end": second}},
            {"text": "there", "class": ["key"], "time": {"start": second, "end": 2 * second}},
        ],
        at=0,
        duration=2,
        speaker="Ann",
    )
    track.cue("Loud", at=2, duration=1, style="loud")
    project = builder.build()
    captions = project["timeline"]["tracks"][0]["captions"]
    assert captions["styles"]["Ann"] == {"fill": "#ffd040"}
    assert captions["cues"][0]["speaker"] == "Ann"
    assert captions["cues"][0]["text"][1]["class"] == ["key"]
    assert captions["cues"][1]["style"] == "loud"
    assert validate_project(project) == []


def test_a_faster_clip_can_keep_its_pitch() -> None:
    builder = ProjectBuilder("Speed", width=640, height=360, frame_rate=(30, 1))
    music = builder.source("audio", "music.m4a")
    clip_id = builder.audio_track().clip(
        music, at=0, duration=2, source_duration=3, preserve_pitch=True
    )
    project = builder.build()
    clip = project["timeline"]["tracks"][0]["clips"][0]
    assert clip["id"] == clip_id
    assert clip["audio"] == {"preserve_pitch": True}
    assert validate_project(project) == []
