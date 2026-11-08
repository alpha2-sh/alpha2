"""An MCP server over standard input and output, run as ``python -m alpha2 mcp``.

Specs and templates compile in this process and ``alpha2-host`` renders. Relative paths are read
from the server's working directory.
"""

from __future__ import annotations

import base64
import functools
import json
import shutil
import sys
import tempfile
import threading
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import IO, Any, cast

from . import _native
from . import compose as _compose
from .builder import TICKS_PER_SECOND
from .client import HostClient
from .errors import Alpha2ComposeError, Alpha2HostError

PROTOCOL_VERSIONS = ("2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05")
"""The MCP versions this server speaks, newest first."""

_STRUCTURED_SINCE = "2025-06-18"

INSTRUCTIONS = """\
alpha2 renders videos from JSON. To make one from a template: `templates` lists them,
`template_data_schema` says what a template's data takes, `bind_template` or `check` tries the
data, and `render_frame` shows a frame before `render_video` writes the file. To write a spec
yourself, read `compose_reference` (no arguments first, then one node type at a time). A spec,
template or data that does not compile names where: fix that path and try again."""


class ToolError(Exception):
    """A tool call that fails with a message the model can act on."""

    def __init__(self, message: str, details: Mapping[str, Any] | None = None) -> None:
        super().__init__(message)
        self.details = dict(details or {})


@dataclass
class Tool:
    name: str
    title: str
    description: str
    input_schema: dict[str, Any]
    run: Callable[[dict[str, Any]], Any]
    read_only: bool = True

    def listing(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "inputSchema": self.input_schema,
            "annotations": {
                "title": self.title,
                "readOnlyHint": self.read_only,
                "destructiveHint": False,
                "openWorldHint": False,
            },
        }


def _object(properties: Mapping[str, Any], required: Sequence[str] = ()) -> dict[str, Any]:
    schema: dict[str, Any] = {
        "type": "object",
        "properties": dict(properties),
        "additionalProperties": False,
    }
    if required:
        schema["required"] = list(required)
    return schema


_SPEC = {
    "description": "A compose spec (see compose_reference), or the path of a JSON file of one.",
    "type": ["object", "string"],
}
_TEMPLATE = {
    "description": "A template id from `templates`, a template document, or the path of a JSON "
    "file holding one.",
    "type": ["object", "string"],
}
_DATA = {"description": "The template's data, as its data schema says.", "type": "object"}
_SOURCE = {"spec": _SPEC, "template": _TEMPLATE, "data": _DATA}


@functools.cache
def _core_schema() -> dict[str, Any]:
    return cast("dict[str, Any]", json.loads(_native.compose_core_schema()))


def _refs(value: Any) -> list[str]:
    found: list[str] = []
    stack = [value]
    while stack:
        item = stack.pop()
        if isinstance(item, dict):
            ref = item.get("$ref")
            if isinstance(ref, str) and ref.startswith("#/$defs/"):
                found.append(ref.removeprefix("#/$defs/"))
            stack.extend(item.values())
        elif isinstance(item, list):
            stack.extend(item)
    return found


def _with_definitions(value: Any) -> dict[str, Any]:
    """``value`` and the definitions it refers to, except ``Node`` (any node) and ``EffectUse``
    (any effect), which ``compose_reference`` describes one type at a time."""
    defs = _core_schema()["$defs"]
    picked: dict[str, Any] = {}
    stack = _refs(value)
    while stack:
        name = stack.pop()
        if name in picked or name in ("Node", "EffectUse") or name not in defs:
            continue
        picked[name] = defs[name]
        stack.extend(_refs(defs[name]))
    return {"schema": value, "$defs": dict(sorted(picked.items()))}


def _node_variants() -> dict[str, dict[str, Any]]:
    variants = _core_schema()["$defs"]["Node"]["oneOf"]
    return {
        variant["properties"]["type"]["const"]: variant
        for variant in variants
        if "const" in variant["properties"]["type"]
    }


def _effect_variants() -> dict[str, dict[str, Any]]:
    variants = _core_schema()["$defs"]["EffectUse"]["oneOf"]
    return {
        variant["properties"]["effect"]["const"]: variant
        for variant in variants
        if "const" in variant["properties"]["effect"]
    }


