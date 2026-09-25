#!/usr/bin/env python3
"""Local checks for Calradia AI: everything short of playing the game.

Run from the repository root, on the machine that has Warband and the model server:

    uv run python tools/calradia_check.py all        # tests, then install, then pipeline
    uv run python tools/calradia_check.py tests      # every automated test and linter
    uv run python tools/calradia_check.py install    # build the mod into Warband
    uv run python tools/calradia_check.py pipeline   # the installed mod's requests, end to end
    uv run python tools/calradia_check.py serve      # calradia-server for a play session
    uv run python tools/calradia_check.py stop       # stop every running calradia-server
    uv run python tools/calradia_check.py report     # what the server saw during play

`pipeline` plays the game's side of the protocol against the real model (Qwen 3.5 9B
through llama-swap by default). It reads the request templates compiled into the
installed module (quick_strings.txt), fills them as the engine's send_message_to_url does
(encode_url=1), polls like the talk window, advances the conversation and world heads the
way the mod does, and checks each answer as the mod's callback does. It starts its own
calradia-server on a free port with a scratch memory database, so port 8766 and the
player's memories are untouched. What the server did (memories recalled, world news,
validated actions, plans) is read from its log; the model's words are printed for a
person to judge.

Only the Python standard library is used.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import random
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
from collections import Counter
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SERVER_DIR = ROOT / "calradia-server"
MANIFEST = SERVER_DIR / "Cargo.toml"
BINARY = SERVER_DIR / "target" / "release" / "calradia-server"
MODULE_SYSTEM = ROOT / "game" / "module_system"
OVERLAY = ROOT / "mods" / "calradia_ai" / "overlay"
LOGS = ROOT / "logs"

MODULE_NAME = "CalradiaAI"
BUILD_MARKER = ".modsys-build.json"  # modsys.build.MARKER_NAME
GAME_PORT = 8766
TEMPLATE_BASE = f"http://127.0.0.1:{GAME_PORT}/"
ROUTES = ("v1/talk", "v1/result", "v1/cancel", "v2/talk", "v2/event", "v2/world", "v2/tick")
STEAM_GAME = Path("steamapps/common/MountBlade Warband")

POLL_SECS = 0.75  # how often the talk window polls /v1/result
REQUEST_TIMEOUT = 10  # every handler answers at once; nothing waits for the model
MAX_FRAME = 580  # bytes, v2 frame (docs/protocol-v1.md)
SLOW_REPLY_SECS = 60
PLAN_WAIT_SECS = 240
CAI_NPC_HRODVAR = 1  # npc= of /v1/talk
PLAYER = "Ylva"

# Direct connections only: the game and calradia-server never use a proxy.
_OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


# --- values shared with the server and the Module System -------------------------------


def rust_constants() -> dict[str, int]:
    """The integer constants of calradia-server/src/protocol.rs (CODE_*, ACT_*, ...)."""
    text = (SERVER_DIR / "src" / "protocol.rs").read_text(encoding="utf-8")
    pattern = r"pub const ([A-Z0-9_]+): [a-z0-9]+ = (-?\d+);"
    return {name: int(value) for name, value in re.findall(pattern, text)}


def rust_default(name: str) -> str:
    text = (SERVER_DIR / "src" / "upstream.rs").read_text(encoding="utf-8")
    match = re.search(rf'pub const {name}: &str = "([^"]*)";', text)
    if not match:
        raise RuntimeError(f"{name} not found in upstream.rs")
    return match[1]


P = rust_constants()


def id_table(name: str) -> dict[str, int]:
    """An ID_*.py file of the Module System, e.g. {"trp_player": 0, ...}."""
    table = {}
    for line in (MODULE_SYSTEM / name).read_text(encoding="cp1254").splitlines():
        key, sep, value = line.partition(" = ")
        if sep:
            table[key.strip()] = int(value)
    return table


def module_constant(name: str) -> int:
    text = (MODULE_SYSTEM / "module_constants.py").read_text(encoding="cp1254")
    match = re.search(rf"^{name}\s*=\s*(-?\d+)", text, re.MULTILINE)
    if not match:
        raise RuntimeError(f"{name} not found in module_constants.py")
    return int(match[1])


def new_id() -> int:
    """A request, job or node id, drawn like script_cai_new_id."""
    return random.randint(1, 900_000_000)


# --- results ---------------------------------------------------------------------------


class Results:
    def __init__(self) -> None:
        self.rows: list[tuple[str, str, str]] = []
        self.replies: list[str] = []

    def add(self, status: str, name: str, detail: str = "") -> None:
        self.rows.append((status, name, detail))
        print(f"{status:<4}  {name}" + (f": {detail}" if detail else ""), flush=True)

    def ok(self, name: str, detail: str = "") -> None:
        self.add("PASS", name, detail)

    def warn(self, name: str, detail: str = "") -> None:
        self.add("WARN", name, detail)

    def fail(self, name: str, detail: str = "") -> None:
        self.add("FAIL", name, detail)

    def check(self, condition: bool, name: str, detail: str = "", soft: bool = False) -> bool:
        self.add("PASS" if condition else "WARN" if soft else "FAIL", name, detail)
        return condition

    def reply(self, who: str, text: str, secs: float) -> None:
        line = f"{who} ({secs:.1f} s): {text}"
        self.replies.append(line)
        print(f"      {line}", flush=True)

    @property
    def failed(self) -> bool:
        return any(status == "FAIL" for status, _, _ in self.rows)

    def summary(self, title: str) -> None:
        counts = Counter(status for status, _, _ in self.rows)
        print(
            f"\n== {title}: {counts['PASS']} passed, {counts['WARN']} warnings, "
            f"{counts['FAIL']} failed"
        )
        for status, name, detail in self.rows:
            if status != "PASS":
                print(f"   {status}  {name}" + (f": {detail}" if detail else ""))


def run(cmd: list[str | Path], cwd: Path = ROOT) -> bool:
    print(f"\n$ {' '.join(map(str, cmd))}", flush=True)
    return subprocess.run([str(c) for c in cmd], cwd=cwd, check=False).returncode == 0


# --- the engine's side of the wire -----------------------------------------------------


class TemplateDrift(Exception):
    """The mod's request templates and this emulator disagree."""


