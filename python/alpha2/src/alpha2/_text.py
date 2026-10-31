"""Decodes user text files as ``alpha2-host`` does: UTF-8 (BOM dropped), or UTF-16 with a BOM
(as Windows PowerShell 5.1 writes). Legacy code pages are refused rather than guessed.
"""

from __future__ import annotations

import codecs
import os


class UserTextError(ValueError):
    """A file that is not text by the rule; the message names the file and what to do."""


def read_user_text(path: str | os.PathLike[str], what: str) -> str:
    """The text of the file at ``path``, which a message calls ``what``.

    Raises :class:`OSError` when the file cannot be read and :class:`UserTextError` naming the
    first offending byte when it is not text by the rule.
    """
    with open(path, "rb") as file:
        data = file.read()
    try:
        return decode_user_text(data)
    except UnicodeDecodeError as error:
        raise UserTextError(
            f"{what} {os.fspath(path)} is not UTF-8 text (invalid byte at offset "
            f"{error.start}); save it as UTF-8"
        ) from None


def decode_user_text(data: bytes) -> str:
    """Raises :class:`UnicodeDecodeError` with ``start`` counted from the start of ``data``."""
    for mark, encoding in (
        (codecs.BOM_UTF8, "utf-8"),
        (codecs.BOM_UTF16_LE, "utf-16-le"),
        (codecs.BOM_UTF16_BE, "utf-16-be"),
    ):
        if data.startswith(mark):
            try:
                return data[len(mark) :].decode(encoding)
            except UnicodeDecodeError as error:
                raise UnicodeDecodeError(
                    error.encoding,
                    data,
                    error.start + len(mark),
                    error.end + len(mark),
                    error.reason,
                ) from None
    return data.decode("utf-8")
