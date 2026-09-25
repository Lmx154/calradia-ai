# Warband Module System 1.171 for Python 3

This repository contains the original TaleWorlds **Mount & Blade: Warband Module System**,
version 1.171, ported from Python 2 to **Python 3.12**. It is managed with **uv** and
builds natively on Linux.

The Module System is the official modding toolkit for Warband. Game content (troops,
items, factions, parties, scripts, dialogs, game menus, mission templates, presentations
and so on) is written as Python data in `module_*.py` files. A set of compiler scripts
(`process_*.py`) serializes that content into the text files that the Warband engine
loads from `Modules/<YourModule>/` (`scripts.txt`, `conversation.txt`, `troops.txt`, …).

**Python is used only at compile time.** The game never runs Python. It reads the
generated `.txt` files. Which Python version produced those files makes no difference to
the engine, as long as the bytes are the same. For vanilla content, this port proves the
bytes are the same (see [Compatibility](#compatibility-what-is-and-isnt-verified)).

This is a modernization of the original TaleWorlds source, not a new Module System, and
it is not based on a community port. It adds no engine extensions (no WSE), no new
operations and no changes to the file formats.

## What was modernized

| Area | Before | Now |
|---|---|---|
| Language | Python 2 | Python 3.12 (`print()`, `range`, `in` instead of `has_key`, `str` methods instead of the `string` module, `isinstance` instead of `types.*Type`, `//` where Python 2 did integer division) |
| Encoding | implicit bytes | explicit `cp1254` on every compiler read and write, so the two non-ASCII bytes in `module_strings.py` reach `strings.txt` unchanged |
| Build | `build_module.bat` (Windows, run twice when IDs change) | `uv run warband-build`: one process per stage, repeated automatically until the ID files stop changing |
| Output location | hard-coded `export_dir` in `module_info.py` | `--output-dir` (default `build/Native/`), with guards against writing into a game install |
| Environment | global Python 2 | `uv sync` (Python 3.12, pytest, ruff) |
| Verification | none | byte-for-byte comparison with a Python 2.7 reference build, plus unit and behavior tests |

Legacy files were changed as little as possible. The port commit contains only
compatibility edits, and the whitespace/line-ending normalization is a separate commit.
Upstream comments are kept.

## Source and provenance

- **Source**: `mb_warband_module_system_1171.zip` (sha256
  `b8d5095031ed2b9df0a4020097bea06b6137022a52cd24a9d5c6b575444b16e1`), files dated
  2012-10-31. It contains `Module_system 1.171/` and `Module_data 1.171/`. The unmodified
  tree is commit `1c213ba`.
- **Target game**: this was developed against native Linux Warband **1.174** (Steam).
  The compiled output matches that game's shipped `Modules/Native` files except for line
  endings and two credits strings (details below).

## Requirements

- Linux (developed and tested on Ubuntu). There are no Windows, Wine or Proton
  dependencies. macOS will probably work but has not been tested.
- [uv](https://docs.astral.sh/uv/). uv downloads Python 3.12 itself, so no system Python
  is needed.

## Setup

```bash
uv sync
```

## Compile the module

```bash
uv run warband-build                      # writes to ./build/Native/
uv run warband-build -o ~/some/dir        # choose the output directory
uv run python -m modsys.build --help      # same tool, as a module
```

The command works from any working directory. It exits with status 0 on success, 1 if
compilation failed and 2 if the invocation was refused. Options:

| Option | Meaning |
|---|---|
| `-o, --output-dir DIR` | Where to write the 32 generated files (default `build/Native/`). |
| `--source-dir DIR` | Module System tree to compile (default `game/module_system`). |
| `--sync-ids` | Copy the regenerated `ID_*.py` files back into the source tree (see below). |
| `--force` | Allow a non-empty output directory that this tool did not create. |
| `--allow-native` | Allow an output directory named `Modules/Native`. |
| `--overlay DIR` | Copy the files in `DIR` over the sources before compiling. This is how a mod is kept as "vanilla + changed files" (see [CalradiaAI](#calradiaai-mod)). With `--sync-ids`, IDs are written into `DIR`. |
| `--keep-work` | Keep the temporary work directory for debugging. |
| `-q, --quiet` | Print only problems. |

What the build does:

1. It copies `game/module_system/` into a temporary work directory, so the source tree is
   never modified.
2. It runs the 29 stages listed in `build_module.bat`, in that order. Each stage runs in
   its own Python process, and the stage list is read from the `.bat` file.
3. It repeats the whole pass until the `ID_*.py` files are stable, then publishes.
4. It writes the generated files and a `.modsys-build.json` marker (with the sha256 of
   each file) to the output directory. It never deletes anything there.

A pass fails if a stage exits with an error or prints a line starting with
`ERROR`/`Error`. The legacy scripts report unresolved identifiers by printing a message
and carrying on, so without this check a broken build would look successful. Lines
starting with `WARNING` are shown but do not fail the build.

### Why each stage runs in its own process

The `process_*.py` scripts do their work when they are imported. `module_*.py` files use
`from ID_troops import *` and similar imports, which copy numeric IDs into the module at
import time. Those `ID_*.py` files are written by earlier stages and must be re-imported
fresh by later ones. The ID files also form a cycle: `module_items` imports
`module_constants`, which imports `ID_items`. That is why the committed `ID_*.py` files
are needed to start a build.

When you add, remove or reorder objects, the first pass can see outdated IDs. The driver
detects that the ID files changed and runs another pass. The original workflow made you
run `build_module.bat` twice instead. Run with `--sync-ids` to update the committed ID
files so later builds need only one pass.

One case cannot be solved by repeating passes. If a new identifier is used *while its
own ID file is still being generated* (for example, a new `itm_x` used inside
`module_constants.py`), every pass fails the same way. The build stops right away and
reports the `NameError`. The original tool had the same limitation.

## Install into Warband (native Linux)

The compiler generates only the `.txt` data files. A playable module also needs
`module.ini`, the `Resource`, `Textures`, `SceneObj`, `Data` and `languages` folders, and
so on. The simplest approach is to start from a copy of Native. **Never compile into
Native itself.** The build refuses to do that unless you pass `--allow-native`.

```bash
GAME="$HOME/.local/share/Steam/steamapps/common/MountBlade Warband"
# Steam installed as a snap:
# GAME="$HOME/snap/steam/common/.local/share/Steam/steamapps/common/MountBlade Warband"

cp -r "$GAME/Modules/Native" "$GAME/Modules/MyMod"
# The first time, the directory has no build marker yet, so --force is required:
uv run warband-build -o "$GAME/Modules/MyMod" --force
# Later builds find the marker and don't need --force:
uv run warband-build -o "$GAME/Modules/MyMod"
```

Then choose **MyMod** in the launcher. If the game fails to load it, check `rgl_log.txt`
in the game directory.

### Module_data (flora, skyboxes, ground specs)

`game/module_data/` contains the separate generators for `flora_kinds.txt`,
`skyboxes.txt` and `ground_specs.txt`. These files belong in a module's `Data/` folder.
The original `build_module.bat` never ran these generators, and neither does
`warband-build`:

```bash
uv run warband-build-data               # writes to ./build/module_data/
```

Changing ground specs also requires the engine to be recompiled, which is not possible
with the retail game. The TaleWorlds notes in `game/module_data/readme.txt` explain this.

## Add content

Edit the files in `game/module_system/` exactly as described in any Warband Module
System tutorial. The file formats and operations are unchanged. Some examples:

- **Script**: add a tuple to the `scripts` list in `module_scripts.py`:

  ```python
  ("my_mod_give_gold",
   [
     (store_script_param_1, ":amount"),
     (troop_add_gold, "trp_player", ":amount"),
   ]),
  ```

  Call it with `(call_script, "script_my_mod_give_gold", 100)`.
- **Dialogue**: add entries to `dialogs` in `module_dialogs.py`, with the form
  `[anyone, "start", [conditions], "text", "next_state", [consequences]]`. Order matters,
  because the first matching entry wins.
- **Troops, items, factions, parties, menus, triggers**: edit `module_troops.py`,
  `module_items.py`, `module_factions.py`, `module_parties.py`, `module_game_menus.py`
  and `module_triggers.py` / `module_simple_triggers.py` in the same way.
- **Operations and flags** are defined in `header_operations.py` and the other
  `header_*.py` files. Do not renumber them, because the numbers are what the engine
  executes.

Then run `uv run warband-build`, and `uv run warband-build --sync-ids` if you added or
reordered objects. The legacy sources are plain Python 3, and `from X import *` is still
how they share names. They are deliberately not reorganized into a package.

## CalradiaAI mod

CalradiaAI lets you talk to NPCs whose replies come from a local language model.

- **Speak freely.** is a new last option when you talk to a lord (`lord_talk`) or to a
  companion in your party (`member_talk`). It opens a conversation window with that
  character. The character's profile, the live campaign situation (faction, relation,
  renown, honour, location, day, status) and memories of your earlier conversations go
  into the prompt. Memories persist across game and server restarts, per campaign and per
  savegame branch. See [`docs/characters-and-memory.md`](docs/characters-and-memory.md).
- **Camp → Talk with Hrodvar.** is the Milestone 2 proof of concept: Hrodvar, a Nord
  sellsword riding with your company, who has no memory.

- **World awareness, actions and autonomy** (Milestones 4-6): characters know the news
  that concerns them (battles, sieges, wars, defections, captures), may propose a small
  action in conversation (a change of regard, an offer or request of gold that you accept
  or refuse), and important characters form goals and plans each day and act on their own
  (letters, changes of attitude, rivalries). See
  [`docs/world-actions-autonomy.md`](docs/world-actions-autonomy.md).

Type a message and click **Say**. Vanilla dialogs, quests, recruitment and trading are
unchanged. The model only proposes; every effect is validated by the server and again by
the game's single executor script, and gold never moves without your consent.

It has three parts:

**The game mod** is in `mods/calradia_ai/overlay/`. Each file is a complete copy of the
vanilla file with small edits marked `# --- Calradia AI`:

| File | Change |
|---|---|
| `module_dialogs.py` | Appends the two "Speak freely." options (lords and companions). |
| `module_game_menus.py` | Adds the "Talk with Hrodvar." camp option. |
| `module_presentations.py` | Adds the `cai_talk` conversation window at the end of the list. |
| `module_scripts.py` | Adds the protocol constants, `cai_new_id`, `cai_tx_send` (the only place that sends requests), `cai_store_npc_name`, `cai_store_context` (reads the live game state), the Milestone 4-6 scripts (`cai_background_send`, `cai_background_done`, `cai_store_log_entry`, `cai_store_snapshot`, `cai_store_player_realm`, `cai_execute_initiative`, and `cai_execute_action`, the only script that changes game state) and `game_receive_url_response`. |
| `module_simple_triggers.py` | Appends one trigger that runs the background sender every map frame. |
| `ID_scripts.py`, `ID_presentations.py` | The regenerated ID files, so the build needs only one pass. |
| `variables.txt` | The vanilla global variables, followed by the mod's `cai_*` variables in a fixed order. |

**The server** is `calradia-server/`, written in Rust. It runs jobs asynchronously
(`/v1/talk`, `/v2/talk`, `/v1/result`, `/v1/cancel`), so no HTTP request stays open while
the model generates. It talks to an OpenAI-compatible endpoint. Character profiles are
TOML files in `calradia-server/characters/` (every companion, king and claimant), and
kingdom lore that every lord of a realm shares is in `calradia-server/factions/`. Both are
read at startup, so you can edit them without recompiling. Memory is a SQLite database (default
`~/.local/share/calradia-ai/memory.sqlite3`; `--memory-db PATH` to change it,
`--memory-report` to see what it holds, `--log-prompts` to log every prompt). Hrodvar's
entry is in `src/npc.rs`.

**The model** is served by llama.cpp through llama-swap. The default is
`calradia-qwen3.5-9b` (Qwen3.5 9B Uncensored, Q6_K) at `http://172.17.0.1:8080/v1`.
Override them with `--upstream`/`CALRADIA_UPSTREAM` and `--model`/`CALRADIA_MODEL`.

To run it, close Warband before building or installing the mod. Changing module files
during a session can leave saves unusable; the game loads the files only at startup.
`tools/calradia_check.py` does all of this and checks the result (see
[Local verification](#local-verification)); by hand:

```bash
GAME=~/.steam/steam/steamapps/common/"MountBlade Warband"
cp -r "$GAME/Modules/Native" "$GAME/Modules/CalradiaAI"      # first time only
uv run warband-build --overlay mods/calradia_ai/overlay -o "$GAME/Modules/CalradiaAI" --force
cargo run --release --manifest-path calradia-server/Cargo.toml   # listens on 127.0.0.1:8766
```

Start a **new game**. Saves from older mod versions are not supported. The first build
fetches the Rust crates (`rusqlite` with a bundled SQLite, `serde`, `toml`) and needs a C
compiler (`build-essential`); later builds work with `--offline`.

If a request hangs, the window shows a red warning after 5 seconds. Restart
`calradia-server` to release it. If a busy flag survives closing and reopening the
window, **Reset** appears after 15 seconds; restart the server before clicking it.
**Cancel** stops waiting for the reply and sends the cancellation when the transport
is free.

### Local verification

On the machine with Warband and the model server, one command runs every check short of
playing:

```bash
uv run python tools/calradia_check.py all     # tests, install into Warband, pipeline
uv run python tools/calradia_check.py serve   # then: calradia-server for a play session
```

The pipeline stage plays the game's side of the protocol with the real model. It sends
the request templates compiled into the installed mod, filled as the engine fills them,
to a private calradia-server, and checks every answer as the mod's callback does. It
covers Milestones 2 to 6: memory across windows, restarts and older saves, world news,
actions reaching the game, and plans with their acts. Then the player runs
[`docs/in-game-test.md`](docs/in-game-test.md), while `report` shows the server's side
of each step. [`CLAUDE.md`](CLAUDE.md) is the procedure for the agent on the desktop.

`--fake-llm` (a canned reply after 1.5 seconds) and `--fault MODE` (`hang`, `close`,
`empty`, `wrong-rid`, `malformed`, `delay`, `oversize-3000`, `nonascii`) are for the
automated tests and for the Milestone 2 transport checks
([acceptance tests](docs/http-ipc.md#milestone-2-acceptance-tests)), not for testing
characters.

Further reading:
- [`docs/protocol-v1.md`](docs/protocol-v1.md): the game↔server contract.
- [`docs/http-ipc.md`](docs/http-ipc.md): the verified engine HTTP behavior and in-game
  findings.

Known limitations:
- Messages are ASCII only, at most 300 characters.
- `{ }` and `^` in messages are interpreted by the engine.
- Replies are capped at 500 characters.
- The text box has no cursor movement (engine limitation).
- The conversation window shows the last 4 exchanges. Hrodvar has no memory; characters
  reached through "Speak freely." do (see the limitations in
  [`docs/characters-and-memory.md`](docs/characters-and-memory.md)).

## Tests and linting

```bash
uv run pytest -q          # full suite, about 2.5 minutes (each full build takes about 10 s)
uv run ruff check .
uv run ruff format --check .
cargo test --manifest-path calradia-server/Cargo.toml     # server: protocol, memory, prompts
cargo clippy --manifest-path calradia-server/Cargo.toml --all-targets -- -D warnings
cargo fmt --manifest-path calradia-server/Cargo.toml --check
```

| Test file | What it checks |
|---|---|
| `test_golden.py` | The 32 export files, the final `ID_*.py` files and the committed `ID_*.py` files are byte-identical to the Python 2.7 reference. It first checks the reference against `MANIFEST.json`. Negative controls confirm that the comparator catches a flipped byte, LF→CRLF, a missing or extra file, and a UTF-8-re-encoded `0x97`. |
| `test_build.py` | A fresh build succeeds in one pass. The build works from any directory and with output paths containing spaces or `&`. `game/` is unchanged afterwards. The output guards work. A decoy `PYTHONPATH`/`PYTHONSAFEPATH` is ignored. The stage list matches `build_module.bat`. Exit codes are correct. |
| `test_fixpoint.py` | Reordered troops, a new troop used by a new party, and a new item used by a troop each build correctly in a single invocation. The unsolvable case fails quickly with a clear message. |
| `test_determinism.py` | Builds with different hash seeds and locale/UTF-8 modes are byte-identical to each other and to the reference. |
| `test_serialization.py` | Operand encoding (`$global`, `:local`, `@quick string`, tagged IDs, negative and large ints), opmask and opcode values, identifier escaping, `%f` float formatting, and integer division rounding down for negative numbers. |
| `test_identifiers.py` | IDs such as `trp_player == 0` stay stable, and every generated ID file equals the reference. |
| `test_module_data.py` | The Module_data output is byte-identical to the reference. |
| `test_calradia_ai.py` | The CalradiaAI overlay builds in one pass. Vanilla output is preserved (untouched files are byte-identical, and vanilla scripts, presentations and dialog lines are an unchanged prefix, so no dialog id moves). There is a single send site, the mod's code uses only whitelisted (read-only) operations and writes only `cai_*` globals, the URL templates follow the protocol rules, the protocol constants match `calradia-server/src/protocol.rs`, and the server embeds the ID files the mod uses. |
| `test_driver_unit.py` | The driver's own logic, tested against a small fake module (fixpoint, error detection, guards, publishing). |
| `test_calradia_check.py` | The local check tool's engine emulation: URL encoding, exact template filling, frame checks, the snapshot shape, and head tracking like the mod. (`test_calradia_ai.py` checks that it fills every template of the built mod.) |

Ruff applies the full rule set to `src/` and `tests/`. For the legacy `game/` tree, it
only checks for syntax errors and undefined names, and it does not format that tree. This
keeps the migration diff reviewable. See `game/ruff.toml`.

### The reference build

`tests/golden/` holds the output of the **unmodified** Module System (commit `1c213ba`)
compiled by Python **2.7.18** in the Docker image
`python:2.7.18-slim@sha256:6c1ffdff…3992363`. `MANIFEST.json` records the provenance and
the sha256 of every file. Two independent runs of that build were byte-identical. Docker
and Python 2 are **not** needed to build or test. They are only needed to reproduce the
reference:

```bash
tools/make_reference.sh /tmp/ref     # compare /tmp/ref with tests/golden
```

Do not regenerate `tests/golden` to make a failing test pass. The reference comes from
the original sources, so a real regression in the port cannot pass by changing it.

## Compatibility: what is and isn't verified

**Verified by automated tests:**

- All 32 files the compiler produces are **byte-identical** to the Python 2.7 reference
  build of the original sources. The same holds for the 23 `ID_*.py` files and the 5
  Module_data outputs.
- Output does not depend on hash seed, locale or `PYTHONUTF8`.

**Verified manually, once:**

- The Python 2.7 reference output equals the `Modules/Native` files shipped with Linux
  Warband 1.174, with two exceptions:
  - The shipped files use CRLF line endings, because they were built on Windows. The
    Linux build uses LF, just as Python 2 on Linux does.
  - `strings.txt` differs in `str_credits_3` and `str_credits_9`. TaleWorlds updated
    those credits after 1.171.

- The game loads and plays the compiled module on native Linux Warband 1.174 (in-game
  smoke test on 2026-09-25). That covers LF line endings, the main menu, Quick Battle
  (including the biographies with the cp1254 dashes), character creation, the world map,
  towns, trading, battles, and saving and loading.

## Known limitations

- **Encoding**: all compiler I/O uses `cp1254`, the encoding declared by
  `module_strings.py`. Text that cannot be encoded in cp1254 fails loudly instead of being
  written incorrectly. Mods that need other scripts should convert the files and test
  carefully.
- **Line endings**: output is LF only. No CRLF option is provided.
- **Latent upstream bugs, deliberately unchanged**:
  - `header_parties.carries_gold` references an undefined `big_num`.
  - `process_operations.py` calls `has_key` on a list, a path vanilla never reaches.
  - `process_global_variables.py` prints an undefined name in an error path.
  - `header_common.reg()` calls an undefined `cause_error()` on purpose to abort.
- `process_line_correction.py` and `process_tags_unused.py` were ported so that they
  compile, but they are not part of the build, just as in the original.
- `build_module.bat` is kept only as the source of the stage order and as provenance.
