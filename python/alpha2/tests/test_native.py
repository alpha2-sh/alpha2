from __future__ import annotations

from conftest import require_or_skip

import alpha2
from alpha2 import _native


def test_a_panic_in_a_native_call_is_an_ordinary_exception() -> None:
    panic = getattr(_native, "_panic", None)
    if panic is None:
        require_or_skip("native-test-hooks", "the native module is a release build")
    try:
        panic("attempt to negate with overflow")
    except Exception as error:  # noqa: BLE001 - what a service's request handler catches
        caught = error
    assert isinstance(caught, alpha2.Alpha2InternalError)
    assert isinstance(caught, RuntimeError) and not isinstance(caught, ValueError)
    assert caught.code == "INTERNAL"
    assert "attempt to negate with overflow" in str(caught)
    assert alpha2.Alpha2InternalError is _native.InternalError
