"""The compose layer: node constructors, compiling with given facts, gathering facts through a
host's ``conform``, and exporting a compiled spec."""

from __future__ import annotations

import json
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest
from conftest import ROOT

import alpha2
from alpha2 import Alpha2ComposeError, HostClient
from alpha2 import compose as c

FakeHost = Callable[[Path, str], str]
SECOND = 120_000
FONT = ROOT / "tests" / "fonts" / "Alpha2Fixture.ttf"

FACTS: dict[str, Any] = {
    "media": {
        "talk.mp4": {
            "width": 1920,
            "height": 1080,
            "frame_rate": {"num": 30, "den": 1},
            "duration_ticks": 12 * SECOND,
            "has_audio": True,
        },
        "photo.jpg": {"width": 1200, "height": 800},
        "music.m4a": {"duration_ticks": 40 * SECOND, "has_audio": True},
    },
    "texts": {},
}


def test_constructors_write_the_spec_as_plain_dicts() -> None:
    node = c.Media(Path("clips") / "a.mp4", loop=True, zoom=c.Ramp(1, 1.2), label=None)
    assert node == {
        "type": "media",
        "path": str(Path("clips") / "a.mp4"),
        "loop": True,
        "zoom": {"from": 1, "to": 1.2},
    }
    assert c.Group(
        [c.Text("Hi")], blend="screen", effects=[c.Effect("gaussian-blur", radius=0.1)]
    ) == {
        "type": "group",
        "children": [{"type": "text", "text": "Hi"}],
        "blend": "screen",
        "effects": [{"effect": "gaussian-blur", "radius": 0.1}],
    }
    assert c.Shape([{"shape": "ellipse"}], fill=c.ThemeColor("accent", 0.5)) == {
        "type": "shape",
        "shapes": [{"shape": "ellipse"}],
        "fill": {"$theme": "accent", "alpha": 0.5},
    }
    card = c.Box([c.Item(c.Text("Hi"), flex_grow=1)], flex_direction="column", gap="8px")
    assert card == {
        "type": "box",
        "children": [{"box": {"flex_grow": 1}, "node": {"type": "text", "text": "Hi"}}],
        "flex_direction": "column",
        "gap": "8px",
    }
    window = c.Matte(c.Media("a.mp4"), c.Text("HI"), use="luma")
    assert window["by"] == {"type": "text", "text": "HI"} and window["use"] == "luma"
    assert c.Mask(c.Media("a.mp4"), [{"shape": "ellipse"}])["shapes"] == [{"shape": "ellipse"}]
    assert c.Keys((0, 1), (2, 1.5, "ease-out")) == {
        "keys": [{"t": 0, "v": 1}, {"t": 2, "v": 1.5, "ease": "ease-out"}]
    }
    assert c.Captions("talk.srt", style="bar-bottom") == {
        "type": "captions",
        "file": "talk.srt",
        "style": "bar-bottom",
    }
    cues = [{"start": 0, "end": 1, "text": "hi"}]
    assert c.Captions(cues)["cues"] == cues
    assert c.Slideshow([Path("a.jpg")], transition=c.Crossfade(1))["transition"] == {
        "type": "crossfade",
        "duration": 1,
    }
    video = c.Video(c.Media("talk.mp4"), audio=c.MusicBed("music.m4a"), loudness=-14)
    assert video.spec["audio"] == [{"type": "music_bed", "path": "music.m4a"}]
    assert json.loads(video.to_json())["loudness"] == -14
    assert c.THEMES == ("clean-dark", "clean-light")


def test_a_spec_with_all_its_facts_compiles_without_a_host() -> None:
    video = c.Video(
        c.Sequence(
            [c.Media("talk.mp4"), c.Media("photo.jpg", duration=3)], transition=c.Crossfade(0.5)
        ),
        overlays=[c.Title("Hello", at=1, duration=2)],
        audio=[c.MusicBed("music.m4a", duck_under="body")],
        loudness={"target_lufs": -16, "true_peak_ceiling_dbtp": -2},
    )
    compiled = video.compile(facts=FACTS, fonts=[FONT])
    assert alpha2.validate_project(compiled.project) == []
    assert sorted(compiled.assets.values()) == ["music.m4a", "photo.jpg", "talk.mp4"]
    assert compiled.loudness == {"target_lufs": -16, "true_peak_ceiling_dbtp": -2}
    assert compiled.fonts == [str(FONT)]
    assert compiled.warnings == []
    tracks = compiled.project["timeline"]["tracks"]
    assert tracks[0]["transitions"][0]["timeline_range"] == {
        "start": 23 * SECOND // 2,
        "end": 12 * SECOND,
    }
    # The music ducks under the clip's sound and lasts as long as the video: 12 + 3 - 0.5 s.
    music = tracks[-1]
    assert music["audio"]["ducking"]["sidechain_track"] == tracks[0]["id"]
    assert music["clips"][0]["timeline_range"]["end"] == 29 * SECOND // 2
    options = compiled.export_options()
    assert options["loudness"] == compiled.loudness and options["fonts"] == [str(FONT)]