PLACEHOLDER = re.compile(r"([a-z0-9]+)=\{(?:reg\d+|s\d+)\}")


def engine_encode(value: object) -> str:
    """A substituted value as send_message_to_url with encode_url=1 sends it: only
    [0-9A-Za-z] pass, every other byte becomes %XX (docs/http-ipc.md)."""
    return "".join(
        chr(b) if chr(b).isascii() and chr(b).isalnum() else f"%{b:02X}"
        for b in str(value).encode("ascii")
    )


def load_templates(quick_strings: str) -> dict[str, str]:
    """The mod's request templates, by route ("v2/talk"), from quick_strings.txt."""
    found = {}
    for line in quick_strings.splitlines()[1:]:
        value = line.partition(" ")[2].strip()
        if value.startswith(TEMPLATE_BASE):
            found[value[len(TEMPLATE_BASE) :].partition("?")[0]] = value
    return found


def fill(template: str, values: dict[str, object], port: int) -> str:
    """The URL the engine sends for `template`, aimed at 127.0.0.1:`port`."""
    keys = set(PLACEHOLDER.findall(template))
    missing, unknown = sorted(keys - values.keys()), sorted(values.keys() - keys)
    if missing or unknown:
        raise TemplateDrift(
            f"{template.partition('?')[0]}: the mod sends {missing or 'no'} fields this check "
            f"does not fill, and not {unknown or 'none'}; update tools/calradia_check.py"
        )
    url = PLACEHOLDER.sub(lambda m: f"{m[1]}={engine_encode(values[m[1]])}", template)
    return f"http://127.0.0.1:{port}/{url[len(TEMPLATE_BASE) :]}"


@dataclass
class Frame:
    code: int
    text: str
    extra: tuple[int, ...] | None  # K N W X of a v2 frame


def parse_frame(body: str, rid: int) -> Frame:
    """An answer checked as the mod's callback and the protocol check it."""
    if len(body.encode()) > MAX_FRAME:
        raise ValueError(f"frame of {len(body.encode())} bytes")
    parts = body.split("|")
    if len(parts) not in (4, 8):
        raise ValueError(f"{len(parts)} fields in {body[:80]!r}")
    try:
        numbers = [int(p) for p in (parts[0], parts[1], *parts[3:])]
    except ValueError:
        raise ValueError(f"non-numeric field in {body[:80]!r}") from None
    first, code, *extra, last = numbers
    if first != rid or last != rid:
        raise ValueError(f"request id {first}/{last}, sent {rid}")
    if not P["CODE_READY"] <= code <= P["CODE_BUSY"]:
        raise ValueError(f"code {code}")
    text = parts[2]
    problems = [
        what
        for what, bad in (
            ("empty", not text),
            ("not ASCII", not text.isascii()),
            ("no letter", not any(c.isalpha() for c in text)),
            ("engine characters", any(c in "{}^\r\n" for c in text)),
            (f"over {P['MAX_TEXT']} characters", len(text) > P["MAX_TEXT"]),
        )
        if bad
    ]
    if problems:
        raise ValueError(f"text {text[:60]!r}: {', '.join(problems)}")
    return Frame(code, text, tuple(extra) if extra else None)


def http_get(url: str) -> str:
    with _OPENER.open(url, timeout=REQUEST_TIMEOUT) as response:
        return response.read().decode("ascii")


# --- the campaign the pipeline check plays ---------------------------------------------


@dataclass
class Cast:
    name: str
    faction: str
    faction_name: str
    occupation: str
    status: int
    relation: int
    gold: int
    ruler: str = ""


CAST = {
    "trp_kingdom_1_lord": Cast(
        "King Harlaus",
        "fac_kingdom_1",
        "Kingdom of Swadia",
        "slto_kingdom_hero",
        P["ST_FACTION_LEADER"],
        30,
        20000,
    ),
    "trp_knight_1_1": Cast(
        "Count Klargus",
        "fac_kingdom_1",
        "Kingdom of Swadia",
        "slto_kingdom_hero",
        0,
        -10,
        3000,
        "King Harlaus",
    ),
    "trp_npc1": Cast(
        "Borcha",
        "fac_commoners",
        "Commoners",
        "slto_player_companion",
        P["ST_IN_PARTY"],
        10,
        0,
    ),
}


