"""Helpers shared by the build tests."""

from __future__ import annotations

import hashlib
import os
import shutil
import subprocess
import sys
from pathlib import Path

from golden_compare import SOURCE, read_tree

from modsys.build import MARKER_NAME


def hash_tree(root: Path) -> dict[str, str]:
    return {
        p.relative_to(root).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest()
        for p in sorted(root.rglob("*"))
        if p.is_file()
    }


def read_output(out: Path) -> dict[str, bytes]:
    """The published export files, without the provenance marker."""
    return read_tree(out, exclude=frozenset({MARKER_NAME}))


def copy_source(dest: Path) -> Path:
    shutil.copytree(SOURCE, dest, ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
    return dest


def edit_once(path: Path, old: str, new: str) -> None:
    """Replace exactly one occurrence of old (files are read as latin-1 to keep bytes)."""
    text = path.read_bytes().decode("latin-1")
    assert text.count(old) == 1, f"{path.name}: expected exactly one {old!r}"
    path.write_bytes(text.replace(old, new).encode("latin-1"))


def run_cli(
    *args: str | os.PathLike[str],
    cwd: Path,
    env: dict[str, str] | None = None,
    module: str = "modsys.build",
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, "-m", module, *map(str, args)],
        cwd=cwd,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