def _summary(project: Mapping[str, Any]) -> dict[str, Any]:
    settings = project.get("settings", {})
    rate = settings.get("frame_rate", {})
    try:
        end = _native.project_end_ticks(json.dumps(project))
    except ValueError:
        end = None
    sources: dict[str, int] = {}
    for source in project.get("sources", []):
        sources[source.get("kind", "?")] = sources.get(source.get("kind", "?"), 0) + 1
    return {
        "name": project.get("name"),
        "width": settings.get("width"),
        "height": settings.get("height"),
        "frameRate": f"{rate.get('num')}/{rate.get('den')}",
        "seconds": None if end is None else end / TICKS_PER_SECOND,
        "sources": sources,
        "tracks": [
            {"id": track.get("id"), "kind": track.get("kind"), "clips": len(track.get("clips", []))}
            for track in project.get("timeline", {}).get("tracks", [])
        ],
        "compositions": len(project.get("compositions", [])),
    }


def _read_json(path: str, what: str) -> Any:
    try:
        return json.loads(Path(path).read_text(encoding="utf-8"))
    except OSError as error:
        raise ToolError(f"cannot read the {what} {path}: {error.strerror}") from None
    except json.JSONDecodeError as error:
        raise ToolError(f"the {what} {path} is not JSON: {error}") from None