class World:
    """Indices of the vanilla world, and the snapshot lists the game builds."""

    def __init__(self) -> None:
        self.troops = id_table("ID_troops.py")
        self.factions = id_table("ID_factions.py")
        self.parties = id_table("ID_parties.py")
        f, p, t = self.factions, self.parties, self.troops
        self.realms = list(range(f["fac_player_supporters_faction"], f["fac_kingdoms_end"]))
        self.centers = range(p["p_town_1"], p["p_village_1"])
        self.lords = range(t["trp_kingdom_1_lord"], t["trp_knight_1_1_wife"])
        names = {v: k for k, v in t.items()}
        self.lord_realm = {}
        for troop in self.lords:
            m = re.match(r"trp_(?:kingdom|knight)_(\d)_", names[troop])
            self.lord_realm[troop] = f[f"fac_kingdom_{m[1]}"] if m else f["fac_commoners"]

    def wars_mask(self, faction: int, wars: set[frozenset[int]]) -> int:
        """Talk field `wars`: bit k = realm fac_player_supporters_faction + k."""
        return sum(1 << k for k, r in enumerate(self.realms) if frozenset((faction, r)) in wars)

    def snapshot(
        self, wars: set[frozenset[int]], owners: dict[int, int], prisoners: set[int]
    ) -> dict[str, object]:
        realms = self.realms
        pairs = [(a, b) for i, a in enumerate(realms) for b in realms[i + 1 :]]
        lords = [f".{self.lord_realm[t] * 2 + (t in prisoners)}" for t in self.lords]
        half = len(lords) // 2
        return {
            "alive": sum(1 << k for k in range(1, len(realms))),
            "wars": "".join(f".{int(frozenset(p) in wars)}" for p in pairs),
            "owners": "".join(f".{owners[c]}" for c in self.centers),
            "lords": "".join(lords[:half]),
            "lords2": "".join(lords[half:]),
        }


@dataclass
class TalkResult:
    job: int
    frame: Frame | None
    secs: float
    why: str = ""

    @property
    def ready(self) -> bool:
        return self.frame is not None and self.frame.code == P["CODE_READY"]


class Game:
    """The mod's side of the protocol: its templates, ids, heads and polling."""

    def __init__(self, templates: dict[str, str], port: int, world: World) -> None:
        self.templates = templates
        self.port = port
        self.w = world
        f = world.factions
        self.wars = {frozenset((f["fac_kingdom_1"], f["fac_kingdom_2"]))}
        self.owners = {c: f["fac_kingdom_1"] for c in world.centers}
        self.prisoners: set[int] = set()
        self.new_campaign()

    def new_campaign(self) -> None:
        self.camp = new_id()
        self.head = 0  # $cai_mem_head
        self.outcome = 0  # $cai_head_outcome, sent as hres
        self.whead = 0  # $cai_world_head
        self.day = 5

    def url(self, route: str, values: dict[str, object]) -> str:
        return fill(self.templates[route], values, self.port)

    def send(self, route: str, **values: object) -> Frame:
        rid = new_id()
        return parse_frame(http_get(self.url(route, {"rid": rid, **values})), rid)

    # Talks.

    def talk_values(self, troop: str, msg: str, conv: int, job: int, hres: int) -> dict:
        c, w = CAST[troop], self.w
        faction = w.factions[c.faction]
        return {
            "job": job,
            "camp": self.camp,
            "conv": conv,
            "head": self.head,
            "troop": w.troops[troop],
            "day": self.day,
            "fac": faction,
            "pfac": 0,
            "frel": 0,
            "rel": c.relation,
            "rep": 0,
            "occ": module_constant(c.occupation),
            "st": c.status,
            "ren": 150,
            "hon": 5,
            "loc": w.parties["p_town_6"],
            "ldist": 3,
            "pg": 1,
            "wars": w.wars_mask(faction, self.wars),
            "whead": self.whead,
            "gold": c.gold,
            "pgold": 2000,
            "hres": hres,
            "pname": PLAYER,
            "nname": c.name,
            "fname": c.faction_name,
            "pfname": "",
            "lname": "Praven",
            "ruler": c.ruler,
            "spouse": "",
            "father": "",
            "msg": msg,
        }

    def start_talk(self, troop: str, msg: str, conv: int) -> tuple[int, Frame]:
        job = new_id()
        return job, self.send("v2/talk", **self.talk_values(troop, msg, conv, job, self.outcome))

    def wait(self, job: int, frame: Frame, started: float) -> TalkResult:
        give_up = P["GAME_GIVE_UP_SECS"]
        while frame.code == P["CODE_PENDING"]:
            if time.monotonic() - started > give_up:
                return TalkResult(job, None, time.monotonic() - started, f"no reply in {give_up} s")
            time.sleep(POLL_SECS)
            frame = self.send("v1/result", job=job)
        secs = time.monotonic() - started
        if frame.code != P["CODE_READY"]:
            return TalkResult(job, frame, secs, f"code {frame.code} {frame.text}")
        return TalkResult(job, frame, secs)

    def talk(self, troop: str, msg: str, conv: int) -> TalkResult:
        started = time.monotonic()
        job, frame = self.start_talk(troop, msg, conv)
        result = self.wait(job, frame, started)
        if result.ready:
            # The window shows the reply and makes it the head. A change of regard is
            # carried out at once; this check refuses every offer of gold.
            self.head = job
            kind = result.frame.extra[0] if result.frame.extra else 0
            self.outcome = (
                P["OUT_ACCEPTED"]
                if kind == P["ACT_RELATION"]
                else P["OUT_DECLINED"]
                if kind in (P["ACT_GIVE"], P["ACT_ASK"])
                else 0
            )
        return result

    def v1_talk_values(self, job: int, msg: str) -> dict:
        return {"job": job, "npc": CAI_NPC_HRODVAR, "day": self.day, "pname": PLAYER, "msg": msg}

    def talk_v1(self, msg: str) -> TalkResult:
        started, job = time.monotonic(), new_id()
        frame = self.send("v1/talk", **self.v1_talk_values(job, msg))
        return self.wait(job, frame, started)

    def cancel(self, job: int) -> Frame:
        return self.send("v1/cancel", job=job)

    # World nodes.

    def node_values(self, job: int) -> dict:
        return {
            "job": job,
            "camp": self.camp,
            "whead": self.whead,
            "day": self.day,
            "pname": PLAYER,
            "pfname": "",
        }

    def event_values(self, job: int, idx: int, kind: int, **fields: int) -> dict:
        entry = {
            "actor": -1,
            "center": -1,
            "clord": -1,
            "cfac": -1,
            "troop": -1,
            "tfac": -1,
            "fac": -1,
            **fields,
        }
        return {**self.node_values(job), "idx": idx, "type": kind, "time": self.day * 24, **entry}

    def world_values(self, job: int) -> dict:
        return {**self.node_values(job), **self.w.snapshot(self.wars, self.owners, self.prisoners)}

    def tick_values(self, job: int) -> dict:
        return {**self.node_values(job), "head": self.head}

    def node(self, route: str, values: dict) -> Frame:
        frame = self.send(route, **values)
        if frame.code == P["CODE_READY"]:
            self.whead = values["job"]  # made the world head after READY
        return frame

    def event(self, idx: int, kind: int, **fields: int) -> Frame:
        return self.node("v2/event", self.event_values(new_id(), idx, kind, **fields))

    def snapshot(self) -> Frame:
        return self.node("v2/world", self.world_values(new_id()))

    def tick(self) -> Frame:
        return self.node("v2/tick", self.tick_values(new_id()))


