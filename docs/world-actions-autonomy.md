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
initiatives). With `--fake-llm`, conversations summarize what the character knows,
planning writes a "[fake]" letter every tick, and an `ACTION: ...` typed in your message is
echoed as the character's proposal, so the game side can be tested without a model.

## Limits and known gaps

- Native logs no news for AI-only castle captures, and some log types are never written
  (see `world/log_entries.toml`). The daily snapshot covers captures, defections,
  captivity and war or peace, one day late at worst.
- One player raid can log two or three entries, so it can appear twice in the news.
- The map distance thresholds, text box and button positions, and how often the map
  trigger runs are unverified in game.
- Acts are deliberately small. Characters cannot declare war, defect, move armies or give
  land.
- Planning needs a model that can answer in JSON. Answers that are not valid JSON are
  logged and dropped, and the character keeps its old plan.

## In-game validation checklist

Run the real server with `--log-prompts` (and a scratch `--memory-db`), build and install
the mod while Warband is closed, and start a **new game**. Record result and date.

| # | Step | Expected | Result / date |
|---|---|---|---|
| W1 | Travel the map for a day. | Server log: `world campaign ... day N: ...` lines and `tick ...: planning queued`; `--memory-report` shows world nodes. No stutter in game. | untested |
| W2 | Fight and beat a bandit party or a lord; then talk to a lord of that realm. | His prompt (logged) lists the battle; he may mention it. | untested |
| W3 | Wait until a war or peace is declared (or a castle falls); talk to a lord of the realm. | The event is in his prompt. | untested |
| W4 | Save; travel until a new event; reload the save; talk to the same lord. | The newer event is not in his prompt. | untested |
| W5 | Stop calradia-server while on the map for a game day, then start it. | No errors in game; forwarding resumes. | untested |
| A1 | With `--fake-llm`, talk to a lord and type `ACTION: relation 2`. | Native's "relation improved" message; the relation changes on the character screen. | untested |
| A2 | With `--fake-llm`, type `ACTION: give 50` to a lord with a purse. | Status "... offers you 50 denars."; Accept and Refuse appear, Say is faded; Accept adds 50 denars. | untested |
| A3 | Type `ACTION: ask 50`; Refuse. Talk again and ask about it. | No gold moves; the next prompt says the offer was refused. | untested |
| A4 | Close the window while an offer is shown. | Counted as refused; no gold moves. | untested |
| A5 | With the real model, provoke or flatter a lord. | Occasional relation changes, never more than 5 per day per lord. | untested |
| M1 | Play a few days with `--fake-llm`. | Letters "[fake] ... writes to ..." pop up (at most one per day). | untested |
| M2 | With the real model, play a week, then talk to a king. | His prompt shows "Your private aims"; letters read in character. | untested |
| M3 | Talk while planning runs (log: "planning for ..."). | Log: "background task preempted by a talk"; the reply is not delayed by planning. | untested |
| M4 | Regression: all Milestone 3 checks and Hrodvar. | Unchanged. | untested |