@dataclass
class Server:
    """The tools and their state, served by :func:`serve`."""

    templates: list[dict[str, Any]] = field(default_factory=list)
    """Templates beside the built-in ones."""
    fonts: list[str] = field(default_factory=list)
    """Font files text is measured and drawn with."""
    host_path: str | None = None
    _client: HostClient | None = None
    _frames: Path | None = None
    _frame_count: int = 0
    _lock: threading.Lock = field(default_factory=threading.Lock)

    def close(self) -> None:
        if self._client is not None:
            self._client.close()
        if self._frames is not None:
            shutil.rmtree(self._frames, ignore_errors=True)
            self._frames = None

    def client(self) -> HostClient:
        """The one client every tool call runs on, so calls reuse its hosts."""
        with self._lock:
            if self._client is None:
                self._client = HostClient(host_path=self.host_path)
            return self._client

    def all_templates(self) -> list[dict[str, Any]]:
        return [*self.templates, *_compose.TEMPLATES]

    def template(self, value: Any) -> dict[str, Any]:
        if isinstance(value, dict):
            return value
        if not isinstance(value, str):
            raise ToolError("template is a template id, a template document or a file path")
        for template in self.all_templates():
            if template["template"]["id"] == value:
                return template
        if value.endswith(".json"):
            document = _read_json(value, "template")
            if isinstance(document, dict):
                return document
        raise ToolError(f'no template "{value}"; `templates` lists them')

    def spec(self, arguments: Mapping[str, Any]) -> dict[str, Any]:
        """The spec ``arguments`` names, as ``spec`` or as ``template`` bound to ``data``."""
        if "spec" in arguments:
            value = arguments["spec"]
            spec = _read_json(value, "spec") if isinstance(value, str) else value
            if not isinstance(spec, dict):
                raise ToolError("spec is an object or the path of a JSON file holding one")
            return spec
        if "template" not in arguments:
            raise ToolError("give either `spec`, or `template` and its `data`")
        template = self.template(arguments["template"])
        data = arguments.get("data", {})
        header = template.get("template", {})
        if header.get("kind", "video") == "node":
            body = {"type": header.get("id"), "with": data}
            return _compose.bind({"body": body}, templates=[template, *self.templates])
        return _compose.bind(template, data, templates=self.templates)

    def compiled(self, arguments: Mapping[str, Any]) -> _compose.Compiled:
        return _compose.compile(
            self.spec(arguments),
            fonts=self.fonts,
            client=self.client(),
            templates=self.templates,
        )

    def rendered(self, render: Callable[[], Any]) -> Any:
        try:
            return render()
        except Alpha2HostError as error:
            if error.code == "MISSING_INPUT" and not self.fonts and "font" in str(error):
                raise ToolError(
                    f"{error}; start the server with --font PATH for each font text uses",
                    {"code": error.code},
                ) from None
            raise

    def frame_path(self) -> Path:
        with self._lock:
            if self._frames is None:
                self._frames = Path(tempfile.mkdtemp(prefix="alpha2-mcp-"))
            self._frame_count += 1
            return self._frames / f"frame-{self._frame_count}.png"

    # The tools.

    def list_templates(self, _arguments: dict[str, Any]) -> Any:
        listed = []
        for template in self.all_templates():
            header = template["template"]
            listed.append(
                {
                    "id": header["id"],
                    "version": header.get("version"),
                    "kind": header.get("kind", "video"),
                    "title": header.get("title"),
                    "description": header.get("description"),
                    "params": [
                        {
                            "name": param["name"],
                            "type": param["type"],
                            "required": param.get("required", "default" not in param),
                        }
                        for param in template.get("params", [])
                    ],
                }
            )
        return {"templates": listed}

    def template_data_schema(self, arguments: dict[str, Any]) -> Any:
        return _compose.data_schema(self.template(arguments["template"]))

    def bind_template(self, arguments: dict[str, Any]) -> Any:
        options: dict[str, Any] = {"templates": self.templates}
        if "theme" in arguments:
            options["theme"] = arguments["theme"]
        template = self.template(arguments["template"])
        return {"spec": _compose.bind(template, arguments.get("data", {}), **options)}

    def lint_template(self, arguments: dict[str, Any]) -> Any:
        issues = _compose.lint(
            self.template(arguments["template"]), arguments.get("data"), templates=self.templates
        )
        return {"issues": issues}

    def compose_reference(self, arguments: dict[str, Any]) -> Any:
        schema = _core_schema()
        if "node" in arguments:
            nodes = _node_variants()
            if arguments["node"] not in nodes:
                raise ToolError(
                    f'no node type "{arguments["node"]}"; the types are {", ".join(nodes)}'
                )
            return _with_definitions(nodes[arguments["node"]])
        if "effect" in arguments:
            effects = _effect_variants()
            if arguments["effect"] not in effects:
                raise ToolError(f'no built-in effect "{arguments["effect"]}"')
            return _with_definitions(effects[arguments["effect"]])
        if "definition" in arguments:
            if arguments["definition"] not in schema["$defs"]:
                raise ToolError(f'no definition "{arguments["definition"]}"')
            return _with_definitions(schema["$defs"][arguments["definition"]])
        top = {key: value for key, value in schema.items() if key != "$defs"}
        return {
            "spec": _with_definitions(top),
            "nodes": [
                {"type": name, "description": variant.get("description", "")}
                for name, variant in _node_variants().items()
            ],
            "effects": list(_effect_variants()),
            "templates": [template["template"]["id"] for template in self.all_templates()],
            "themes": list(_native.COMPOSE_THEMES),
            "note": "`#/$defs/Node` is any node: pass `node` for one type's fields. "
            "`#/$defs/EffectUse` is any effect: pass `effect` for one. A node may also be a "
            'template, `{"type": "<template id>", "with": {...data}}`.',
        }

    def check(self, arguments: dict[str, Any]) -> Any:
        compiled = self.compiled(arguments)
        return {
            "summary": _summary(compiled.project),
            "warnings": compiled.warnings,
            "assets": compiled.assets,
        }

    def describe_project(self, arguments: dict[str, Any]) -> Any:
        value = arguments["project"]
        project = _read_json(value, "project") if isinstance(value, str) else value
        if not isinstance(project, dict):
            raise ToolError("project is an object or the path of a JSON file holding one")
        problems = _native.validate_project_json(json.dumps(project))
        return {"summary": _summary(project), "problems": problems}

    def render_frame(self, arguments: dict[str, Any]) -> Any:
        compiled = self.compiled(arguments)
        output = Path(arguments["output"]) if "output" in arguments else self.frame_path()
        result = self.rendered(
            lambda: self.client().still(
                compiled,
                float(arguments.get("seconds", 0)),
                output,
                overwrite=bool(arguments.get("overwrite", False)),
            )
        )
        still = result["stills"][0]
        return _Image(
            output.read_bytes(),
            {
                "output": still["output"],
                "seconds": still["frameTime"] / TICKS_PER_SECOND,
                "width": still["width"],
                "height": still["height"],
                "warnings": compiled.warnings,
            },
        )

    def render_video(self, arguments: dict[str, Any]) -> Any:
        compiled = self.compiled(arguments)
        options: dict[str, Any] = {"overwrite": bool(arguments.get("overwrite", False))}
        if "start" in arguments:
            options["start_ticks"] = round(float(arguments["start"]) * TICKS_PER_SECOND)
        if "end" in arguments:
            options["end_ticks"] = round(float(arguments["end"]) * TICKS_PER_SECOND)
        result = self.rendered(
            lambda: self.client().export(compiled, arguments["output"], **options)
        )
        return {
            "output": result.get("output"),
            "summary": _summary(compiled.project),
            "warnings": compiled.warnings,
            "renderer": result.get("renderer"),
        }

    def tools(self) -> list[Tool]:
        return [
            Tool(
                "templates",
                "List templates",
                "The templates a spec can use, with what each takes. A `video` template binds "
                "to a whole spec; a `node` template is one node of a spec.",
                _object({}),
                self.list_templates,
            ),
            Tool(
                "template_data_schema",
                "Template data schema",
                "The JSON Schema of a template's data: its fields, their types and limits.",
                _object({"template": _TEMPLATE}, ["template"]),
                self.template_data_schema,
            ),
            Tool(
                "bind_template",
                "Bind a template",
                "A template filled in with its data, as a plain compose spec (or node). Fails "
                "naming the data field that does not fit.",
                _object(
                    {
                        "template": _TEMPLATE,
                        "data": _DATA,
                        "theme": {
                            "description": "A theme name or a theme object, replacing a video "
                            "template's.",
                            "type": ["string", "object"],
                        },
                    },
                    ["template"],
                ),
                self.bind_template,
            ),
            Tool(
                "lint_template",
                "Lint a template",
                "A template's problems: unknown or unused parameters, literal media, colors or "
                "words that belong in its data, and whether it binds (to `data`, or to sample "
                "data).",
                _object({"template": _TEMPLATE, "data": _DATA}, ["template"]),
                self.lint_template,
            ),
            Tool(
                "compose_reference",
                "Compose reference",
                "The compose schema, a slice at a time. No arguments: the spec's top-level "
                "fields and the node types, effects, templates and themes. `node`: one node "
                "type's fields. `effect`: one built-in effect's parameters. `definition`: a "
                "named definition the others refer to.",
                _object(
                    {
                        "node": {"type": "string", "description": "A node type, such as `box`."},
                        "effect": {
                            "type": "string",
                            "description": "An effect id, such as `gaussian-blur`.",
                        },
                        "definition": {"type": "string", "description": "A `$defs` name."},
                    }
                ),
                self.compose_reference,
            ),
            Tool(
                "check",
                "Check a spec",
                "Compiles a spec (or a template with its data) without rendering: its size, "
                "length, tracks and media, and what the compiler could not do as asked. Reads "
                "the media the spec names.",
                _object(_SOURCE),
                self.check,
            ),
            Tool(
                "describe_project",
                "Describe a project",
                "Summarizes an alpha2 project document (the engine's format, not a compose "
                "spec) and lists why it is invalid, if it is.",
                _object(
                    {
                        "project": {
                            "description": "A project document, or the path of a JSON file "
                            "holding one.",
                            "type": ["object", "string"],
                        }
                    },
                    ["project"],
                ),
                self.describe_project,
            ),
            Tool(
                "render_frame",
                "Render a frame",
                "Renders one frame of a spec (or a template with its data) and returns it as a "
                "PNG image, to see the result before rendering the video.",
                _object(
                    {
                        **_SOURCE,
                        "seconds": {"type": "number", "minimum": 0, "description": "Default 0."},
                        "output": {
                            "type": "string",
                            "description": "Where to keep the PNG. Default: a temporary file.",
                        },
                        "overwrite": {
                            "type": "boolean",
                            "description": "Replace a file at `output`. Default false.",
                        },
                    }
                ),
                self.render_frame,
                read_only=False,
            ),
            Tool(
                "render_video",
                "Render a video",
                "Renders a spec (or a template with its data) to a video file: MP4, MOV, "
                "Matroska or WebM by the output's extension.",
                _object(
                    {
                        **_SOURCE,
                        "output": {"type": "string", "description": "The video file to write."},
                        "start": {"type": "number", "minimum": 0, "description": "Seconds."},
                        "end": {"type": "number", "minimum": 0, "description": "Seconds."},
                        "overwrite": {
                            "type": "boolean",
                            "description": "Replace a file at `output`. Default false.",
                        },
                    },
                    ["output"],
                ),
                self.render_video,
                read_only=False,
            ),
        ]


