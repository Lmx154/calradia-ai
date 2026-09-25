"""Helpers shared by the build tests."""

from __future__ import annotations

import hashlib
import importlib.util
import os
import shutil
import subprocess
import sys
from pathlib import Path
from types import ModuleType

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


def load_local_check() -> ModuleType:
    """tools/calradia_check.py, the desktop agent's check tool (not a package)."""
    path = Path(__file__).resolve().parents[1] / "tools" / "calradia_check.py"
    spec = importlib.util.spec_from_file_location("calradia_check", path)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules["calradia_check"] = module  # dataclasses resolve annotations through it
    spec.loader.exec_module(module)
    return module
