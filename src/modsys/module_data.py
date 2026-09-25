"""Regenerate the Module_data files (flora kinds, ground specs, skyboxes).

Runs ``Flora_kinds.py``, ``Ground_specs.py`` and ``Skyboxes.py`` from a temporary copy of
``game/module_data`` (each in its own interpreter, like the build stages), with an empty
sibling ``Module_system`` directory as the sink for ``Ground_specs.py``'s
``../Module_system/header_ground_types.py`` write, and publishes the generated files to
the output dir. Nothing is written into ``game/`` or into a game install.
"""

from __future__ import annotations

import argparse
import shutil
import sys
import tempfile
from collections.abc import Sequence
from pathlib import Path

from modsys.build import (
    GAME_DIR,
    REPO_ROOT,
    BuildError,
    GuardError,
    PassResult,
    _atomic_write,
    _is_within,
    run_stage,
    stage_env,
)

DEFAULT_SOURCE_DIR = GAME_DIR / "module_data"
DEFAULT_OUTPUT_DIR = REPO_ROOT / "build" / "module_data"
SCRIPTS = ("Flora_kinds.py", "Ground_specs.py", "Skyboxes.py")
DATA_OUTPUTS = ("flora_kinds.txt", "ground_specs.txt", "skyboxes.txt", "ground_spec_codes.h")
SINK_OUTPUTS = ("header_ground_types.py",)


def check_output_dir(output_dir: Path) -> Path:
    resolved = output_dir.expanduser().absolute().resolve()
    if _is_within(resolved, GAME_DIR.resolve()):
        raise GuardError(f"refusing to write inside the game sources: {resolved}")
    if "modules" in (p.lower() for p in resolved.parts):
        raise GuardError(f"refusing to write into what looks like a game install: {resolved}")
    if resolved.exists() and not resolved.is_dir():
        raise GuardError(f"output path exists and is not a directory: {resolved}")
    return resolved


def build_data(
    source_dir: Path = DEFAULT_SOURCE_DIR,
    output_dir: Path = DEFAULT_OUTPUT_DIR,
    *,
    quiet: bool = False,
) -> dict[str, bytes]:
    """Generate the Module_data files and publish them; return name -> bytes."""
    source_dir = source_dir.expanduser().absolute().resolve()
    missing = [s for s in SCRIPTS if not (source_dir / s).is_file()]
    if missing:
        raise GuardError(f"{source_dir} lacks {', '.join(missing)}")
    out = check_output_dir(output_dir)

    with tempfile.TemporaryDirectory(prefix="modsys-data-") as tmp:
        data_dir = Path(tmp) / "Module_data"
        sink_dir = Path(tmp) / "Module_system"
        shutil.copytree(source_dir, data_dir, ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
        sink_dir.mkdir()
        for name in DATA_OUTPUTS:
            (data_dir / name).unlink(missing_ok=True)
        env = stage_env(None)
        stages = [run_stage(script, data_dir, env) for script in SCRIPTS]
        result = PassResult(1, stages, [])
        if result.failed:
            raise BuildError("Module_data script(s) failed", [result])
        produced = {name: data_dir / name for name in DATA_OUTPUTS}
        produced |= {name: sink_dir / name for name in SINK_OUTPUTS}
        absent = [name for name, path in produced.items() if not path.is_file()]
        if absent:
            raise BuildError(f"expected output(s) not produced: {', '.join(absent)}", [result])
        files = {name: path.read_bytes() for name, path in produced.items()}

    out.mkdir(parents=True, exist_ok=True)
    for name, data in files.items():
        _atomic_write(out / name, data)
    if not quiet:
        print(f"published {len(files)} files to {out}: {', '.join(files)}")
    return files


def main(argv: Sequence[str] | None = None) -> int:
    p = argparse.ArgumentParser(
        prog="warband-build-data",
        description="Regenerate flora_kinds.txt, ground_specs.txt, skyboxes.txt and friends.",
    )
    p.add_argument(
        "-o",
        "--output-dir",
        type=Path,
        default=DEFAULT_OUTPUT_DIR,
        help=f"where to publish the files (default: {DEFAULT_OUTPUT_DIR})",
    )
    p.add_argument(
        "--source-dir",
        type=Path,
        default=DEFAULT_SOURCE_DIR,
        help=f"Module_data source tree (default: {DEFAULT_SOURCE_DIR})",
    )
    p.add_argument("-q", "--quiet", action="store_true", help="only print problems")
    args = p.parse_args(argv)
    try:
        build_data(args.source_dir, args.output_dir, quiet=args.quiet)
    except GuardError as exc:
        print(f"warband-build-data: error: {exc}", file=sys.stderr)
        return 2
    except BuildError as exc:
        print(exc.report(), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
