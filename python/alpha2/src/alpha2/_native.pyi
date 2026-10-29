"""The compiled part of the package (bindings/alpha2-python): the engine's own answers."""

__version__: str
SCHEMA_VERSION: int
MAX_PROJECT_JSON_BYTES: int
TICKS_PER_SECOND: int
COMPOSE_VERSION: int
COMPOSE_THEMES: list[str]

def validate_project_json(json: str) -> list[str]:
    """The problems the engine finds in a project document, as JSON text: empty when it is valid."""

def project_end_ticks(json: str) -> int | None:
    """Where a whole-project export ends, in ticks, or None for a timeline that shows nothing.

    Raises ValueError for a document that is not a valid project.
    """

def lint_project_json(json: str) -> str:
    """A valid project's non-fatal diagnostics, as a JSON array of issues.

    Raises ValueError for a document that is not a valid project.
    """

def resolve_export_format(
    container: str | None = None,
    extension: str | None = None,
    video_codec: str | None = None,
    audio_codec: str | None = None,
) -> tuple[str, str, str]:
    """An export's container, video codec and audio codec, as the host resolves them.

    Raises ValueError with the engine's message for a request it refuses.
    """

def compose_requirements(spec: str) -> str:
    """The files a compose spec (JSON text) names, as JSON text: ``{"media", "texts"}``.

    Raises ValueError(message, path, problem) for a spec that does not parse.
    """

def compose(spec: str, facts: str, fonts: list[bytes]) -> str:
    """A compose spec compiled with facts about its files and the export's font files, as JSON
    text: ``{"project", "assets", "loudness", "warnings"}``.

    Raises ValueError(message, path, problem) for a spec that does not compile.
    """

class TextMeasurer:
    """A text and layout measurer over the export's font files, registered as the host registers
    them (every face of a collection, in order)."""

    def __init__(self, fonts: list[str]) -> None:
        """Raises FileNotFoundError naming a missing file, OSError for another read failure and
        ValueError naming a file with no face drawn."""

    def text(self, generator: str, width: int, height: int) -> str:
        """Where a text generator (JSON text) lies on a canvas, as JSON text
        ``{layout, content, ink, lineCount, baselines}``. Raises ValueError for one it refuses."""

    def layout(
        self,
        generator: str,
        width: int,
        height: int,
        time: int,
        canvas: tuple[int, int] | None = None,
    ) -> str:
        """Where each node of a layout generator (JSON text) lies on a picture at picture time
        ``time`` (ticks), as JSON text ``{nodes}``. Raises ValueError for one it refuses."""
