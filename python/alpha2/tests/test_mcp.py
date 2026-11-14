from __future__ import annotations

import base64
import io
import json
import sys
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest
from conftest import lines_in, project_document

from alpha2 import mcp
from alpha2.mcp import PROTOCOL_VERSIONS, Server, load_templates, serve

FakeHost = Callable[[Path, str], str]

CARD = {
    "template": {"id": "acme/card", "version": 1, "kind": "video", "title": "Card"},
    "params": [{"name": "color", "type": "color", "default": "#336699"}],
    "duration": 1,
    "body": {"type": "color", "color": {"$param": "color"}},
}


def exchange(server: Server, *messages: Any, version: str = "2025-06-18") -> list[Any]:
    lines = [
        {
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {"protocolVersion": version, "capabilities": {}},
        },
        *messages,
    ]
    reader = io.BytesIO(b"".join(json.dumps(line).encode() + b"\n" for line in lines))
    writer = io.BytesIO()
    serve(server, reader, writer)
    return [json.loads(line) for line in writer.getvalue().splitlines()][1:]


def call(name: str, arguments: dict[str, Any], request_id: int = 1) -> dict[str, Any]:
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "tools/call",
        "params": {"name": name, "arguments": arguments},
    }


def answer(server: Server, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
    [reply] = exchange(server, call(name, arguments))
    return dict(reply["result"])


def test_the_server_negotiates_a_version_and_answers_only_requests() -> None:
    writer = io.BytesIO()
    messages = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "1999"}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "ping"},
        {"jsonrpc": "2.0", "id": 3, "method": "resources/list"},
        {"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "nope"}},
    ]
    reader = io.BytesIO(
        b"".join(json.dumps(line).encode() + b"\n" for line in messages) + b"{not json\n"
    )
    serve(Server(), reader, writer)
    replies = [json.loads(line) for line in writer.getvalue().splitlines()]
    assert [reply["id"] for reply in replies] == [1, 2, 3, 4, None]
    assert replies[0]["result"]["protocolVersion"] == PROTOCOL_VERSIONS[0]
    assert replies[0]["result"]["capabilities"] == {"tools": {"listChanged": False}}
    assert replies[1]["result"] == {}
    assert [reply["error"]["code"] for reply in replies[2:]] == [-32601, -32602, -32700]


def test_the_tools_are_listed_with_their_input_schemas() -> None:
    [listed] = exchange(Server(), {"jsonrpc": "2.0", "id": 1, "method": "tools/list"})
    tools = {tool["name"]: tool for tool in listed["result"]["tools"]}
    assert set(tools) == {
        "templates",
        "template_data_schema",
        "bind_template",
        "lint_template",
        "compose_reference",
        "check",
        "describe_project",
        "render_frame",
        "render_video",
    }
    assert tools["render_video"]["inputSchema"]["required"] == ["output"]
    hints = {
        name: (tool["annotations"]["readOnlyHint"], tool["annotations"]["destructiveHint"])
        for name, tool in tools.items()
    }
    writing = {"render_frame", "render_video"}
    assert {name for name, hint in hints.items() if hint == (False, True)} == writing
    assert all(hint == (True, False) for name, hint in hints.items() if name not in writing)


def test_templates_are_listed_described_bound_and_linted() -> None:
    server = Server(templates=[CARD])
    listed = answer(server, "templates", {})["structuredContent"]["templates"]
    ids = [template["id"] for template in listed]
    assert ids[0] == "acme/card" and "alpha2/circle-reveal" in ids
    assert listed[0]["params"] == [{"name": "color", "type": "color", "required": False}]
    schema = answer(server, "template_data_schema", {"template": "acme/card"})
    assert schema["structuredContent"]["properties"]["color"]["default"] == "#336699"
    bound = answer(server, "bind_template", {"template": "acme/card", "data": {"color": "#ff0000"}})
    assert bound["structuredContent"]["spec"]["body"]["color"] == "#ff0000"
    assert json.loads(bound["content"][0]["text"]) == bound["structuredContent"]
    issues = answer(server, "lint_template", {"template": CARD})["structuredContent"]["issues"]
    assert issues == []


