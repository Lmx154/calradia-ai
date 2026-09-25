"""Driver semantics on tiny fake Module System trees (independent of the real game)."""

from __future__ import annotations

import json
import shutil
import textwrap
from pathlib import Path

import pytest
from golden_compare import GAME
from support import hash_tree

from modsys import build as mb
from modsys.build import BuildError, GuardError, build, main, parse_stages

PRELUDE = "import os, sys\nEXPORT = os.environ['WARBAND_EXPORT_DIR']\n"

# Writes ID_x.py from module_x.VALUE (an "ID generator").
GEN_IDS = """
from module_x import VALUE
with open("ID_x.py", "w") as f:
    f.write(f"x = {VALUE}\\n")
"""
# Exports the currently bound ID (a consumer that binds IDs at import time).
USE_IDS = """
from ID_x import x
with open(EXPORT + "use.txt", "w") as f:
    f.write(f"x={x}\\n")
"""
# Fails unless the ID file agrees with the module (like a NameError on a stale ID).
CHECK_IDS = """
from ID_x import x
from module_x import VALUE
sys.exit(0 if x == VALUE else 3)
"""


def make_source(
    root: Path,
    scripts: dict[str, str],
    *,
    value: int = 1,
    ids: int | None = None,
    bat_extra: str = "",
) -> Path:
    root.mkdir(parents=True)
    bat = "@echo off\r\n" + "".join(f"python {name}\r\n" for name in scripts)
    bat += bat_extra + "@del *.pyc\r\necho.\r\npause>nul"
    (root / "build_module.bat").write_bytes(bat.encode())
    for name, body in scripts.items():
        (root / name).write_text(PRELUDE + textwrap.dedent(body))
    (root / "module_x.py").write_text(f"VALUE = {value}\n")
    (root / "ID_x.py").write_text(f"x = {value if ids is None else ids}\n")
    return root


@pytest.fixture
def basic(tmp_path: Path) -> Path:
    return make_source(
        tmp_path / "src", {"process_use.py": USE_IDS, "process_ids.py": GEN_IDS}, value=1
    )


@pytest.fixture
def stale(tmp_path: Path) -> Path:
    return make_source(
        tmp_path / "src", {"process_use.py": USE_IDS, "process_ids.py": GEN_IDS}, value=1, ids=0
    )


def files_of(out: Path) -> dict[str, bytes]:
    return {p.name: p.read_bytes() for p in sorted(out.iterdir())}


# --- passes and acceptance ----------------------------------------------------------------


def test_clean_source_accepted_in_one_pass(basic: Path, tmp_path: Path) -> None:
    before = hash_tree(basic)
    result = build(basic, tmp_path / "out", quiet=True)
    assert result.passes == 1
    assert (tmp_path / "out" / "use.txt").read_text() == "x=1\n"
    assert result.ids == {"ID_x.py": b"x = 1\n"}
    assert result.ids_differ_from_source == []
    assert hash_tree(basic) == before


