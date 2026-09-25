"""Golden reference locations and an exact byte-for-byte tree comparator."""

from __future__ import annotations

import hashlib
import json
from collections.abc import Mapping
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
GAME = REPO / "game"
SOURCE = GAME / "module_system"
MODULE_DATA_SOURCE = GAME / "module_data"
GOLDEN = REPO / "tests" / "golden"
GOLDEN_EXPORT = GOLDEN / "module_system_1171" / "export"
GOLDEN_IDS = GOLDEN / "module_system_1171" / "ids"
GOLDEN_DATA = GOLDEN / "module_data_1171"
MANIFEST = GOLDEN / "MANIFEST.json"
MANIFEST_UNLISTED = frozenset({"MANIFEST.json", "reference_build.log", "stages.txt"})


def read_tree(
    root: Path, pattern: str = "*", exclude: frozenset[str] = frozenset()
) -> dict[str, bytes]:
    """Map relative posix path -> bytes for every file under root matching pattern."""
    return {
        p.relative_to(root).as_posix(): p.read_bytes()
        for p in sorted(root.rglob(pattern))
        if p.is_file() and p.name not in exclude
    }


def first_difference(expected: bytes, actual: bytes) -> int | None:
    if expected == actual:
        return None
    n = min(len(expected), len(actual))
    lo, hi = 0, n
    # binary search on prefix equality: fast even for multi-megabyte files
    while lo < hi:
        mid = (lo + hi) // 2
        if expected[lo : mid + 1] == actual[lo : mid + 1]:
            lo = mid + 1
        else:
            hi = mid
    return lo


def _byte_at(data: bytes, offset: int) -> str:
    return f"0x{data[offset]:02x}" if offset < len(data) else "EOF"


def compare_trees(expected: Mapping[str, bytes], actual: Mapping[str, bytes]) -> list[str]:
    """Return one message per problem; empty means identical name sets and bytes."""
    problems = [f"missing file: {name}" for name in sorted(set(expected) - set(actual))]
    problems += [f"unexpected extra file: {name}" for name in sorted(set(actual) - set(expected))]
    for name in sorted(set(expected) & set(actual)):
        exp, act = expected[name], actual[name]
        offset = first_difference(exp, act)
        if offset is not None:
            problems.append(
                f"{name}: first differing byte at offset {offset} "
                f"(expected {_byte_at(exp, offset)}, got {_byte_at(act, offset)}; "
                f"sizes {len(exp)} vs {len(act)})"
            )
    return problems


def assert_same_tree(expected: Mapping[str, bytes], actual: Mapping[str, bytes]) -> None:
    problems = compare_trees(expected, actual)
    assert not problems, "trees differ:\n" + "\n".join(problems)


def check_manifest() -> list[str]:
    """Verify every golden file against MANIFEST.json; return problems."""
    listed = json.loads(MANIFEST.read_text())["files"]
    present = read_tree(GOLDEN, exclude=MANIFEST_UNLISTED)
    problems = [f"listed but missing: {name}" for name in sorted(set(listed) - set(present))]
    problems += [f"present but unlisted: {name}" for name in sorted(set(present) - set(listed))]
    for name in sorted(set(listed) & set(present)):
        data = present[name]
        if len(data) != listed[name]["size"]:
            problems.append(f"{name}: size {len(data)} != manifest {listed[name]['size']}")
        if hashlib.sha256(data).hexdigest() != listed[name]["sha256"]:
            problems.append(f"{name}: sha256 differs from manifest")
    return problems