def test_a_refusal_is_a_tool_error_naming_where() -> None:
    refused = answer(
        Server(templates=[CARD]), "bind_template", {"template": "acme/card", "data": {"x": 1}}
    )
    assert refused["isError"] is True
    assert refused["structuredContent"]["path"] == "data.x"
    missing = answer(Server(), "template_data_schema", {"template": "acme/none"})
    assert missing["isError"] is True and "`templates` lists them" in missing["content"][0]["text"]
    neither = answer(Server(), "check", {})
    assert neither["isError"] is True and "either `spec`" in neither["content"][0]["text"]


def test_templates_load_from_directories(tmp_path: Path) -> None:
    (tmp_path / "card.json").write_text(json.dumps(CARD), encoding="utf-8")
    (tmp_path / "notes.json").write_text("{}", encoding="utf-8")
    assert load_templates([str(tmp_path)]) == [CARD]


def test_the_reference_gives_the_schema_a_slice_at_a_time() -> None:
    server = Server()
    overview = answer(server, "compose_reference", {})["structuredContent"]
    types = [node["type"] for node in overview["nodes"]]
    assert "box" in types and "raw" not in types
    assert "Canvas" in overview["spec"]["$defs"]
    assert "Node" not in overview["spec"]["$defs"]
    assert "gaussian-blur" in overview["effects"]
    box = answer(server, "compose_reference", {"node": "box"})["structuredContent"]
    assert "BoxNode" in box["$defs"] and "Node" not in box["$defs"]
    blur = answer(server, "compose_reference", {"effect": "gaussian-blur"})["structuredContent"]
    assert blur["schema"]["properties"]["effect"]["const"] == "gaussian-blur"
    unknown = answer(server, "compose_reference", {"node": "nope"})
    assert unknown["isError"] is True and "box" in unknown["content"][0]["text"]


def test_a_project_is_described_with_its_problems() -> None:
    project = project_document()
    described = answer(Server(), "describe_project", {"project": project})
    summary = described["structuredContent"]["summary"]
    assert summary["width"] == project["settings"]["width"]
    assert described["structuredContent"]["problems"] == []
    broken = answer(Server(), "describe_project", {"project": {"schema_version": 1}})
    assert broken["structuredContent"]["problems"]
    odd = answer(Server(), "describe_project", {"project": {"settings": {"frame_rate": 30}}})
    assert odd["isError"] is False and odd["structuredContent"]["problems"]
    assert odd["structuredContent"]["summary"] == {
        "name": None,
        "width": None,
        "height": None,
        "frameRate": None,
        "seconds": None,
        "sources": {},
        "tracks": [],
        "compositions": 0,
    }


def raw_call(name: Any, arguments: Any) -> str:
    message = {"jsonrpc": "2.0", "id": 1, "method": "tools/call"}
    return json.dumps({**message, "params": {"name": name, "arguments": arguments}})


SPEC = {"duration": 1, "body": {"type": "color", "color": "#336699"}}

# Each line is followed by a ping (id 99); the server answers both and ends at end of input.
MALFORMED: dict[str, tuple[str, Any, str]] = {
    "a project whose frame rate is a number": (
        raw_call("describe_project", {"project": {"settings": {"frame_rate": 30}}}),
        1,
        "result",
    ),
    "a project whose source is a number": (
        raw_call("describe_project", {"project": {**project_document(), "sources": [1]}}),
        1,
        "result",
    ),
    "a template whose header is a string": (
        raw_call("check", {"template": {"template": "acme/card"}}),
        1,
        "tool error",
    ),
    "an end past any float": (
        raw_call("render_video", {"spec": SPEC, "output": "never.mp4", "end": 1}).replace(
            '"end": 1', '"end": 1e400'
        ),
        1,
        "tool error",
    ),
    "a frame time past any float": (
        raw_call("render_frame", {"spec": SPEC, "seconds": 1}).replace(
            '"seconds": 1', '"seconds": 1e400'
        ),
        1,
        "tool error",
    ),
    "a tool name that is a list": (raw_call(["describe_project"], {}), 1, "-32602"),
    # A parser without a nesting limit (Python 3.15) reads it and finds no request instead.
    "a line nested too deep to parse": ("[" * 100_000 + "]" * 100_000, None, "-32700|-32600"),
    "a ping whose id is a lone surrogate": (
        '{"jsonrpc":"2.0","id":"\\ud800","method":"ping"}',
        "\ud800",
        "result",
    ),
    "a panic in the native module": (
        raw_call("describe_project", {"project": project_document()}),
        1,
        "INTERNAL",
    ),
}


