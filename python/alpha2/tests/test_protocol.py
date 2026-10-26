"""The protocol, failure classes and engine policy, with no host."""

from __future__ import annotations

import ast
import json
from pathlib import Path

import pytest
from conftest import corpus

import alpha2
from alpha2 import _native
from alpha2.errors import failure_class_of_code
from alpha2.protocol import create_request, loader_refused, parse_host_message


def test_the_shared_protocol_corpus_parses_as_the_host_and_node_read_it() -> None:
    for fixture in corpus("host-protocol-corpus.json"):
        request = fixture["request"]
        if fixture["requestValid"]:
            payload = {
                key: value
                for key, value in request.items()
                if key not in ("protocolVersion", "operation", "requestId")
            }
            assert create_request(request["operation"], request["requestId"], payload) == request, (
                fixture["name"]
            )
        for message in fixture["messages"]:
            assert parse_host_message(json.dumps(message)) == message, fixture["name"]


@pytest.mark.parametrize(
    "message",
    [
        "[]",
        '{"protocolVersion": 2, "requestId": "r", "ok": true, "result": {}}',
        '{"protocolVersion": true, "requestId": "r", "ok": true, "result": {}}',
        '{"protocolVersion": 1, "type": "progress", "requestId": "r", "phase": "done", '
        '"completed": 1, "total": 1}',
        '{"protocolVersion": 1, "type": "progress", "requestId": "r", "completed": 2, "total": 1}',
        '{"protocolVersion": 1, "type": "progress", "requestId": "r", "completed": -1, "total": 1}',
        '{"protocolVersion": 1, "type": "fatal", "error": {"code": "", "message": "m"}}',
        '{"protocolVersion": 1, "requestId": "r", "ok": false, "error": {"code": "X"}}',
        '{"protocolVersion": 1, "requestId": "r", "ok": true}',
        '{"protocolVersion": 1, "requestId": 3, "ok": true, "result": {}}',
    ],
)
def test_a_malformed_host_message_is_refused(message: str) -> None:
    with pytest.raises(ValueError):
        parse_host_message(message)


def test_the_loaders_refusal_is_told_apart_from_a_host_that_started_and_died() -> None:
    refused = [
        # glibc: a missing library, one of another version, and a missing symbol.
        (
            127,
            "alpha2-host: error while loading shared libraries: libswscale.so.9: cannot open "
            "shared object file: No such file or directory\n",
        ),
        (
            127,
            "./alpha2-host: /usr/lib/x86_64-linux-gnu/libavcodec.so.62: version `LIBAVCODEC_62' "
            "not found (required by ./alpha2-host)\n",
        ),
        (
            127,
            "./alpha2-host: symbol lookup error: ./alpha2-host: undefined symbol: "
            "av_frame_side_data_get_c\n",
        ),
        # musl.
        (
            127,
            "Error loading shared library libavcodec.so.62: No such file or directory "
            "(needed by /usr/bin/alpha2-host)\n",
        ),
        # macOS dyld aborts the process.
        (None, "dyld[4242]: Library not loaded: @rpath/libavcodec.62.dylib\n  Referenced from\n"),
        (None, "dyld[4242]: Symbol not found: _av_frame_side_data_get_c\n"),
        # Windows ends it with STATUS_DLL_NOT_FOUND or STATUS_ENTRYPOINT_NOT_FOUND and writes
        # nothing; Python may report the status signed or unsigned.
        (0xC0000135, ""),
        (0xC0000139, ""),
        (0xC0000135 - 2**32, ""),
    ]
    for code, stderr in refused:
        assert loader_refused(code, stderr), stderr or code
    started = [
        (None, "last words\n"),
        (1, "alpha2-host: stream did not contain valid UTF-8\n"),
        (127, ""),
        (134, "thread 'main' panicked at 'error while loading the project'\n"),
        (0xC0000005, ""),
    ]
    for code, stderr in started:
        assert not loader_refused(code, stderr), stderr or code


def test_every_code_is_classified_as_the_shared_table_says() -> None:
    table = corpus("failure-classes.json")
    for section in ("protocol", "node"):
        for code, expected in table[section].items():
            assert failure_class_of_code(code) == (expected["code"], expected["resource"]), code
    assert failure_class_of_code("NOT_A_CODE") == ("UNKNOWN", "unknown")
    assert failure_class_of_code(None) == ("UNKNOWN", "unknown")


def test_every_code_the_client_makes_is_in_the_shared_table() -> None:
    table = corpus("failure-classes.json")
    known = set(table["protocol"]) | set(table["node"])
    source = Path(alpha2.client.__file__).read_text(encoding="utf-8")
    made = set()
    for node in ast.walk(ast.parse(source)):
        if (
            isinstance(node, ast.Call)
            and isinstance(node.func, ast.Name)
            and node.func.id == "Alpha2HostError"
            and node.args
            and isinstance(node.args[0], ast.Constant)
        ):
            made.add(node.args[0].value)
    made |= {"INVALID_INPUT", "ABORTED", "CONFORM_FAILED", "GPU_INIT_FAILED"}
    assert len(made) >= 8, made
    assert made <= known, made - known


def test_export_formats_resolve_as_the_engine_resolves_them() -> None:
    cases = corpus("export-container-corpus.json")["resolve"]
    for case in cases:
        arguments = (case["container"], case["extension"], case["video"], case["audio"])
        if "ok" in case:
            assert list(_native.resolve_export_format(*arguments)) == case["ok"], case
        else:
            with pytest.raises(ValueError):
                _native.resolve_export_format(*arguments)


def test_the_native_module_is_the_packages_version_and_the_engines_constants() -> None:
    assert alpha2.__version__ == _native.__version__
    assert _native.SCHEMA_VERSION == 1
    assert _native.TICKS_PER_SECOND == 120_000
    limits = json.loads(
        (Path(__file__).resolve().parents[3] / "schema" / "project-limits.v1.json").read_text()
    )
    assert limits["MAX_PROJECT_JSON_BYTES"] == _native.MAX_PROJECT_JSON_BYTES