# --- calradia-server -------------------------------------------------------------------


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def port_open(port: int) -> bool:
    with socket.socket() as s:
        s.settimeout(0.5)
        return s.connect_ex(("127.0.0.1", port)) == 0


class Server:
    def __init__(self, port: int, db: Path, log: Path, args: list[str]) -> None:
        self.port, self.db, self.log, self.args = port, db, log, args
        self.proc: subprocess.Popen | None = None

    def start(self) -> bool:
        cmd = [
            str(BINARY),
            "--bind",
            f"127.0.0.1:{self.port}",
            "--memory-db",
            str(self.db),
            "--log-prompts",
            *self.args,
        ]
        with self.log.open("a") as out:
            self.proc = subprocess.Popen(cmd, stdout=out, stderr=subprocess.STDOUT, cwd=ROOT)
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                return False
            if port_open(self.port):
                return True
            time.sleep(0.1)
        return False

    def stop(self) -> None:
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()

    def text(self) -> str:
        return self.log.read_text(encoding="utf-8", errors="replace")

    def mark(self) -> int:
        return len(self.text())

    def since(self, mark: int) -> str:
        return self.text()[mark:]

    def wait_for(self, pattern: str, mark: int, secs: float) -> re.Match | None:
        deadline = time.monotonic() + secs
        while True:
            match = re.search(pattern, self.since(mark), re.MULTILINE)
            if match or time.monotonic() > deadline:
                return match
            time.sleep(0.5)

    def memory(self, job: int) -> dict[str, int] | None:
        """The recall of a talk, from its `job N memory: ...` line."""
        m = re.search(
            rf"job {job} memory: chain (\d+) turns, (\d+) with \S+ in \d+ conversations; "
            rf"using (\d+) of this talk, (\d+) recent, (\d+) relevant; (\d+) world events",
            self.text(),
        )
        if not m:
            return None
        keys = ("chain", "with", "current", "recent", "relevant", "world")
        return dict(zip(keys, map(int, m.groups()), strict=True))

    def action(self, job: int) -> tuple[str, str]:
        """("valid", "K A"), ("refused", why) or ("none", "") for a talk's proposal."""
        text = self.text()
        if m := re.search(rf"^\[[\d.]+\] job {job} action (\d+) (-?\d+)$", text, re.MULTILINE):
            return "valid", f"{m[1]} {m[2]}"
        if m := re.search(rf"job {job} action (.*)$", text, re.MULTILINE):
            return "refused", m[1]
        return "none", ""


def build_server(r: Results) -> bool:
    return r.check(
        run(["cargo", "build", "--release", "--manifest-path", MANIFEST]),
        "calradia-server builds (release)",
    )


def model_config(args: argparse.Namespace) -> tuple[str, str]:
    upstream = (
        args.upstream or os.environ.get("CALRADIA_UPSTREAM") or rust_default("DEFAULT_UPSTREAM")
    )
    model = args.model or os.environ.get("CALRADIA_MODEL") or rust_default("DEFAULT_MODEL")
    return upstream.rstrip("/"), model


def check_model(r: Results, upstream: str, model: str) -> bool:
    try:
        with _OPENER.open(f"{upstream}/models", timeout=REQUEST_TIMEOUT) as response:
            served = [m.get("id") for m in json.load(response).get("data", [])]
    except Exception as e:  # any failure means the same: no model to test with
        r.fail("model server reachable", f"{upstream}/models: {e}. Is llama-swap running?")
        return False
    return r.check(
        model in served,
        "model server serves the model",
        f"{model} at {upstream}" if model in served else f"{model} not in {served}",
    )


# --- the game ---------------------------------------------------------------------------


def steam_libraries() -> list[Path]:
    home = Path.home()
    roots = [
        home / ".local/share/Steam",
        home / ".steam/steam",
        home / ".steam/root",
        home / "snap/steam/common/.local/share/Steam",
        home / ".var/app/com.valvesoftware.Steam/.local/share/Steam",
        home / ".var/app/com.valvesoftware.Steam/data/Steam",
    ]
    libraries = []
    for root in roots:
        libraries.append(root)
        vdf = root / "steamapps" / "libraryfolders.vdf"
        if vdf.is_file():
            found = re.findall(r'"path"\s+"([^"]+)"', vdf.read_text(errors="replace"))
            libraries += [Path(p) for p in found]
    return libraries