@dataclass
class _Image:
    png: bytes
    details: dict[str, Any]


def _content(result: Any, structured: bool) -> dict[str, Any]:
    if isinstance(result, _Image):
        answer: dict[str, Any] = {
            "content": [
                {
                    "type": "image",
                    "data": base64.b64encode(result.png).decode(),
                    "mimeType": "image/png",
                },
                {
                    "type": "text",
                    "text": json.dumps(result.details, ensure_ascii=False, separators=(",", ":")),
                },
            ],
            "isError": False,
        }
        if structured:
            answer["structuredContent"] = result.details
        return answer
    answer = {
        "content": [
            {"type": "text", "text": json.dumps(result, ensure_ascii=False, separators=(",", ":"))}
        ],
        "isError": False,
    }
    if structured and isinstance(result, dict):
        answer["structuredContent"] = result
    return answer


def _failure(message: str, details: Mapping[str, Any], structured: bool) -> dict[str, Any]:
    answer: dict[str, Any] = {"content": [{"type": "text", "text": message}], "isError": True}
    if structured and details:
        answer["structuredContent"] = dict(details)
    return answer


def _call(tool: Tool, arguments: dict[str, Any], structured: bool) -> dict[str, Any]:
    try:
        return _content(tool.run(arguments), structured)
    except ToolError as error:
        return _failure(str(error), error.details, structured)
    except Alpha2ComposeError as error:
        return _failure(str(error), {"path": error.path, "problem": error.problem}, structured)
    except Alpha2HostError as error:
        return _failure(f"{error.code}: {error}", {"code": error.code}, structured)
    except KeyError as error:
        return _failure(f"missing argument {error}", {}, structured)
    except (ValueError, TypeError, OSError) as error:
        return _failure(str(error), {}, structured)