def test_compiling_is_deterministic() -> None:
    video = c.Video(c.Slideshow(["photo.jpg"] * 3, seconds=2, ken_burns=1.1))
    first = json.dumps(video.compile(facts=FACTS).project, sort_keys=False)
    assert first == json.dumps(video.compile(facts=FACTS).project, sort_keys=False)


def test_errors_name_where_in_the_spec() -> None:
    with pytest.raises(Alpha2ComposeError) as caught:
        c.compile(c.Video(c.Stack([c.Text("x", opacity=2)])), facts=FACTS)
    assert caught.value.path == "spec.body.children[0].opacity"
    assert caught.value.problem == "2 is outside 0..=1"
    assert str(caught.value) == "spec.body.children[0].opacity: 2 is outside 0..=1"
    assert isinstance(caught.value, ValueError)
    with pytest.raises(Alpha2ComposeError) as caught:
        c.compile({"body": {"type": "media", "path": "talk.mp4", "colour": 1}}, facts=FACTS)
    assert caught.value.path == "spec.body.colour"
    assert c.requirements({"body": c.Media("a.png")}) == {
        "media": [{"path": "a.png", "kind": "image"}],
        "texts": [],
    }


def conform_host(tmp_path: Path, fake_host: FakeHost, catalog: list[dict[str, Any]]) -> str:
    recorded = tmp_path / "request.json"
    answer = json.dumps({"catalog": {"assets": catalog}, "report": {"ready": True}})
    return fake_host(
        tmp_path / "host",
        f"""
        request, line = read_request()
        open({str(recorded)!r}, "w", encoding="utf-8").write(line)
        send(request, ok=True, result=json.loads({answer!r}))
        """,
    )