def find_game(explicit: str | None) -> Path | None:
    given = explicit or os.environ.get("WARBAND_DIR")
    candidates = (
        [Path(given).expanduser()] if given else [lib / STEAM_GAME for lib in steam_libraries()]
    )
    for game in candidates:
        if (game / "Modules" / "Native").is_dir():
            return game.resolve()
    return None


def warband_running() -> bool:
    for comm in Path("/proc").glob("[0-9]*/comm"):
        try:
            if comm.read_text().startswith("mb_warband"):
                return True
        except OSError:
            continue
    return False


def sha(path: Path) -> str | None:
    return hashlib.sha256(path.read_bytes()).hexdigest() if path.is_file() else None


# --- commands ---------------------------------------------------------------------------


def cmd_tests(args: argparse.Namespace, r: Results) -> None:
    cargo = ["cargo"]
    manifest = ["--manifest-path", MANIFEST]
    checks = [
        ("pytest (Module System port, golden files, mod build)", ["uv", "run", "pytest", "-q"]),
        ("ruff check", ["uv", "run", "ruff", "check", "."]),
        ("ruff format", ["uv", "run", "ruff", "format", "--check", "."]),
        ("cargo fmt", [*cargo, "fmt", *manifest, "--check"]),
        ("cargo clippy", [*cargo, "clippy", *manifest, "--all-targets", "--", "-D", "warnings"]),
        ("cargo test (server)", [*cargo, "test", *manifest]),
    ]
    for name, cmd in checks:
        r.check(run(cmd), name)


def cmd_install(args: argparse.Namespace, r: Results) -> None:
    game = find_game(args.game)
    if not game:
        r.fail("Warband found", "pass --game DIR or set WARBAND_DIR (the folder with Modules/)")
        return
    r.ok("Warband found", str(game))
    running = warband_running()
    if not r.check(not running, "Warband is closed", "close it, then run again" if running else ""):
        return
    module = game / "Modules" / args.module_name
    before = sha(module / "scripts.txt")
    if not module.exists():
        shutil.copytree(game / "Modules" / "Native", module, symlinks=True)
        r.ok("module folder created from Native", str(module))
    force = [] if (module / BUILD_MARKER).exists() else ["--force"]
    cmd = ["uv", "run", "warband-build", "-q", "--overlay", OVERLAY, "-o", module, *force]
    if not r.check(run(cmd), "mod built into the game", str(module)):
        return
    missing = [
        route
        for route in ROUTES
        if route not in load_templates((module / "quick_strings.txt").read_text("cp1254"))
    ]
    r.check((module / "module.ini").is_file(), "module.ini present")
    r.check(not missing, "request templates installed", f"missing {missing}" if missing else "")
    after = sha(module / "scripts.txt")
    if before is None:
        r.ok("first install", "start a new game in Warband")
    elif before != after:
        r.warn("the installed mod changed", "start a NEW game: older saves are not supported")
    else:
        r.ok("the installed mod is unchanged")


def installed_templates(args: argparse.Namespace, r: Results, scratch: Path) -> dict | None:
    """Templates from --module, the installed module, or a fresh build."""
    module = Path(args.module).expanduser() if args.module else None
    if module is None and (game := find_game(args.game)):
        candidate = game / "Modules" / args.module_name
        module = candidate if (candidate / "quick_strings.txt").is_file() else None
    if module is None:
        module = scratch / "build"
        r.warn("templates from a fresh build", "the mod is not installed; run `install` first")
        if not run(["uv", "run", "warband-build", "-q", "--overlay", OVERLAY, "-o", module]):
            r.fail("mod builds")
            return None
    else:
        r.ok("templates from the installed mod", str(module))
    templates = load_templates((module / "quick_strings.txt").read_text(encoding="cp1254"))
    missing = [route for route in ROUTES if route not in templates]
    if not r.check(not missing, "every request template present", ", ".join(missing)):
        return None
    return templates


def reply_is_clean(r: Results, who: str, t: TalkResult) -> None:
    text = t.frame.text if t.frame else ""
    problems = [
        what
        for what, bad in (
            ("thinking tags", "<think" in text or "</think" in text),
            ("an ACTION line left in the speech", "action:" in text.lower()),
            ("a canned reply (is the server in --fake-llm mode?)", text.startswith("[fake]")),
        )
        if bad
    ]
    r.check(not problems, f"{who}: reply is clean speech", ", ".join(problems))
    r.reply(who, text, t.secs)


def talked(r: Results, name: str, t: TalkResult) -> bool:
    return r.check(t.ready, name, t.why or f"{t.secs:.1f} s")


def check_action(r: Results, s: Server, g: Game, who: str, t: TalkResult) -> str:
    """The frame carries exactly the proposal the server validated; returns its kind."""
    kind, detail = s.action(t.job)
    extra = t.frame.extra if t.frame else None
    if kind == "valid":
        k, amount = map(int, detail.split())
        expected = (k, amount, g.w.troops[who], 0)
        name = f"{CAST[who].name}: the validated action reaches the game"
        r.check(extra == expected, name, f"{expected}")
    else:
        name = f"{CAST[who].name}: no action in the frame"
        r.check(extra == (0, 0, 0, 0), name, detail or "none proposed")
    return kind


