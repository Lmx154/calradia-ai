# World awareness, actions and autonomy (Milestones 4-6)

These milestones build on the character memory of Milestone 3
([characters-and-memory.md](characters-and-memory.md)). The wire format is in
[protocol-v1.md](protocol-v1.md#protocol-v2-milestones-4-6-world-nodes-actions-and-initiatives).

## What the player sees

- **Milestone 4, world awareness.** While you travel the map, the mod quietly forwards
  what happens in Calradia to calradia-server: Native's own event log (your battles and
  sieges, lords' quarrels and insults, war declarations and their reasons, pledges, fief
  grants, marriages, raids) and a daily snapshot of wars, town and castle owners, and every
  lord's allegiance and captivity. When you then talk to a character, they know the news
  that concerns them: what they took part in, what happened to their realm, what you have
  done, and the great events of the last month.
- **Milestone 5, actions.** A character's words can do one thing. Its regard for you may
  change (you see Native's own "relation changed" message), or it may offer you gold or ask
  you for some. **Accept** or **Refuse** buttons appear for gold, and nothing changes hands
  without your answer. The character remembers what you did with its offer.
- **Milestone 6, autonomy.** Once a game day, one important character (a ruler, a
  claimant, or someone you have spoken with) thinks about what it wants: a goal, a plan and
  sometimes an act. It may write you a letter, change its attitude towards you (and tell
  you why), or fall out with or warm to another lord (you see a rumour). When you next
  speak with it, it knows its own aims and what it did.

## Guarantees

- **The model never changes the game directly.** It can only propose from a fixed menu.
  The server validates every proposal against bounds, purses and cooldowns. The game checks
  it again in `script_cai_execute_action`, the mod's only state-changing script (a test
  enforces this), which uses vanilla operations and scripts. Gold needs your consent.
- **Everything follows the savegame.** News, plans, deeds and outcomes hang off the save's
  chains. Reload an older save and the characters forget what happened after it, including
  letters they had not yet sent in that timeline.
- **Conversations come first.** Planning runs only when no conversation is waiting, and a
  new message stops it at once.
- **The game never waits.** Background requests go one at a time between conversations.
  If calradia-server is not running, the sender waits a game hour before trying again.

Server switches: `--no-actions`, `--no-autonomy`, `--log-prompts` (see what each character
was told), `--memory-report` (per campaign: conversations, world nodes and events, plans,
initiatives). `--fake-llm` exists only for the automated tests, which run without a
model. All local testing uses the real model.

## Limits and known gaps

- Native logs no news for AI-only castle captures, and some log types are never written
  (see `world/log_entries.toml`). The daily snapshot covers captures, defections,
  captivity and war or peace, one day late at worst.
- One player raid can log two or three entries, so it can appear twice in the news.
- The map distance thresholds, text box and button positions, and how often the map
  trigger runs are unverified in game.
- Acts are deliberately small. Characters cannot declare war, defect, move armies or give
  land.
- Planning needs a model that can answer in JSON. The server asks for a JSON object
  (`response_format`, which llama.cpp enforces) and allows 400 tokens, against 220 for a
  spoken reply. Answers that are still not valid JSON are logged and dropped, and the
  character keeps its old plan.

## Validation

Automated: `cargo test` covers the world chain, snapshot diffs, action validation,
outcomes, planning, preemption and initiative delivery. Before any play session,
`uv run python tools/calradia_check.py all` replays the installed mod's own requests
against calradia-server and the real model: news from a log entry and from snapshots,
news in a lord's prompt, actions the model proposes (each validated one must reach the
game's frame), a refused offer remembered, and a plan with its act delivered on the next
tick.

In game: Parts 2 to 4 of [`in-game-test.md`](in-game-test.md). The session is run by
the player, with the agent on the desktop checking the server side ([`CLAUDE.md`](../CLAUDE.md)).
