# Calradia AI: instructions for the agent on the player's desktop

This repository is a Python 3 port of the Mount & Blade: Warband Module System plus
**Calradia AI**, a mod whose characters talk through a local language model. There are
three parts: the game mod (`mods/calradia_ai/overlay/`), calradia-server (Rust,
`calradia-server/`, port 8766), and the model server (llama.cpp through llama-swap,
serving `calradia-qwen3.5-9b`, which is Qwen 3.5 9B). The README explains the whole design.

This machine has Warband (native Linux, Steam) and the model server. You do everything
short of playing: run the tests, install the mod, prove that the game's requests reach
the model and come back correctly, run calradia-server while the player plays, and check
the server's side of each in-game step. **The player only plays** and tells you what they
saw.

**Always test with the real model.** `--fake-llm` and `--fault` exist for the automated
tests in cloud sessions, which have no model. Never use them for local verification.

## The one command

```bash
uv run python tools/calradia_check.py all
```

It runs three stages and stops at the first one that fails:

1. **`tests`** (about 7 minutes): pytest (the Module System port against the golden
   files, the mod build and its static checks), ruff, cargo fmt, clippy and cargo test.
2. **`install`**: finds Warband (Steam libraries, or `--game DIR` / `WARBAND_DIR`), and
   refuses while Warband is running. The first time, it copies `Modules/Native` to
   `Modules/CalradiaAI`; then it builds the mod into that folder.
3. **`pipeline`** (a few minutes): builds calradia-server, checks that the model server
   lists the model, then plays the game's side of the protocol against a private
   calradia-server (free port, scratch memory database). It uses the request templates
   compiled into the **installed** mod (`quick_strings.txt`), filled the way the engine
   fills them. It covers:
   - Hrodvar (v1);
   - a Native log entry and two daily snapshots becoming news;
   - a companion and a lord with their profiles, and world news in the lord's prompt;
   - memory across talk windows, a server restart, an older save, and a new campaign;
   - Cancel;
   - three provoking talks, where every action the server validated must arrive in the
     game's frame, and a refused offer of gold must be remembered;
   - daily ticks: the model makes a plan, and its act is delivered on the next tick.

Each line reads PASS, WARN or FAIL, and the summary lists everything that did not pass.
The exit status is 1 if anything failed. Every stage can also run alone: `tests`,
`install`, `pipeline`.

### Reading the pipeline result

- **FAIL** means the plumbing is broken. Fix it before the player plays.
  `logs/pipeline-*.log` holds every prompt and answer.
- **WARN** is about the model's behaviour, not the plumbing. Examples: a reply that does
  not mention the remembered fact, no action proposed by the model in three provoking
  talks, one planning answer rejected, the slowest reply over 60 seconds. Pass these on
  to the player; the in-game test judges them.
- Read **"Replies to read"** at the end. Do the replies sound like the characters? Borcha
  (`calradia-server/characters/trp_npc1.toml`) calls the player "boss" and talks of
  horses and debts. King Harlaus (`trp_kingdom_1_lord.toml`) is cold and measured.
  Report any reply that is out of character, mentions being an AI, or leaks thinking
  text.

### When something fails

| Symptom | Likely cause and fix |
|---|---|
| `model server reachable` fails | llama-swap is not running, or not at `http://172.17.0.1:8080/v1`. Start it, or pass `--upstream URL` (or set `CALRADIA_UPSTREAM`). |
| `model server serves the model` fails | The llama-swap config has another model name: pass `--model NAME` or set `CALRADIA_MODEL`. |
| `Warband found` fails | Pass `--game "/path/to/MountBlade Warband"` or set `WARBAND_DIR`. |
| `Warband is closed` fails | Ask the player to quit Warband, then run again. Never install while it runs. |
| `port 8766 is free` fails (`serve`) | A server is already running: `uv run python tools/calradia_check.py stop`. |
| A talk FAILS with `timeout` | The model took more than 90 s. Check the GPU and llama-swap, and whether the model was still loading. |
| `request templates match this check` fails | The mod's request URLs changed. Update `Game` in `tools/calradia_check.py` (`test_local_check_fills_every_request_template` fails too). |
| Planning answers are rejected | The log line says why. The server asks llama.cpp for a JSON object (`response_format`); check that llama-swap passes it through. |

## The play session

1. `all` passes.
2. Start the server for play **in the background**:
   `uv run python tools/calradia_check.py serve`. It listens on 127.0.0.1:8766, uses the
   real model and the player's normal memory database, and logs every prompt to
   `logs/server-<time>.log`.
3. Give the player [`docs/in-game-test.md`](docs/in-game-test.md). It says what they do
   and should see. The "Agent checks" column is yours: after the player reports a step,
   run `uv run python tools/calradia_check.py report` and confirm it from the log and the
   memory database.
4. Some steps ask you to stop or restart the server: run `stop`, then `serve` again. For
   the "model down" step, use `serve --skip-model-check --upstream http://127.0.0.1:9/v1`.
5. Write each result, with the date, in the Result column of `docs/in-game-test.md`. If a
   step fails, note what was seen, then find the cause in the code. When a step passes
   that closes one of the open points in `docs/characters-and-memory.md` or
   `docs/world-actions-autonomy.md`, update those documents too.

## Rules

- Never build or install while Warband is running. After installing a changed mod, the
  player must start a **new** game.
- Run one calradia-server on port 8766 at a time. Stop servers with
  `tools/calradia_check.py stop`, never with `pkill -f` (the pattern can match your own
  shell).
- Do not regenerate `tests/golden/`, and do not edit `game/` (the vanilla Module System).
  Mod changes go in `mods/calradia_ai/overlay/`, between the `# --- Calradia AI` markers
  (see the README).
- Change the Warband folder only through `install`.
- Commit on a branch and let the player review. Never commit `logs/` or memory
  databases (both are ignored by git).

## Where things are

| Path | What |
|---|---|
| `tools/calradia_check.py` | tests, install, pipeline, serve, stop, report |
| `docs/in-game-test.md` | the player's test session |
| `docs/protocol-v1.md` | the game ↔ server protocol (v1 and v2) |
| `docs/characters-and-memory.md`, `docs/world-actions-autonomy.md` | Milestones 3 to 6 |
| `calradia-server/characters/`, `calradia-server/factions/` | character profiles and kingdom lore (TOML; read at startup) |
| `calradia-server/world/log_entries.toml` | how Native's log entries read as news |
| `logs/` | pipeline and play-session server logs |
| `~/.local/share/calradia-ai/memory.sqlite3` | the player's memory database |