def scenario(g: Game, s: Server, r: Results) -> None:
    w, f = g.w, g.w.factions
    harlaus, klargus, borcha = "trp_kingdom_1_lord", "trp_knight_1_1", "trp_npc1"
    times: list[float] = []

    def talk(name: str, troop: str, msg: str, conv: int) -> TalkResult:
        t = g.talk(troop, msg, conv)
        times.append(t.secs)
        if talked(r, name, t):
            reply_is_clean(r, CAST[troop].name, t)
        return t

    print("\n-- Milestone 2: Camp -> Talk with Hrodvar (v1)")
    t = g.talk_v1("Hello, Hrodvar. How was the road today?")
    times.append(t.secs)
    if talked(r, "v1 talk with Hrodvar", t):
        r.reply("Hrodvar", t.frame.text, t.secs)

    print("\n-- Milestone 4: world news")
    mark = s.mark()
    frame = g.event(
        1,
        module_constant("logent_lord_defeated_by_player"),
        actor=0,
        troop=w.troops[klargus],
        tfac=f["fac_kingdom_1"],
        fac=f["fac_kingdom_1"],
    )
    r.check(
        frame.code == P["CODE_READY"] and frame.extra == (0, 0, 0, 0),
        "event node stored",
        frame.text,
    )
    r.check(
        "defeated Count Klargus" in s.since(mark),
        "Native log entry becomes news",
        "Ylva defeated Count Klargus in battle",
    )
    mark = s.mark()
    frame = g.snapshot()
    r.check(frame.code == P["CODE_READY"], "baseline snapshot stored", frame.text)
    r.check("world campaign" not in s.since(mark), "a baseline yields no news")
    g.day = 6
    g.wars.clear()
    g.owners[w.parties["p_castle_1"]] = f["fac_kingdom_2"]
    g.prisoners.add(w.troops[klargus])
    mark = s.mark()
    frame = g.snapshot()
    news = s.since(mark)
    r.check(frame.code == P["CODE_READY"], "next day's snapshot stored", frame.text)
    for what, needle in (
        ("peace", "made peace"),
        ("a castle changing hands", "Culmarr Castle"),
        ("a lord taken prisoner", "Count Klargus was taken prisoner"),
    ):
        r.check(needle in news, f"snapshot diff reports {what}")

    print("\n-- Milestone 3: Speak freely, profiles and memory")
    mark = s.mark()
    talk("companion talk (Borcha)", borcha, "Who are you, and why do you ride with me?", 1)
    r.check("no profile for" not in s.since(mark), "Borcha's profile is used")
    first = talk(
        "lord talk (King Harlaus)",
        harlaus,
        "My sister Ylfa keeps an inn in Sargoth. If you pass by, tell her I am well.",
        2,
    )
    m = s.memory(first.job) or {}
    r.check(
        m.get("world", 0) >= 2,
        "world news reaches the talk prompt",
        f"{m.get('world')} events for King Harlaus",
    )
    second = talk(
        "talk in a new window", harlaus, "Do you remember what I told you about my sister?", 3
    )
    m = s.memory(second.job) or {}
    r.check(
        m.get("with") == 1 and m.get("recent", 0) + m.get("relevant", 0) >= 1,
        "the earlier conversation is recalled",
        f"{m}",
    )
    if second.ready:
        said = second.frame.text.lower()
        r.check(
            any(k in said for k in ("ylfa", "sargoth", "inn")),
            "the reply uses the memory",
            "mentions Ylfa, Sargoth or the inn",
            soft=True,
        )

    print("\n   restarting calradia-server (same database)")
    s.stop()
    if not r.check(s.start(), "calradia-server restarts"):
        return
    third = talk("talk after a server restart", harlaus, "Where did I say my sister lives?", 4)
    m = s.memory(third.job) or {}
    r.check(m.get("with") == 2, "memory survives a server restart", f"{m}")

    g.head, g.outcome = first.job, 0  # the player loads a save made after the first talk
    branch = talk("talk after loading an older save", harlaus, "Have we spoken before?", 5)
    m = s.memory(branch.job) or {}
    r.check(m.get("with") == 1, "the older save does not know later talks", f"{m}")

    started = time.monotonic()
    job, frame = g.start_talk(harlaus, "Tell me about the war with the Vaegirs.", 6)
    canceled = g.cancel(job)
    result = g.wait(job, canceled, started)
    r.check(
        result.frame is not None and result.frame.code == P["CODE_CANCELED"],
        "Cancel stops the reply",
        result.why or canceled.text,
    )

    print("\n-- Milestone 5: actions proposed by the model")
    tries = (
        (
            harlaus,
            "Sire, my men have not been paid in weeks and they grumble. Could you "
            "spare me 300 denars? I will repay you in service.",
        ),
        (klargus, "You ran from me like a frightened hare, Klargus. You are a disgrace to Swadia."),
        (borcha, "You look troubled, Borcha. If you are short of coin, tell me how much you need."),
    )
    proposals, gold_offer = 0, None
    for n, (troop, msg) in enumerate(tries):
        t = talk(f"provoking talk with {CAST[troop].name}", troop, msg, 7 + n)
        if troop == harlaus:
            m = s.memory(t.job) or {}
            r.check(m.get("with") == 2, "the canceled reply is not remembered", f"{m}")
        if t.ready:
            kind = check_action(r, s, g, troop, t)
            proposals += kind != "none"
            k = t.frame.extra[0] if t.frame.extra else 0
            if kind == "valid" and k in (P["ACT_GIVE"], P["ACT_ASK"]):
                gold_offer = troop
    r.check(
        proposals > 0,
        "the model proposes actions",
        f"{proposals} of {len(tries)} provoking talks",
        soft=True,
    )
    if gold_offer:
        mark = s.mark()
        talk("talk after refusing the gold", gold_offer, "About that offer of gold...", 20)
        r.check("; refused" in s.since(mark), "the refusal is remembered")

    print("\n-- Milestone 6: planning and initiatives")
    delivered = False
    for attempt in (1, 2):
        g.day += 1
        mark = s.mark()
        frame = g.tick()
        r.check(frame.code == P["CODE_READY"], f"tick on day {g.day} stored", frame.text)
        if not r.check(bool(s.wait_for(r"planning queued", mark, 5)), "planning queued"):
            break
        outcome = s.wait_for(
            r"^\[[\d.]+\] (plan for (\S+)(?: rejected)?: .*|background task failed: .*)$",
            mark,
            PLAN_WAIT_SECS,
        )
        if outcome is None:
            r.fail("the model answers the planner", f"nothing in {PLAN_WAIT_SECS} s")
            break
        line = outcome[1]
        if "rejected" in line or line.startswith("background"):
            (r.warn if attempt == 1 else r.fail)("planning answer is usable", line)
            continue
        r.ok("a character made a plan", line)
        if "; act none" in line:
            r.ok("no act this time", "letters and deeds are tested in game")
            break
        planner = outcome[2]
        g.day += 1
        frame = g.tick()
        kinds = (P["INIT_LETTER"], P["INIT_ATTITUDE"], P["INIT_RIVALRY"])
        delivered = bool(frame.extra) and frame.extra[0] in kinds
        r.check(
            delivered and frame.extra[2] == w.troops.get(planner),
            "the next tick delivers the act",
            f"{frame.extra} {frame.text}",
        )
        if delivered:
            r.reply(f"{CAST[planner].name if planner in CAST else planner} (act)", frame.text, 0)
        break

    print("\n-- campaigns are separate")
    g.new_campaign()
    fresh = talk("talk in a new campaign", harlaus, "Have we met before?", 30)
    m = s.memory(fresh.job) or {}
    r.check(m.get("chain") == 0 and m.get("with") == 0, "a new campaign starts empty", f"{m}")

    print("\n-- summary")
    slow = max(times, default=0)
    r.check(
        slow <= SLOW_REPLY_SECS,
        "replies arrive in good time",
        f"slowest {slow:.1f} s, mean {sum(times) / max(len(times), 1):.1f} s",
        soft=True,
    )
    log = s.text()
    r.check("panicked" not in log, "no server panic")
    problems = [
        line
        for line in re.findall(
            r"^\[[\d.]+\] (.*(?:WARNING|upstream failure).*)$", log, re.MULTILINE
        )
        if "upstream failure: canceled" not in line  # the talk this check canceled
    ]
    r.check(not problems, "no warnings or model failures in the log", "; ".join(problems[:5]))


