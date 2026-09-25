"""Byte-exact comparison of the build against the Python 2.7 reference build."""

from __future__ import annotations

from collections.abc import Callable

import pytest
from golden_compare import (
    GOLDEN_EXPORT,
    GOLDEN_IDS,
    SOURCE,
    assert_same_tree,
    check_manifest,
    compare_trees,
    read_tree,
)


def test_manifest_self_check() -> None:
    assert check_manifest() == []


def test_export_matches_golden(golden_manifest_ok, vanilla) -> None:
    assert_same_tree(read_tree(GOLDEN_EXPORT), vanilla.files)


def test_built_ids_match_golden(golden_manifest_ok, vanilla) -> None:
    assert_same_tree(read_tree(GOLDEN_IDS), vanilla.result.ids)


def test_source_ids_match_golden(golden_manifest_ok) -> None:
    source_ids = {p.name: p.read_bytes() for p in sorted(SOURCE.glob("ID_*.py"))}
    assert_same_tree(read_tree(GOLDEN_IDS), source_ids)


# --- negative controls: the comparator must catch each of these -------------------------


def _flip_byte(tree: dict[str, bytes]) -> tuple[str, int]:
    data = bytearray(tree["troops.txt"])
    data[1000] ^= 0x01
    tree["troops.txt"] = bytes(data)
    return "troops.txt", 1000


def _lf_to_crlf(tree: dict[str, bytes]) -> tuple[str, int]:
    data = tree["scripts.txt"]
    tree["scripts.txt"] = data.replace(b"\n", b"\r\n")
    return "scripts.txt", data.index(b"\n")


def _cp1252_dash_to_utf8(tree: dict[str, bytes]) -> tuple[str, int]:
    data = tree["strings.txt"]
    offset = data.index(b"\x97")
    tree["strings.txt"] = data[:offset] + b"\xe2\x80\x94" + data[offset + 1 :]
    return "strings.txt", offset


@pytest.mark.parametrize(
    "mutate",
    [_flip_byte, _lf_to_crlf, _cp1252_dash_to_utf8],
    ids=["flipped-byte", "lf-to-crlf", "0x97-to-utf8"],
)
def test_comparator_reports_byte_difference(
    golden_manifest_ok, mutate: Callable[[dict[str, bytes]], tuple[str, int]]
) -> None:
    golden = read_tree(GOLDEN_EXPORT)
    mutated = dict(golden)
    name, offset = mutate(mutated)
    problems = compare_trees(golden, mutated)
    assert len(problems) == 1, problems
    assert problems[0].startswith(f"{name}: first differing byte at offset {offset} ")


def test_comparator_reports_missing_file(golden_manifest_ok) -> None:
    golden = read_tree(GOLDEN_EXPORT)
    mutated = dict(golden)
    del mutated["skills.txt"]
    assert compare_trees(golden, mutated) == ["missing file: skills.txt"]


def test_comparator_reports_extra_file(golden_manifest_ok) -> None:
    golden = read_tree(GOLDEN_EXPORT)
    mutated = dict(golden, **{"extra.txt": b""})
    assert compare_trees(golden, mutated) == ["unexpected extra file: extra.txt"]


def test_comparator_accepts_identical_trees(golden_manifest_ok) -> None:
    assert compare_trees(read_tree(GOLDEN_EXPORT), read_tree(GOLDEN_EXPORT)) == []