class _Panic(BaseException):
    """Stands in for the native module's panic, which is not an ``Exception``."""


@pytest.mark.parametrize("case", list(MALFORMED))
def test_every_malformed_request_is_answered_and_the_server_keeps_serving(
    case: str, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    line, request_id, outcome = MALFORMED[case]
    if outcome == "INTERNAL":

        def panic(_text: str) -> list[str]:
            raise _Panic("attempt to negate with overflow")

        monkeypatch.setattr(mcp._native, "validate_project_json", panic)
    monkeypatch.chdir(tmp_path)
    reader = io.BytesIO(f"{line}\n".encode() + b'{"jsonrpc":"2.0","id":99,"method":"ping"}\n')
    writer = io.BytesIO()
    # A host that resolves, as in an installation; no call reaches it.
    serve(Server(host_path=sys.executable), reader, writer)
    replies = [json.loads(line) for line in writer.getvalue().splitlines()]
    assert [reply["id"] for reply in replies] == [request_id, 99]
    first = replies[0]
    if outcome == "result":
        assert "result" in first and not first["result"].get("isError")
    elif outcome.startswith("-"):
        assert first["error"]["code"] in {int(code) for code in outcome.split("|")}
    else:
        assert first["result"]["isError"] is True
        if outcome == "INTERNAL":
            assert first["result"]["structuredContent"] == {"code": "INTERNAL"}
            assert first["result"]["content"][0]["text"].startswith("_Panic: ")
    assert not (tmp_path / "never.mp4").exists()


def still_host(tmp_path: Path, fake_host: FakeHost, error: dict[str, Any] | None = None) -> str:
    return fake_host(
        tmp_path / "host",
        f"""
        record("starts", str(os.getpid()))
        def answer(request):
            error = {error!r}
            if error:
                send(request, ok=False, error=error)
            else:
                still = request["stills"][0]
                frame = {{"frameTime": still["time"], "width": 16, "height": 9}}
                if "output" in still:
                    open(still["output"], "wb").write(b"PNG")
                    frame["output"] = still["output"]
                    attachments = []
                else:
                    frame.update(output=None, attachment=0)
                    attachments = [("image/png", b"PNG")]
                write_message({{"requestId": request["requestId"], "ok": True, "hostState": "ready",
                    "result": {{"stills": [frame], "renderer": "cpu", "timings": {{}}}}}}, attachments)
        serve(answer)
        """,
    )


def test_a_frame_comes_back_as_an_image(tmp_path: Path, fake_host: FakeHost) -> None:
    server = Server(templates=[CARD], host_path=still_host(tmp_path, fake_host))
    frame = answer(server, "render_frame", {"template": "acme/card", "seconds": 0.5})
    image, details = frame["content"]
    assert image == {
        "type": "image",
        "data": base64.b64encode(b"PNG").decode(),
        "mimeType": "image/png",
    }
    assert json.loads(details["text"])["seconds"] == 0.5


def test_tool_calls_reuse_one_host(tmp_path: Path, fake_host: FakeHost) -> None:
    server = Server(templates=[CARD], host_path=still_host(tmp_path, fake_host))
    replies = exchange(
        server,
        call("render_frame", {"template": "acme/card", "seconds": 0.25}, 1),
        call("render_frame", {"template": "acme/card", "seconds": 0.5}, 2),
    )
    assert [reply["result"]["isError"] for reply in replies] == [False, False]
    assert len(lines_in(tmp_path / "host" / "starts")) == 1


def test_an_older_client_gets_no_structured_content(tmp_path: Path, fake_host: FakeHost) -> None:
    server = Server(templates=[CARD], host_path=still_host(tmp_path, fake_host))
    [reply] = exchange(
        server, call("render_frame", {"template": "acme/card"}), version="2024-11-05"
    )
    assert reply["result"]["isError"] is False
    assert "structuredContent" not in reply["result"]


def test_text_without_fonts_says_how_to_give_them(tmp_path: Path, fake_host: FakeHost) -> None:
    error = {
        "code": "MISSING_INPUT",
        "message": "the project shows text, but no fonts were registered",
    }
    server = Server(host_path=still_host(tmp_path, fake_host, error))
    refused = answer(
        server, "render_frame", {"spec": {"duration": 1, "body": {"type": "text", "text": "Hi"}}}
    )
    assert refused["isError"] is True
    assert "--font PATH" in refused["content"][0]["text"]