def test_facts_come_from_the_hosts_conform_and_caption_files(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    srt = tmp_path / "talk.srt"
    srt.write_text("\ufeff1\n00:00:00,000 --> 00:00:01,500\nHello\n", encoding="utf-8")
    fingerprint = {
        "byte_length": 10,
        "streams": {
            "video": {"width": 1920, "height": 1080, "frame_rate": {"num": 25, "den": 1}},
            "audio": {"channels": 2, "sample_rate": 48000},
        },
        "duration_ticks": 10 * SECOND,
    }
    host = conform_host(
        tmp_path,
        fake_host,
        [
            {
                "source_id": "m0",
                "revision": 0,
                "original": {"state": "online", "fingerprint": fingerprint},
            }
        ],
    )
    video = c.Video(
        c.CaptionedVideo(tmp_path / "talk.mp4", c.Captions(srt)),
        overlays=[c.Media("photo.jpg", duration=1)],
    )
    facts = c.gather_facts(
        video,
        known={"media": {"photo.jpg": FACTS["media"]["photo.jpg"]}},
        client=HostClient(host_path=host),
    )
    assert facts["media"][str(tmp_path / "talk.mp4")] == {
        "has_audio": True,
        "width": 1920,
        "height": 1080,
        "frame_rate": {"num": 25, "den": 1},
        "duration_ticks": 10 * SECOND,
    }
    assert facts["texts"][str(srt)].startswith("1\n")
    request = json.loads((tmp_path / "request.json").read_text(encoding="utf-8"))
    assert request["assets"] == {"m0": str(tmp_path / "talk.mp4")}
    assert request["project"]["sources"] == [
        {"id": "m0", "kind": "video", "locator": str(tmp_path / "talk.mp4")}
    ]
    compiled = video.compile(facts=facts)
    cues = compiled.project["timeline"]["tracks"][1]["captions"]["cues"]
    assert cues == [{"timeline_range": {"start": 0, "end": 3 * SECOND // 2}, "text": "Hello"}]


def test_a_file_the_host_cannot_read_is_a_compose_error(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    host = conform_host(
        tmp_path, fake_host, [{"source_id": "m0", "revision": 0, "original": {"state": "offline"}}]
    )
    with pytest.raises(Alpha2ComposeError, match=r'cannot read "gone\.mp4": no such file'):
        c.Video(c.Media("gone.mp4")).compile(client=HostClient(host_path=host))


def test_exporting_a_compiled_spec_sends_its_assets_fonts_and_loudness(
    tmp_path: Path, fake_host: FakeHost
) -> None:
    recorded = tmp_path / "export.json"
    host = fake_host(
        tmp_path / "host",
        f"""
        request, line = read_request()
        open({str(recorded)!r}, "w", encoding="utf-8").write(line)
        send(request, ok=True, result={{"videoPipeline": request["output"]}})
        """,
    )
    compiled = c.Video(c.Media("talk.mp4"), loudness=-14).compile(facts=FACTS, fonts=[FONT])
    HostClient(host_path=host).export(compiled, tmp_path / "out.mp4")
    request = json.loads(recorded.read_text(encoding="utf-8"))
    assert request["assets"] == compiled.assets
    assert request["fonts"] == [str(FONT)]
    assert request["loudness"] == {"targetLufs": -14.0, "truePeakCeilingDbtp": -1.0}
    assert request["endTicks"] == 12 * SECOND
    HostClient(host_path=host).export(
        compiled, tmp_path / "silent.mp4", include_audio=False, fonts=[]
    )
    request = json.loads(recorded.read_text(encoding="utf-8"))
    assert request["fonts"] == [] and "loudness" not in request


def test_karaoke_captions_follow_word_times_and_speakers_get_their_look() -> None:
    words = [
        {"start": 0.0, "end": 0.5, "text": "Sing", "speaker": "Ann"},
        {"start": 0.5, "end": 1.0, "text": "along", "speaker": "Ann"},
    ]
    video = c.Video(
        c.Captions(
            words=words,
            style="karaoke",
            highlight="#ffd040",
            speakers={"Ann": {"color": "#ff8080", "position": {"x": 0.3, "y": 0.8}}},
            duration=1,
        )
    )
    compiled = video.compile(facts=FACTS, fonts=[FONT])
    assert alpha2.validate_project(compiled.project) == []
    captions = compiled.project["timeline"]["tracks"][0]["captions"]
    assert [animator["selector"]["state"] for animator in captions["style"]["animators"]] == [
        "spoken",
        "active",
    ]
    cue = captions["cues"][0]
    assert cue["speaker"] == "Ann"
    assert [run["text"] for run in cue["text"]] == ["Sing ", "along"]
    assert captions["styles"]["Ann"]["position"] == {"x": 0.3, "y": 0.8}


def test_a_caption_file_in_a_legacy_code_page_is_a_compose_error(tmp_path: Path) -> None:
    srt = tmp_path / "talk.srt"
    # U+D55C in cp949, which is not UTF-8.
    srt.write_bytes(b"1\n00:00:00,000 --> 00:00:01,000\n\xc7\xd1\n")
    video = c.Video(c.CaptionedVideo("talk.mp4", c.Captions(srt)))
    with pytest.raises(Alpha2ComposeError) as caught:
        c.gather_facts(video, known={"media": FACTS["media"]})
    assert caught.value.path == "facts.texts"
    assert f"{srt} is not UTF-8 text (invalid byte at offset 32)" in str(caught.value)


def test_motion_presets_compile_to_keyframes_timed_from_each_nodes_span() -> None:
    video = c.Video(
        c.Stack(
            [
                c.Color("#102030", enter=c.Rise(0.5, easing="ease-out"), exit=c.Fade(1)),
                c.Text("One two three", reveal=c.Reveal("word", kind="rise", stagger=0.1)),
                c.Counter(0, 1200, thousands=",", suffix=" users", duration=2),
            ],
            stagger=0.25,
            enter=c.Scale(0.4, size=0.8),
            duration=4,
        ),
    )
    compiled = video.compile(facts=FACTS)
    assert alpha2.validate_project(compiled.project) == []
    tracks = compiled.project["timeline"]["tracks"]
    color = tracks[0]["clips"][0]["visual"]
    assert [key["time"] for key in color["opacity"]["keyframes"]][-2:] == [3 * SECOND, 4 * SECOND]
    counter = compiled.project["sources"][2]["generator"]["root"]["children"][0]["text"]
    assert counter["keyframes"][-1]["value"] == "1,200 users"
    # The third child enters half a second after the stack starts.
    scale = tracks[2]["clips"][0]["visual"]["scale"]["keyframes"]
    assert scale[1]["time"] == SECOND // 2


def test_motion_constructors_write_plain_dicts() -> None:
    assert c.Slide("left", 0.3, distance=40) == {
        "type": "slide",
        "direction": "left",
        "duration": 0.3,
        "distance": 40,
    }
    assert c.Scale(size=0.5, fade=False) == {"type": "scale", "size": 0.5, "fade": False}
    assert c.Reveal("line") == {"by": "line"}
    assert c.Counter(1, 2)["start"] == 1


def test_a_template_binds_its_data_and_says_what_it_takes() -> None:
    template = {
        "template": {"id": "acme/card", "version": 1, "kind": "video", "title": "Card"},
        "params": [
            {"name": "title", "type": "text", "max_chars": 10},
            {"name": "photo", "type": "media"},
        ],
        "duration": 2,
        "body": {
            "type": "alpha2/wipe-reveal",
            "with": {"content": {"type": "media", "path": {"$param": "photo"}}, "from": "right"},
        },
        "overlays": [{"type": "text", "text": "{{title}}!"}],
    }
    spec = c.bind(template, {"title": "Hello", "photo": "a.jpg"}, theme="clean-light")
    assert spec["theme"] == "clean-light"
    assert spec["overlays"][0]["text"] == "Hello!"
    assert spec["body"]["children"][0]["content"]["path"] == "a.jpg"
    schema = c.data_schema(template)
    assert schema["title"] == "Card"
    assert schema["required"] == ["title", "photo"]
    assert c.lint(template, {"title": "Hello", "photo": "a.jpg"}) == []
    with pytest.raises(alpha2.Alpha2ComposeError, match="at most 10"):
        c.bind(template, {"title": "x" * 11, "photo": "a.jpg"})
    assert {entry["template"]["id"] for entry in c.TEMPLATES} >= {
        "alpha2/circle-reveal",
        "alpha2/wipe-reveal",
    }
