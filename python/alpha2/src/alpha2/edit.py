"""Edit operations that apply data to a project once, run by the engine in this process.

The operations are named for primitives: ``remove_ranges`` (drop silences, keep highlights),
``split_at`` (cut on beats) and ``bake`` (data bindings to keyframes, words to cues). Every kept
moment renders as before, and the result is the same canonical document on every surface::

    import alpha2

    edited = alpha2.edit.apply(project, {
        "op": "remove_ranges",
        "ranges": {"data": "silence"},
        "target": {"track": "v1"},
        "padding": 12_000,
    })
    alpha2.export(edited.project, "tight.mp4")
"""

from __future__ import annotations

import json
from collections.abc import Mapping
from dataclasses import dataclass
from typing import Any

from . import _native
from .errors import Alpha2EditError

__all__ = ["Edited", "apply"]


@dataclass(frozen=True)
class Edited:
    """An operation's result.

    ``project_json`` is the engine's canonical text of ``project``, byte for byte what every
    surface answers. ``report`` has ``removed`` (old timeline ranges), ``moved`` (``from`` and
    ``to`` ranges) and ``created`` (clip ids).
    """

    project: dict[str, Any]
    project_json: str
    report: dict[str, Any]


def _text(value: Mapping[str, Any] | str) -> str:
    return value if isinstance(value, str) else json.dumps(value)


def apply(project: Mapping[str, Any] | str, request: Mapping[str, Any] | str) -> Edited:
    """``request`` applied to ``project``; each is a dict or JSON text, and neither is changed.

    Raises :class:`alpha2.Alpha2EditError` for an invalid project or request and for every refusal.
    """
    try:
        project_json, report = _native.apply_edit(_text(project), _text(request))
    except ValueError as error:
        message, code, refusal = error.args
        raise Alpha2EditError(message, code, refusal) from None
    return Edited(json.loads(project_json), project_json, json.loads(report))
