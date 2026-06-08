"""Tests for ilink_bridge SDK."""
import json
import tempfile
from pathlib import Path

import pytest

from ilink_bridge import (
    HistoryEntry,
    append_history,
    load_history,
    session_file_path,
)


def test_session_file_path_default(monkeypatch, tmp_path):
    monkeypatch.setenv("HOME", str(tmp_path))
    # Re-import to pick up patched HOME
    from ilink_bridge import session_file_path as sfp
    p = sfp("abc-123")
    assert p == tmp_path / ".ilink-hub" / "sessions" / "abc-123.jsonl"


def test_session_file_path_custom():
    p = session_file_path("abc-123", Path("/tmp/custom"))
    assert p == Path("/tmp/custom/abc-123.jsonl")


def test_load_history_missing_file(tmp_path):
    result = load_history("nonexistent", tmp_path)
    assert result == []


def test_round_trip_history(tmp_path):
    sid = "test-session-roundtrip"
    entries = [
        HistoryEntry(role="user", content="hello", ts="2026-01-01T00:00:00+00:00"),
        HistoryEntry(role="assistant", content="hi there", ts="2026-01-01T00:00:01+00:00"),
    ]
    append_history(sid, entries, tmp_path)

    loaded = load_history(sid, tmp_path)
    assert len(loaded) == 2
    assert loaded[0].role == "user"
    assert loaded[0].content == "hello"
    assert loaded[1].role == "assistant"
    assert loaded[1].content == "hi there"


def test_append_multiple_calls(tmp_path):
    sid = "test-session-multi"
    append_history(sid, [HistoryEntry(role="user", content="msg1")], tmp_path)
    append_history(sid, [HistoryEntry(role="assistant", content="reply1")], tmp_path)

    loaded = load_history(sid, tmp_path)
    assert len(loaded) == 2
    assert loaded[0].content == "msg1"
    assert loaded[1].content == "reply1"


def test_jsonl_format(tmp_path):
    """Each line must be valid JSON."""
    sid = "test-jsonl-format"
    append_history(sid, [HistoryEntry(role="user", content="test")], tmp_path)

    file_content = session_file_path(sid, tmp_path).read_text()
    for line in file_content.strip().splitlines():
        obj = json.loads(line)
        assert "role" in obj
        assert "content" in obj
        assert "ts" in obj