def cmd_pipeline(args: argparse.Namespace, r: Results) -> None:
    upstream, model = model_config(args)
    if not build_server(r) or not check_model(r, upstream, model):
        return
    LOGS.mkdir(exist_ok=True)
    log = LOGS / f"pipeline-{time.strftime('%Y%m%d-%H%M%S')}.log"
    with tempfile.TemporaryDirectory(prefix="calradia-check-") as tmp:
        scratch = Path(tmp)
        templates = installed_templates(args, r, scratch)
        if templates is None:
            return
        port = free_port()
        server = Server(
            port, scratch / "memory.sqlite3", log, ["--upstream", upstream, "--model", model]
        )
        if not r.check(server.start(), "calradia-server starts", f"log {log}"):
            print(server.text()[-2000:])
            return
        try:
            head = server.text()
            r.check(
                f"model {model}" in head,
                "the server uses the real model",
                next((ln for ln in head.splitlines() if "listening" in ln), ""),
            )
            scenario(Game(templates, port, World()), server, r)
        except TemplateDrift as e:
            r.fail("request templates match this check", str(e))
        except (OSError, ValueError) as e:
            r.fail("the game's requests are answered correctly", f"{type(e).__name__}: {e}")
        finally:
            server.stop()
    print(f"\nFull server log, with every prompt: {log}")
    if r.replies:
        print("\nReplies to read (do they sound like the character?):")
        for line in r.replies:
            print(f"  {line}")


def cmd_serve(args: argparse.Namespace, r: Results) -> None:
    upstream, model = model_config(args)
    if port_open(GAME_PORT):
        r.fail("port 8766 is free", "another calradia-server is running; stop it first")
        return
    if not build_server(r):
        return
    if args.skip_model_check:
        r.warn("model server not checked", f"{model} at {upstream}")
    elif not check_model(r, upstream, model):
        return
    LOGS.mkdir(exist_ok=True)
    log = LOGS / f"server-{time.strftime('%Y%m%d-%H%M%S')}.log"
    cmd = [str(BINARY), "--log-prompts", "--upstream", upstream, "--model", model]
    if args.memory_db:
        cmd += ["--memory-db", args.memory_db]
    cmd += args.server_args
    print(f"\n$ {' '.join(cmd)}\nlog: {log}\nStop with Ctrl-C (or SIGTERM).\n", flush=True)
    proc = subprocess.Popen(
        cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, cwd=ROOT
    )
    signal.signal(signal.SIGTERM, lambda *_: proc.terminate())
    with log.open("w") as out:
        try:
            for line in proc.stdout:
                sys.stdout.write(line)
                out.write(line)
                out.flush()
        except KeyboardInterrupt:
            proc.terminate()
    proc.wait()
    r.ok("calradia-server stopped", f"log {log}")


def server_pids() -> list[int]:
    pids = []
    for comm in Path("/proc").glob("[0-9]*/comm"):
        try:
            if comm.read_text().strip() == "calradia-server":
                pids.append(int(comm.parent.name))
        except OSError:
            continue
    return pids


