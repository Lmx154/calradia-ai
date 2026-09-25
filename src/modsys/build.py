"""Build driver for the Warband Module System (replacement for ``build_module.bat``).

The legacy compiler is a set of flat ``process_*.py`` scripts that do all their work at
import time. The stage list is read, in order, from the ``python process_*.py`` lines of
``build_module.bat`` in the source tree; ``process_line_correction.py`` and
``process_tags_unused.py`` are never run.

Every stage runs in its own interpreter process, never in-process, because:

* the stages do their work at import time, so importing one twice does nothing;
* ``from ID_x import *`` binds the numeric IDs at import time, and the ``ID_*.py`` files
  written by earlier stages must be imported fresh by later stages;
* the ID files form a bootstrap cycle (for example ``module_constants`` imports
  ``ID_items`` while ``module_items`` imports ``module_constants``), so a change to the
  module files may need more than one full pass before the IDs are stable.

Each build copies the source tree into a temporary work directory (the source tree is
never written, except by ``--sync-ids``) and exports into a staging directory inside it.
A pass runs every stage in order and, like the ``.bat``, continues past a failed stage so
that later ID generators can still repair stale IDs. A pass is accepted only if every
stage exits 0, no output line starts with ``ERROR``/``Error``, and the ``ID_*.py`` bytes
did not change during the pass. If a pass is not accepted but the IDs changed, another
pass is run (at most ``MAX_PASSES``); if the IDs did not change, a rerun would repeat the
same failure, so the build stops. Only an accepted pass is published to the output dir.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Callable, Iterator, Mapping, Sequence
from contextlib import contextmanager
from dataclasses import dataclass, field
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
GAME_DIR = REPO_ROOT / "game"
DEFAULT_SOURCE_DIR = GAME_DIR / "module_system"
DEFAULT_OUTPUT_DIR = REPO_ROOT / "build" / "Native"

BAT_NAME = "build_module.bat"
MARKER_NAME = ".modsys-build.json"
EXPORT_ENV_VAR = "WARBAND_EXPORT_DIR"
EXCLUDED_STAGES = frozenset({"process_line_correction.py", "process_tags_unused.py"})
STRIPPED_ENV_VARS = ("PYTHONPATH", "PYTHONSAFEPATH", "PYTHONHOME", "PYTHONSTARTUP")
MAX_PASSES = 24
TAIL_LINES = 12

_STAGE_LINE = re.compile(r"^\s*python\s+(process_\w+\.py)\s*$", re.IGNORECASE)
_ERROR_LINE = re.compile(r"^(ERROR|Error)")
_WARNING_LINE = re.compile(r"^(WARNING|Warning)")

Log = Callable[[str], None]


class GuardError(Exception):
    """Refused invocation (bad paths, unsafe output dir). Exit status 2."""


@dataclass
class StageResult:
    script: str
    returncode: int
    stdout: str
    stderr: str

    @property
    def lines(self) -> list[str]:
        return self.stdout.splitlines() + self.stderr.splitlines()

    @property
    def error_lines(self) -> list[str]:
        return [line for line in self.lines if _ERROR_LINE.match(line)]

    @property
    def warning_lines(self) -> list[str]:
        return [line for line in self.lines if _WARNING_LINE.match(line)]

    @property
    def ok(self) -> bool:
        return self.returncode == 0 and not self.error_lines

    def tail(self, n: int = TAIL_LINES) -> list[str]:
        return self.lines[-n:]


@dataclass
class PassResult:
    number: int
    stages: list[StageResult]
    ids_changed: list[str]

    @property
    def failed(self) -> list[StageResult]:
        return [s for s in self.stages if not s.ok]

    @property
    def accepted(self) -> bool:
        return not self.failed and not self.ids_changed


class BuildError(Exception):
    """The module did not compile. Exit status 1."""

    def __init__(self, reason: str, passes: list[PassResult]) -> None:
        super().__init__(reason)
        self.reason = reason
        self.pass_results = passes

    @property
    def passes(self) -> int:
        return len(self.pass_results)

    def report(self) -> str:
        out = [f"build failed after {self.passes} pass(es): {self.reason}"]
        last = self.pass_results[-1] if self.pass_results else None
        for i, stage in enumerate(last.failed if last else []):
            out.append(f"--- stage {stage.script} failed (exit status {stage.returncode})")
            for line in stage.error_lines:
                out.append(f"    error line: {line}")
            out.append("    output tail:")
            out.extend(f"    | {line}" for line in stage.tail(TAIL_LINES if i == 0 else 1))
        return "\n".join(out)


@dataclass
class BuildResult:
    passes: int
    output_dir: Path
    files: dict[str, str]
    ids: dict[str, bytes]
    warnings: list[str] = field(default_factory=list)
    new: list[str] = field(default_factory=list)
    changed: list[str] = field(default_factory=list)
    unchanged: list[str] = field(default_factory=list)
    stale: list[str] = field(default_factory=list)
    ids_differ_from_source: list[str] = field(default_factory=list)
    synced_ids: list[str] = field(default_factory=list)
    work_dir: Path | None = None


def parse_stages(bat_path: Path) -> list[str]:
    """Return the ``python process_*.py`` stages of ``build_module.bat``, in order."""
    text = bat_path.read_bytes().decode("latin-1")
    stages = []
    for line in text.splitlines():
        m = _STAGE_LINE.match(line)
        if m and m.group(1) not in EXCLUDED_STAGES:
            stages.append(m.group(1))
    return stages


def stage_env(export_dir: Path | None) -> dict[str, str]:
    env = {k: v for k, v in os.environ.items() if k not in STRIPPED_ENV_VARS}
    if export_dir is not None:
        env[EXPORT_ENV_VAR] = export_dir.absolute().as_posix().rstrip("/") + "/"
    return env


def run_stage(script: str, cwd: Path, env: Mapping[str, str]) -> StageResult:
    proc = subprocess.run(
        [sys.executable, "-B", "-W", "error::SyntaxWarning", script],
        cwd=cwd,
        env=dict(env),
        stdin=subprocess.DEVNULL,
        capture_output=True,
        check=False,
    )
    return StageResult(
        script=script,
        returncode=proc.returncode,
        stdout=proc.stdout.decode("utf-8", "backslashreplace"),
        stderr=proc.stderr.decode("utf-8", "backslashreplace"),
    )


def snapshot_ids(directory: Path) -> dict[str, bytes]:
    return {p.name: p.read_bytes() for p in sorted(directory.glob("ID_*.py")) if p.is_file()}


def diff_names(a: Mapping[str, bytes], b: Mapping[str, bytes]) -> list[str]:
    return sorted(name for name in set(a) | set(b) if a.get(name) != b.get(name))


def _is_within(path: Path, parent: Path) -> bool:
    return path == parent or path.is_relative_to(parent)


def _is_native(path: Path) -> bool:
    return [p.lower() for p in path.parts[-2:]] == ["modules", "native"]


def check_output_dir(
    output_dir: Path,
    source_dir: Path,
    *,
    allow_native: bool = False,
    force: bool = False,
) -> Path:
    """Apply the output guards; return the resolved output directory."""
    absolute = output_dir.expanduser().absolute()
    resolved = absolute.resolve()
    if not allow_native and (_is_native(absolute) or _is_native(resolved)):
        raise GuardError(f"refusing to write into a game install ({absolute}); pass --allow-native")
    if _is_within(resolved, GAME_DIR.resolve()):
        raise GuardError(f"refusing to write inside the game sources: {resolved}")
    if _is_within(resolved, source_dir.resolve()):
        raise GuardError(f"refusing to write inside the source dir: {resolved}")
    if resolved.exists():
        if not resolved.is_dir():
            raise GuardError(f"output path exists and is not a directory: {resolved}")
        if any(resolved.iterdir()) and not (resolved / MARKER_NAME).is_file() and not force:
            raise GuardError(
                f"output dir {resolved} is not empty and has no {MARKER_NAME} "
                "(not created by this tool); pass --force to write into it anyway"
            )
    return resolved


@contextmanager
def work_tree(source_dir: Path, keep: bool) -> Iterator[Path]:
    root = Path(tempfile.mkdtemp(prefix="modsys-build-"))
    try:
        work = root / "work"
        shutil.copytree(source_dir, work, ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
        yield work
    finally:
        if not keep:
            shutil.rmtree(root, ignore_errors=True)


def _git_provenance(source_dir: Path) -> dict[str, object]:
    def git(*args: str) -> str | None:
        try:
            proc = subprocess.run(
                ["git", "-C", str(source_dir), *args],
                capture_output=True,
                text=True,
                check=False,
            )
        except OSError:
            return None
        return proc.stdout.strip() if proc.returncode == 0 else None

    commit = git("rev-parse", "HEAD")
    status = git("status", "--porcelain", "--", ".") if commit else None
    return {
        "source_commit": commit,
        "source_dirty": None if status is None else bool(status),
    }


def _atomic_write(path: Path, data: bytes) -> None:
    tmp = path.with_name(f".{path.name}.modsys-tmp")
    try:
        tmp.write_bytes(data)
        os.replace(tmp, path)
    finally:
        tmp.unlink(missing_ok=True)


def _run_passes(work: Path, stages: Sequence[str], log: Log) -> list[PassResult]:
    staging = work / "export"
    env = stage_env(staging)
    results: list[PassResult] = []
    for number in range(1, MAX_PASSES + 1):
        if staging.exists():
            shutil.rmtree(staging)
        staging.mkdir()
        before = snapshot_ids(work)
        stage_results = []
        for script in stages:
            stage_results.append(run_stage(script, work, env))
        result = PassResult(number, stage_results, diff_names(before, snapshot_ids(work)))
        results.append(result)
        failed = ", ".join(s.script for s in result.failed) or "none"
        changed = ", ".join(result.ids_changed) or "none"
        log(f"pass {number}: failed stages: {failed}; ID files changed: {changed}")
        if result.accepted:
            return results
        if not result.ids_changed:
            raise BuildError(
                "stage(s) failed and no ID file changed during the pass, "
                "so another pass cannot help",
                results,
            )
    raise BuildError(f"ID files did not reach a fixpoint within {MAX_PASSES} passes", results)


def build(
    source_dir: Path = DEFAULT_SOURCE_DIR,
    output_dir: Path = DEFAULT_OUTPUT_DIR,
    *,
    sync_ids: bool = False,
    allow_native: bool = False,
    force: bool = False,
    keep_work: bool = False,
    quiet: bool = False,
) -> BuildResult:
    """Compile the module in ``source_dir`` and publish the export files to ``output_dir``.

    Raises GuardError for refused invocations and BuildError if no pass is accepted.
    """

    def log(msg: str) -> None:
        if not quiet:
            print(msg, flush=True)

    source_dir = source_dir.expanduser().absolute().resolve()
    bat = source_dir / BAT_NAME
    if not bat.is_file():
        raise GuardError(f"{bat} not found (is --source-dir a Module System tree?)")
    out = check_output_dir(output_dir, source_dir, allow_native=allow_native, force=force)
    stages = parse_stages(bat)
    if not stages:
        raise GuardError(f"no process_*.py stages found in {bat}")

    with work_tree(source_dir, keep_work) as work:
        if keep_work:
            log(f"work dir: {work}")
        log(f"building {source_dir} ({len(stages)} stages)")
        pass_results = _run_passes(work, stages, log)
        staging = work / "export"
        exports = {p.name: p.read_bytes() for p in sorted(staging.iterdir()) if p.is_file()}
        final_ids = snapshot_ids(work)

    warnings = [
        f"{stage.script}: {line}"
        for stage in pass_results[-1].stages
        for line in stage.warning_lines
    ]
    for w in warnings:
        print(f"warning: {w}", file=sys.stderr)

    result = BuildResult(
        passes=len(pass_results),
        output_dir=out,
        files={name: hashlib.sha256(data).hexdigest() for name, data in exports.items()},
        ids=final_ids,
        warnings=warnings,
        work_dir=work if keep_work else None,
    )

    out.mkdir(parents=True, exist_ok=True)
    for name, data in exports.items():
        target = out / name
        if not target.exists():
            result.new.append(name)
        elif target.read_bytes() != data:
            result.changed.append(name)
        else:
            result.unchanged.append(name)
            continue
        _atomic_write(target, data)
    result.stale = sorted(
        p.name
        for p in out.iterdir()
        if p.is_file() and p.name != MARKER_NAME and p.name not in exports
    )
    marker = {
        "generator": "modsys.build",
        "source_dir": str(source_dir),
        **_git_provenance(source_dir),
        "python": sys.version,
        "passes": result.passes,
        "stages": stages,
        "files": result.files,
    }
    _atomic_write(out / MARKER_NAME, (json.dumps(marker, indent=2, sort_keys=True) + "\n").encode())

    log(
        f"accepted after {result.passes} pass(es); published {len(exports)} files to {out}: "
        f"{len(result.new)} new, {len(result.changed)} changed, "
        f"{len(result.unchanged)} unchanged"
    )
    for name in result.new:
        log(f"  new:     {name}")
    for name in result.changed:
        log(f"  changed: {name}")
    if result.stale:
        log(f"  not produced by this build (left in place): {', '.join(result.stale)}")

    source_ids = snapshot_ids(source_dir)
    result.ids_differ_from_source = diff_names(source_ids, final_ids)
    if sync_ids:
        for name in result.ids_differ_from_source:
            if name in final_ids:
                _atomic_write(source_dir / name, final_ids[name])
                result.synced_ids.append(name)
        if result.synced_ids:
            log(f"synced ID files into {source_dir}: {', '.join(result.synced_ids)}")
        else:
            log("ID files in the source dir are already up to date")
    elif result.ids_differ_from_source:
        print(
            "notice: regenerated ID files differ from the source dir: "
            f"{', '.join(result.ids_differ_from_source)} (rerun with --sync-ids to update them)",
            file=sys.stderr,
        )
    return result


def make_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="warband-build",
        description="Compile the Warband Module System into game text files.",
    )
    p.add_argument(
        "-o",
        "--output-dir",
        type=Path,
        default=DEFAULT_OUTPUT_DIR,
        help=f"where to publish the export files (default: {DEFAULT_OUTPUT_DIR})",
    )
    p.add_argument(
        "--source-dir",
        type=Path,
        default=DEFAULT_SOURCE_DIR,
        help=f"Module System source tree (default: {DEFAULT_SOURCE_DIR})",
    )
    p.add_argument(
        "--sync-ids",
        action="store_true",
        help="copy regenerated ID_*.py files back into the source dir",
    )
    p.add_argument(
        "--allow-native",
        action="store_true",
        help="allow an output dir ending in Modules/Native",
    )
    p.add_argument(
        "--force",
        action="store_true",
        help="allow a non-empty output dir that was not created by this tool",
    )
    p.add_argument("--keep-work", action="store_true", help="keep the temporary work dir")
    p.add_argument("-q", "--quiet", action="store_true", help="only print problems")
    return p


def main(argv: Sequence[str] | None = None) -> int:
    args = make_parser().parse_args(argv)
    try:
        build(
            args.source_dir,
            args.output_dir,
            sync_ids=args.sync_ids,
            allow_native=args.allow_native,
            force=args.force,
            keep_work=args.keep_work,
            quiet=args.quiet,
        )
    except GuardError as exc:
        print(f"warband-build: error: {exc}", file=sys.stderr)
        return 2
    except BuildError as exc:
        print(exc.report(), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