def test_stale_ids_reach_fixpoint_in_two_passes(
    stale: Path, tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    before = hash_tree(stale)
    result = build(stale, tmp_path / "out", quiet=True)
    assert result.passes == 2
    assert (tmp_path / "out" / "use.txt").read_text() == "x=1\n"
    assert result.ids_differ_from_source == ["ID_x.py"]
    assert result.synced_ids == []
    assert hash_tree(stale) == before
    assert "ID_x.py" in capsys.readouterr().err


def test_sync_ids_writes_back_and_next_build_is_one_pass(stale: Path, tmp_path: Path) -> None:
    first = build(stale, tmp_path / "out", sync_ids=True, quiet=True)
    assert first.passes == 2
    assert first.synced_ids == ["ID_x.py"]
    assert (stale / "ID_x.py").read_text() == "x = 1\n"
    second = build(stale, tmp_path / "out", quiet=True)
    assert second.passes == 1
    assert second.unchanged == ["use.txt"]


def test_failed_stage_is_repaired_by_later_generator(tmp_path: Path) -> None:
    src = make_source(
        tmp_path / "src",
        {"process_check.py": CHECK_IDS, "process_ids.py": GEN_IDS, "process_use.py": USE_IDS},
        value=2,
        ids=1,
    )
    result = build(src, tmp_path / "out", quiet=True)
    assert result.passes == 2
    assert (tmp_path / "out" / "use.txt").read_text() == "x=2\n"


def test_failure_without_id_change_stops_after_one_pass(tmp_path: Path) -> None:
    src = make_source(
        tmp_path / "src",
        {
            "process_fail.py": "print('about to fail')\nsys.exit(3)\n",
            "process_after.py": "open(EXPORT + 'after.txt', 'w').write('ran')\n",
        },
    )
    with pytest.raises(BuildError) as info:
        build(src, tmp_path / "out", quiet=True)
    err = info.value
    assert err.passes == 1
    stages = err.pass_results[0].stages
    assert [(s.script, s.returncode) for s in stages] == [
        ("process_fail.py", 3),
        ("process_after.py", 0),
    ]
    report = err.report()
    assert "process_fail.py failed (exit status 3)" in report
    assert "about to fail" in report
    assert "process_after.py" not in report
    assert not (tmp_path / "out").exists()


def test_ids_that_never_settle_hit_the_pass_cap(tmp_path: Path) -> None:
    src = make_source(
        tmp_path / "src",
        {
            "process_bump.py": """
            from ID_x import x
            open("ID_x.py", "w").write(f"x = {x + 1}\\n")
            """
        },
    )
    with pytest.raises(BuildError, match="fixpoint") as info:
        build(src, tmp_path / "out", quiet=True)
    assert info.value.passes == mb.MAX_PASSES == 24
    assert not (tmp_path / "out").exists()


@pytest.mark.parametrize(
    "code",
    [
        "print('Error: bad thing')",
        "print('ERROR: bad thing')",
        "print('Error in trigger:')",
        "sys.stderr.write('Error bad thing\\n')",
    ],
)
def test_error_lines_are_fatal(tmp_path: Path, code: str) -> None:
    src = make_source(tmp_path / "src", {"process_err.py": code + "\n"})
    with pytest.raises(BuildError) as info:
        build(src, tmp_path / "out", quiet=True)
    assert info.value.passes == 1
    assert "process_err.py failed (exit status 0)" in info.value.report()
    assert "error line: " in info.value.report()
    assert not (tmp_path / "out").exists()


@pytest.mark.parametrize(
    "code", ["print('error: lower case')", "print('  Error: indented')", "print('No Error')"]
)
def test_other_error_text_is_not_fatal(tmp_path: Path, code: str) -> None:
    src = make_source(tmp_path / "src", {"process_ok.py": code + "\n"})
    assert build(src, tmp_path / "out", quiet=True).passes == 1


def test_warnings_are_reported_not_fatal(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    src = make_source(
        tmp_path / "src", {"process_warn.py": "print('WARNING: Global variable never used: x')\n"}
    )
    result = build(src, tmp_path / "out", quiet=True)
    assert result.passes == 1
    assert result.warnings == ["process_warn.py: WARNING: Global variable never used: x"]
    assert "WARNING: Global variable never used: x" in capsys.readouterr().err


def test_syntax_warning_is_an_error(tmp_path: Path) -> None:
    src = make_source(tmp_path / "src", {"process_sw.py": 'pattern = "\\d+"\n'})
    assert '"\\d+"' in (src / "process_sw.py").read_text()
    with pytest.raises(BuildError) as info:
        build(src, tmp_path / "out", quiet=True)
    assert "SyntaxError" in info.value.report() or "SyntaxWarning" in info.value.report()


def test_staging_dir_is_emptied_every_pass(tmp_path: Path) -> None:
    src = make_source(
        tmp_path / "src",
        {
            "process_first.py": """
            from ID_x import x
            if x == 0:
                open(EXPORT + "only_with_stale_ids.txt", "w").write("stale")
            open(EXPORT + "always.txt", "w").write("ok")
            """,
            "process_ids.py": GEN_IDS,
        },
        value=1,
        ids=0,
    )
    result = build(src, tmp_path / "out", quiet=True)
    assert result.passes == 2
    assert sorted(result.files) == ["always.txt"]
    assert sorted(files_of(tmp_path / "out")) == [".modsys-build.json", "always.txt"]


# --- stage list and environment -----------------------------------------------------------


def test_parse_stages_reads_bat_lines_in_order(tmp_path: Path) -> None:
    bat = tmp_path / "build_module.bat"
    bat.write_bytes(
        b"@echo off\r\npython process_b.py\r\npython process_line_correction.py\r\n"
        b"rem python process_c.py\r\n  python process_a.py  \r\npython process_tags_unused.py\r\n"
        b"python other.py\r\n@del *.pyc\r\necho.\r\npause>nul"
    )
    assert parse_stages(bat) == ["process_b.py", "process_a.py"]


def test_unused_scripts_listed_in_bat_are_never_run(tmp_path: Path) -> None:
    boom = "open(EXPORT + 'ran.txt', 'w').write('ran')\nsys.exit(1)\n"
    src = make_source(
        tmp_path / "src",
        {"process_use.py": USE_IDS},
        bat_extra="python process_line_correction.py\r\npython process_tags_unused.py\r\n",
    )
    (src / "process_line_correction.py").write_text(PRELUDE + boom)
    (src / "process_tags_unused.py").write_text(PRELUDE + boom)
    result = build(src, tmp_path / "out", quiet=True)
    assert sorted(result.files) == ["use.txt"]


def test_stage_environment(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    decoy = tmp_path / "decoy"
    decoy.mkdir()
    (decoy / "module_x.py").write_text("raise RuntimeError('decoy imported')\n")
    monkeypatch.setenv("PYTHONPATH", str(decoy))
    monkeypatch.setenv("PYTHONSAFEPATH", "1")
    monkeypatch.setenv("PYTHONHOME", str(tmp_path / "no-such-home"))
    monkeypatch.setenv("PYTHONSTARTUP", str(decoy / "module_x.py"))
    monkeypatch.setenv("MODSYS_TEST_PASSTHROUGH", "kept")
    src = make_source(
        tmp_path / "src",
        {
            "process_env.py": """
            import json, module_x
            names = ["PYTHONPATH", "PYTHONSAFEPATH", "PYTHONHOME", "PYTHONSTARTUP",
                     "MODSYS_TEST_PASSTHROUGH"]
            info = {
                "export": EXPORT,
                "env": {k: os.environ.get(k) for k in names},
                "cwd": os.getcwd(),
                "path0": sys.path[0],
                "dont_write_bytecode": sys.flags.dont_write_bytecode,
                "warnoptions": sys.warnoptions,
                "argv": sys.argv,
            }
            open(EXPORT + "env.json", "w").write(json.dumps(info))
            """
        },
    )
    build(src, tmp_path / "out", quiet=True)
    info = json.loads((tmp_path / "out" / "env.json").read_text())
    assert info["export"].endswith("/export/")
    assert Path(info["export"]).is_absolute()
    assert Path(info["export"]).resolve() == Path(info["cwd"]).resolve() / "export"
    assert info["env"] == {
        "PYTHONPATH": None,
        "PYTHONSAFEPATH": None,
        "PYTHONHOME": None,
        "PYTHONSTARTUP": None,
        "MODSYS_TEST_PASSTHROUGH": "kept",
    }
    assert Path(info["cwd"]) != src and not Path(info["cwd"]).is_relative_to(src)
    assert info["path0"] == info["cwd"]
    assert info["dont_write_bytecode"] == 1
    assert "error::SyntaxWarning" in info["warnoptions"]
    assert info["argv"] == ["process_env.py"]
    assert not list(src.rglob("__pycache__"))


# --- publishing ---------------------------------------------------------------------------


def test_marker_and_summary(basic: Path, tmp_path: Path) -> None:
    out = tmp_path / "out"
    first = build(basic, out, quiet=True)
    assert first.new == ["use.txt"] and first.changed == [] and first.unchanged == []
    marker = json.loads((out / mb.MARKER_NAME).read_text())
    assert marker["passes"] == 1
    assert marker["stages"] == ["process_use.py", "process_ids.py"]
    assert marker["files"] == first.files
    assert set(marker) >= {"source_commit", "python", "source_dir"}

    (out / "keep.txt").write_text("user file")
    (basic / "module_x.py").write_text("VALUE = 5\n")
    second = build(basic, out, quiet=True)
    assert second.passes == 2
    assert second.changed == ["use.txt"]
    assert second.stale == ["keep.txt"]
    assert (out / "keep.txt").read_text() == "user file"
    assert (out / "use.txt").read_text() == "x=5\n"
    assert not [p for p in out.iterdir() if "modsys-tmp" in p.name]


def test_failed_build_leaves_output_untouched(basic: Path, tmp_path: Path) -> None:
    out = tmp_path / "out"
    build(basic, out, quiet=True)
    before = {p.name: (p.read_bytes(), p.stat().st_mtime_ns) for p in out.iterdir()}
    (basic / "process_use.py").write_text(PRELUDE + "open(EXPORT+'use.txt','w')\nsys.exit(1)\n")
    with pytest.raises(BuildError):
        build(basic, out, quiet=True)
    assert {p.name: (p.read_bytes(), p.stat().st_mtime_ns) for p in out.iterdir()} == before


def test_keep_work(basic: Path, tmp_path: Path) -> None:
    result = build(basic, tmp_path / "out", keep_work=True, quiet=True)
    try:
        assert result.work_dir is not None
        assert (result.work_dir / "export" / "use.txt").read_text() == "x=1\n"
        assert (result.work_dir / "module_x.py").is_file()
    finally:
        shutil.rmtree(result.work_dir.parent)


# --- guards -------------------------------------------------------------------------------


@pytest.mark.parametrize(
    "parts", [("Modules", "Native"), ("modules", "native"), ("MODULES", "NATIVE")]
)
def test_native_refused_unless_allowed(basic: Path, tmp_path: Path, parts: tuple[str, str]) -> None:
    out = tmp_path.joinpath("Mount&Blade Warband", *parts)
    with pytest.raises(GuardError):
        build(basic, out, quiet=True)
    assert not out.exists()
    assert build(basic, out, allow_native=True, quiet=True).passes == 1


def test_native_via_symlink_refused(basic: Path, tmp_path: Path) -> None:
    real = tmp_path / "Modules" / "Native"
    real.mkdir(parents=True)
    link = tmp_path / "harmless"
    link.symlink_to(real)
    with pytest.raises(GuardError):
        build(basic, link, quiet=True)


def test_foreign_nonempty_dir_needs_force(basic: Path, tmp_path: Path) -> None:
    out = tmp_path / "foreign"
    out.mkdir()
    (out / "notes.txt").write_text("mine")
    with pytest.raises(GuardError):
        build(basic, out, quiet=True)
    assert sorted(p.name for p in out.iterdir()) == ["notes.txt"]
    build(basic, out, force=True, quiet=True)
    assert (out / "notes.txt").read_text() == "mine"
    assert (out / mb.MARKER_NAME).is_file()
    build(basic, out, quiet=True)  # now it carries the marker


def test_empty_existing_dir_is_fine(basic: Path, tmp_path: Path) -> None:
    (tmp_path / "empty").mkdir()
    assert build(basic, tmp_path / "empty", quiet=True).passes == 1


def test_output_inside_source_or_game_refused(basic: Path) -> None:
    game_before = hash_tree(GAME)
    for out in (basic, basic / "export", GAME / "module_system" / "out", GAME):
        with pytest.raises(GuardError):
            build(basic, out, quiet=True)
    assert not (basic / "export").exists()
    assert hash_tree(GAME) == game_before


def test_output_path_is_a_file_refused(basic: Path, tmp_path: Path) -> None:
    (tmp_path / "file").write_text("x")
    with pytest.raises(GuardError):
        build(basic, tmp_path / "file", quiet=True)


def test_source_without_bat_refused(tmp_path: Path) -> None:
    (tmp_path / "src").mkdir()
    with pytest.raises(GuardError):
        build(tmp_path / "src", tmp_path / "out", quiet=True)


# --- overlay ------------------------------------------------------------------------------


def make_overlay(root: Path, files: dict[str, str]) -> Path:
    root.mkdir(parents=True)
    for name, body in files.items():
        (root / name).write_text(body)
    return root


def test_overlay_replaces_and_adds_files(
    basic: Path, tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    base_before = hash_tree(basic)
    ovl = make_overlay(
        tmp_path / "ovl",
        {
            "module_x.py": "VALUE = 7\n",
            "ID_x.py": "x = 7\n",
            "build_module.bat": "python process_use.py\r\npython process_extra.py\r\n",
            "process_extra.py": PRELUDE + "open(EXPORT + 'extra.txt', 'w').write('added')\n",
            "stale.pyc": "not python",
        },
    )
    (ovl / "__pycache__").mkdir()
    (ovl / "__pycache__" / "module_x.cpython-312.pyc").write_text("ignored")
    out = tmp_path / "out"
    result = build(basic, out, overlay=ovl)
    assert result.passes == 1
    assert (out / "use.txt").read_text() == "x=7\n"
    assert (out / "extra.txt").read_text() == "added"
    assert sorted(result.files) == ["extra.txt", "use.txt"]
    assert result.ids_differ_from_source == []
    assert hash_tree(basic) == base_before
    stdout = capsys.readouterr().out
    assert "overlay: 4 files (3 replaced, 1 added)" in stdout
    assert "replaced: build_module.bat" in stdout
    assert "added:    process_extra.py" in stdout
    assert "stale.pyc" not in stdout


def test_overlay_marker_fields(basic: Path, tmp_path: Path) -> None:
    build(basic, tmp_path / "plain", quiet=True)
    marker = json.loads((tmp_path / "plain" / mb.MARKER_NAME).read_text())
    assert marker["overlay_dir"] is None
    assert marker["overlay_files"] == []

    ovl = make_overlay(tmp_path / "ovl", {"module_x.py": "VALUE = 1\n", "b_new.py": ""})
    build(basic, tmp_path / "out", overlay=ovl, quiet=True)
    marker = json.loads((tmp_path / "out" / mb.MARKER_NAME).read_text())
    assert marker["overlay_dir"] == str(ovl.resolve())
    assert marker["overlay_files"] == ["b_new.py", "module_x.py"]
    assert marker["source_dir"] == str(basic.resolve())


def test_overlay_with_subdir_refused(basic: Path, tmp_path: Path) -> None:
    ovl = make_overlay(tmp_path / "ovl", {"module_x.py": "VALUE = 2\n"})
    (ovl / "nested").mkdir()
    with pytest.raises(GuardError, match="nested"):
        build(basic, tmp_path / "out", overlay=ovl, quiet=True)
    assert not (tmp_path / "out").exists()


def test_overlay_missing_or_not_a_dir_refused(basic: Path, tmp_path: Path) -> None:
    (tmp_path / "file").write_text("x")
    for ovl in (tmp_path / "missing", tmp_path / "file"):
        with pytest.raises(GuardError, match="overlay"):
            build(basic, tmp_path / "out", overlay=ovl, quiet=True)
    assert not (tmp_path / "out").exists()


def test_overlay_inside_source_refused(basic: Path, tmp_path: Path) -> None:
    for ovl in (make_overlay(basic / "ovl", {"module_x.py": "VALUE = 2\n"}), basic):
        with pytest.raises(GuardError, match="inside the source dir"):
            build(basic, tmp_path / "out", overlay=ovl, quiet=True)
    assert not (tmp_path / "out").exists()


def test_output_inside_overlay_refused(basic: Path, tmp_path: Path) -> None:
    ovl = make_overlay(tmp_path / "ovl", {"module_x.py": "VALUE = 2\n"})
    before = hash_tree(ovl)
    for out in (ovl, ovl / "out"):
        with pytest.raises(GuardError, match="inside the overlay dir"):
            build(basic, out, overlay=ovl, quiet=True)
    assert hash_tree(ovl) == before
    assert not (ovl / "out").exists()


def test_sync_ids_with_overlay_writes_into_overlay(
    stale: Path, tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    base_before = hash_tree(stale)
    ovl = make_overlay(tmp_path / "ovl", {"module_x.py": "VALUE = 3\n"})
    out = tmp_path / "out"

    first = build(stale, out, overlay=ovl, quiet=True)
    assert first.ids_differ_from_source == ["ID_x.py"]
    assert "differ from the source dir + overlay: ID_x.py" in capsys.readouterr().err
    assert sorted(p.name for p in ovl.iterdir()) == ["module_x.py"]

    second = build(stale, out, overlay=ovl, sync_ids=True, quiet=True)
    assert second.passes == 2
    assert second.synced_ids == ["ID_x.py"]
    assert (ovl / "ID_x.py").read_text() == "x = 3\n"
    assert hash_tree(stale) == base_before

    third = build(stale, out, overlay=ovl, quiet=True)
    assert third.passes == 1
    assert third.ids_differ_from_source == []
    assert (out / "use.txt").read_text() == "x=3\n"
    assert hash_tree(stale) == base_before


# --- CLI ----------------------------------------------------------------------------------


def test_main_exit_codes(basic: Path, tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    assert main(["--source-dir", str(basic), "-o", str(tmp_path / "out"), "-q"]) == 0
    assert capsys.readouterr().out == ""
    assert main(["--source-dir", str(basic), "-o", str(tmp_path / "out")]) == 0
    assert "unchanged" in capsys.readouterr().out
    assert main(["--source-dir", str(basic), "-o", str(tmp_path / "Modules" / "Native")]) == 2
    assert "--allow-native" in capsys.readouterr().err
    (basic / "process_use.py").write_text(PRELUDE + "sys.exit(4)\n")
    assert main(["--source-dir", str(basic), "-o", str(tmp_path / "out"), "-q"]) == 1
    assert "process_use.py failed (exit status 4)" in capsys.readouterr().err
    with pytest.raises(SystemExit) as info:
        main(["--bogus"])
    assert info.value.code == 2


def test_main_overlay_option(
    basic: Path, tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    assert mb.make_parser().parse_args([]).overlay is None
    assert mb.make_parser().parse_args(["--overlay", "some/dir"]).overlay == Path("some/dir")
    ovl = make_overlay(tmp_path / "ovl", {"module_x.py": "VALUE = 4\n", "ID_x.py": "x = 4\n"})
    out = tmp_path / "out"
    assert main(["--source-dir", str(basic), "-o", str(out), "--overlay", str(ovl), "-q"]) == 0
    assert (out / "use.txt").read_text() == "x=4\n"
    missing = str(tmp_path / "missing")
    assert main(["--source-dir", str(basic), "-o", str(out), "--overlay", missing, "-q"]) == 2
    assert "overlay dir not found" in capsys.readouterr().err