def cmd_stop(args: argparse.Namespace, r: Results) -> None:
    pids = server_pids()
    for pid in pids:
        os.kill(pid, signal.SIGTERM)
    deadline = time.monotonic() + 5
    while server_pids() and time.monotonic() < deadline:
        time.sleep(0.1)
    left = server_pids()
    r.check(not left, "calradia-server stopped", f"pids {pids or 'none were running'}"
            if not left else f"still running: {left}")  # fmt: skip


def cmd_report(args: argparse.Namespace, r: Results) -> None:
    logs = sorted(LOGS.glob("server-*.log"))
    log = Path(args.log) if args.log else (logs[-1] if logs else None)
    if not log or not log.is_file():
        r.fail("server log found", "run `serve` first, or pass --log FILE")
        return
    text = log.read_text(encoding="utf-8", errors="replace")
    lines = [re.sub(r"^\[[\d.]+\] ", "", ln) for ln in text.splitlines()]

    def grep(pattern: str) -> list[str]:
        return [ln for ln in lines if re.search(pattern, ln)]

    def section(title: str, found: list[str], limit: int = 12) -> None:
        print(f"\n{title}: {len(found)}")
        for ln in found[-limit:]:
            print(f"  {ln[:300]}")

    print(f"log: {log}")
    section("Server", grep(r"^calradia-server listening|^memory "), 2)
    talks = Counter(m[1] for ln in lines if (m := re.search(r"memory: chain .* with (\S+)", ln)))
    print(f"\nTalks by character: {dict(talks) or 'none'}")
    section(
        "Memory recalled (talks with recent or relevant turns)",
        grep(r"memory: chain .*using \d+ of this talk, ([1-9]\d* recent|\d+ recent, [1-9])"),
    )
    ready = [int(m[1]) for ln in lines if (m := re.search(r"READY after (\d+) ms", ln))]
    if ready:
        print(
            f"\nReplies: {len(ready)}, slowest {max(ready) / 1000:.1f} s, "
            f"mean {sum(ready) / len(ready) / 1000:.1f} s"
        )
    section("Replies (last)", grep(r"READY after \d+ ms: "), 8)
    section("World news", grep(r"^world campaign "))
    section("Planning", grep(r"^(planning for|plan for|tick \d+ .*planning queued)"))
    section("Planning preempted by a talk", grep(r"preempted by a talk"), 3)
    section(
        "Actions (validated: kind 1 regard, 2 gift of gold, 3 request for gold; or refused)",
        grep(r"^job \d+ action "),
    )
    section(
        "Initiatives delivered (tick answers carrying an act)",
        grep(r"GET /v2/tick .*-> \"\d+\|0\|(?!stored)"),
    )
    section(
        "Failures and warnings",
        grep(
            r"WARNING|FAILED|upstream failure|background task failed|panicked|conflict|"
            r"bad HTTP|MEMORY IS UNAVAILABLE"
        ),
        20,
    )
    db = args.memory_db or next(
        (m[1] for ln in lines if (m := re.match(r"memory (.+?); \d+ character profiles", ln))),
        None,
    )
    cmd = [str(BINARY), "--memory-report", *(["--memory-db", db] if db else [])]
    print("\nMemory database:")
    subprocess.run(cmd, check=False)


def cmd_all(args: argparse.Namespace, r: Results) -> None:
    for name, command in (
        ("tests", cmd_tests),
        ("install", cmd_install),
        ("pipeline", cmd_pipeline),
    ):
        print(f"\n######## {name}")
        command(args, r)
        if r.failed:
            print(f"\nStopped after `{name}`: fix the failures above first.")
            return
    print(
        "\nAll local checks passed. Next: start the server for play with\n"
        "  uv run python tools/calradia_check.py serve\n"
        "and give the player docs/in-game-test.md."
    )


def main(argv: list[str] | None = None) -> int:
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--game", help="Warband folder (default: $WARBAND_DIR or Steam's)")
    common.add_argument("--module-name", default=MODULE_NAME, help="default %(default)s")
    common.add_argument("--upstream", help="model server URL (default: the server's)")
    common.add_argument("--model", help="model name (default: the server's)")
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ("all", "tests", "install", "stop"):
        sub.add_parser(name, parents=[common])
    pipeline = sub.add_parser("pipeline", parents=[common])
    pipeline.add_argument("--module", help="a built module folder (default: the installed one)")
    serve = sub.add_parser("serve", parents=[common])
    serve.add_argument("--memory-db", help="default: the server's")
    serve.add_argument(
        "--skip-model-check", action="store_true", help="start even if the model is down"
    )
    serve.add_argument("server_args", nargs=argparse.REMAINDER, help="more server options")
    report = sub.add_parser("report", parents=[common])
    report.add_argument("--log", help="default: the newest logs/server-*.log")
    report.add_argument("--memory-db", help="default: the server's")
    args = parser.parse_args(argv)
    args.module = getattr(args, "module", None)
    if args.command == "serve" and args.server_args[:1] == ["--"]:
        args.server_args = args.server_args[1:]
    results = Results()
    commands = {
        "all": cmd_all,
        "tests": cmd_tests,
        "install": cmd_install,
        "pipeline": cmd_pipeline,
        "serve": cmd_serve,
        "stop": cmd_stop,
        "report": cmd_report,
    }
    commands[args.command](args, results)
    if args.command != "report":
        results.summary(args.command)
    return 1 if results.failed else 0


if __name__ == "__main__":
    sys.exit(main())