class _Protocol:
    def __init__(self, server: Server) -> None:
        self.server = server
        self.tools = {tool.name: tool for tool in server.tools()}
        self.version = PROTOCOL_VERSIONS[0]

    def answer(self, message: Any) -> dict[str, Any] | None:
        if not isinstance(message, dict) or message.get("jsonrpc") != "2.0":
            return _error(None, -32600, "a message is a JSON-RPC 2.0 object")
        method = message.get("method")
        if "id" not in message:
            return None
        request_id = message["id"]
        params = message.get("params") or {}
        if not isinstance(method, str) or not isinstance(params, dict):
            return _error(request_id, -32600, "a request has a method and object params")
        if method == "initialize":
            asked = params.get("protocolVersion")
            self.version = asked if asked in PROTOCOL_VERSIONS else PROTOCOL_VERSIONS[0]
            return _result(
                request_id,
                {
                    "protocolVersion": self.version,
                    "capabilities": {"tools": {"listChanged": False}},
                    "serverInfo": {"name": "alpha2", "version": _version()},
                    "instructions": INSTRUCTIONS,
                },
            )
        if method == "ping":
            return _result(request_id, {})
        if method == "tools/list":
            return _result(request_id, {"tools": [tool.listing() for tool in self.tools.values()]})
        if method == "tools/call":
            tool = self.tools.get(params.get("name", ""))
            if tool is None:
                return _error(request_id, -32602, f"unknown tool {params.get('name')!r}")
            arguments = params.get("arguments") or {}
            if not isinstance(arguments, dict):
                return _error(request_id, -32602, "arguments is an object")
            structured = self.version >= _STRUCTURED_SINCE
            return _result(request_id, _call(tool, arguments, structured))
        return _error(request_id, -32601, f"unknown method {method!r}")


def _version() -> str:
    from importlib.metadata import PackageNotFoundError, version

    try:
        return version("alpha2")
    except PackageNotFoundError:
        return "0"


def _result(request_id: Any, result: Any) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": request_id, "result": result}


def _error(request_id: Any, code: int, message: str) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}}


def serve(server: Server, reader: IO[bytes] | None = None, writer: IO[bytes] | None = None) -> None:
    """Answers MCP messages, one JSON object per line, until the input ends."""
    reader = reader or sys.stdin.buffer
    writer = writer or sys.stdout.buffer
    protocol = _Protocol(server)
    try:
        for line in reader:
            if not line.strip():
                continue
            try:
                message = json.loads(line)
            except (json.JSONDecodeError, UnicodeDecodeError):
                reply: dict[str, Any] | None = _error(None, -32700, "not JSON")
            else:
                reply = protocol.answer(message)
            if reply is not None:
                writer.write(json.dumps(reply, ensure_ascii=False).encode() + b"\n")
                writer.flush()
    finally:
        server.close()


def load_templates(directories: Sequence[str]) -> list[dict[str, Any]]:
    """The template documents (``*.json`` with a ``template`` header) in ``directories``."""
    templates = []
    for directory in directories:
        for path in sorted(Path(directory).glob("*.json")):
            document = _read_json(str(path), "template")
            if isinstance(document, dict) and isinstance(document.get("template"), dict):
                templates.append(document)
    return templates
