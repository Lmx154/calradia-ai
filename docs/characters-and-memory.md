# Characters and memory (Milestone 3)

Milestone 3 lets the player speak freely with real Warband characters. Each character has
a recognizable personality, knows the live campaign situation, and remembers earlier
conversations across game and server restarts, separately per campaign and per savegame
branch. The wire format and the exact memory rules are in
[`protocol-v1.md`](protocol-v1.md#protocol-v2-characters-live-context-and-memory-milestone-3).

Nothing here changes gameplay. The mod only reads game state, and the model's words have
no effect on the game. There is no autonomous NPC behavior.

## In the game

- Talk to a **lord** (on the map, in a hall, or as your prisoner) or to a **companion in
  your party** (party screen → Talk). **Speak freely.** is the last option. It opens the
  talk window for that character; the character is identified automatically from
  `$g_talk_troop`. Closing the window returns to the dialog ("Anything else?").
- Vanilla options are untouched and come first, so quests, recruitment, trading and
  so on work as before.
- The camp menu's **Talk with Hrodvar.** is still there. Hrodvar uses protocol v1 and
  has no memory.

## Character profiles

`calradia-server/characters/<troop id>.toml`, one file per character, keyed by the troop's
stable Module System identifier (`trp_npc1`, not the display name and not the troop
index). The server reads them at startup; restart it after editing. A bad file stops the
server with a message that names the file.

```toml
id = "trp_npc1"          # must equal the file name
name = "Borcha"

[canon]                  # from the vanilla game files; every claim traceable to sources
background = """..."""
personality = "..."
motivations = ["..."]
speaking_style = "..."
relationships = ["Marnid: ..."]
tendencies = ["..."]
sources = ["module_strings.py: npc1_backstory_b", "module_scripts.py initialize_npcs: ..."]

[mod]                    # written for this mod; not in the vanilla game
personality = "..."      # same keys as [canon], without sources
```

Shipped profiles: **Borcha**, **Marnid** and **Matheld** (companions), and **King
Harlaus** and **King Ragnar**. Their canonical parts come from the companion strings and
`initialize_npcs`, and from the pretender stories and "monarch responses" in
`module_strings.py`.

Characters without a file still talk, with a generic profile built from live data: their
name, kind (lord, lady, king, claimant or companion) and this campaign's
`slot_lord_reputation_type`, described in the words of Native's own comments (martial,
quarrelsome, cunning, and so on). The prompt tells them not to invent a detailed past.

Troop indices from the game are mapped to identifiers using the `ID_troops.py`,
`ID_factions.py` and `ID_parties.py` files compiled into the server (`src/ids.rs`). The mod
does not change troops, factions or parties; a test enforces this.

## What the prompt contains

The prompt has five separate parts:

1. **Character profile**: canon, then mod traits.
2. **Live game state**, only what the game sent: day; nearest settlement and whether the
   player is at or near it; the NPC's realm and whether they rule it or are its marshal;
   whether they ride with the player, are the player's or someone's prisoner, or are the
   player's spouse or betrothed; whether they are a claimant in exile; the player's sex,
   renown and honour (number plus a word); the player's allegiance (none, vassal,
   mercenary, own realm); personal relation (-100..100); and whether the realms are
   hostile.
3. **Memories**: see "Recall" in the protocol document.
4. **The current conversation** as chat turns, ending with the player's message.
5. **Rules**: first person, 1 to 3 sentences, no narration or speaking for the player;
   knows only what is supplied; does not present invented events between them as fact.
   The NPC has its own goals and need not agree, like, trust or help the player; pride,
   rudeness, evasion and deceit are allowed. Words only: no giving money, goods, troops
   or land.

`--log-prompts` prints every prompt, which is the easiest way to check what an NPC was
told.

## Savegames and campaigns: why memories do not mix

The native Module System exposes no savegame name, no save or load event and no campaign
identifier. So the mod makes its own, stored in global variables that are saved with the
game:

- `$cai_campaign`: a random id, drawn at the first "Speak freely." message of a campaign.
- `$cai_mem_head`: the job id of the last reply the player saw.

Every stored turn remembers the head it was sent with, and memory is the chain of turns
reachable from the current head. Reloading an older save therefore restores an older head,
and the NPC remembers exactly what happened in that save's past: no more, no less. No
reload detection is needed, and no player action is needed to separate branches. A new
campaign has head 0 and remembers nothing. Because branch separation is reliable by
construction, there is no player-facing "new memory branch" option.

Limitations:
- A conversation that happened after the last save is forgotten when that save is
  reloaded. This is intended: it did not happen in the reloaded timeline.
- Old saves are not migrated. A save made before this build has head 0 and gets a
  campaign id at its first message; it simply starts without memories.
- Deleting or replacing the database loses the memories. The NPC then talks without
  memories and the server logs a warning.
- Only "Speak freely." conversations are remembered, not vanilla dialog choices.
- Different NPCs do not share memories, so there is no gossip.

## Delivery and commit policy (summary)

A reply is stored before the game can fetch it and becomes memory only after the game
shows it. The full rules are in "Commit and delivery policy" in
[`protocol-v1.md`](protocol-v1.md#commit-and-delivery-policy). Failed, empty, canceled
and superseded generations are never stored. A reply that was generated but never shown
stays in the database, marked undelivered, and is never recalled.

## Engine assumptions not yet verified in game

These were designed from the Module System sources only. The in-game checklist below
covers them:

1. **`start_presentation` from a dialog consequence.** Native never does this; it opens
   presentations from menus. The option keeps the dialog open (next state `lord_pretalk` /
   `member_pretalk`), so the window should appear over the conversation. If the engine
   defers the window until the conversation ends, it should appear then. If it does not
   appear at all, the fallback is to route through a game menu (`jump_to_menu` plus a menu
   that starts the presentation), which is how Native opens its presentations.
2. **Global variables in saves.** New globals (`cai_campaign`, `cai_mem_head`) are
   assumed to be saved and restored like vanilla globals.
3. **Map distances.** `ldist` is in engine map units. The prompt says "at" for 0-1,
   "near" for 2-10 and "in open country" beyond. The thresholds are a guess to check in
   game.
4. **Names.** `str_store_faction_name` for companions (faction "Commoners") and for
   `$players_kingdom`.

## Manual validation checklist

Build and install the mod while Warband is closed, then run the real model server:

```bash
uv run warband-build --overlay mods/calradia_ai/overlay -o "$GAME/Modules/CalradiaAI"
cargo run --release --manifest-path calradia-server/Cargo.toml -- --log-prompts
```

Before checking identity and context with a model, you can check the plumbing with
`--fake-llm`: its reply names the character, faction, day and relation, and the memories
recalled. Use a scratch `--memory-db` for testing. `--memory-report` shows what is
stored. Record the result and the date for each row.

| # | Step | Expected | Result / date |
|---|---|---|---|
| 1 | Start a new campaign. | | untested |
| 2 | Talk to a lord on the map, and to a companion in your party (for example Borcha, from a tavern). Pick **Speak freely.** | The window opens, titled with the character's name. Vanilla options are all still listed above it. Closing the window returns to "Anything else?". | untested |
| 3 | Ask "Who are you?" | Borcha talks like Borcha ("boss", horses, debts); King Harlaus sounds unlike him. | untested |
| 4 | Ask where you are, what realm they serve, and how they feel about you. | Matches the game: nearest settlement, faction, relation sign, ruler/marshal/prisoner status. With `--log-prompts`, the logged situation matches the character screen. | untested |
| 5 | Tell the NPC something distinctive ("My sister Ylfa keeps an inn in Sargoth"). | | untested |
| 6 | Close the window; end the dialog. | | untested |
| 7 | Speak to the same NPC again (new "Speak freely."). | | untested |
| 8 | Ask "Do you remember my sister?" | The NPC recalls it. The server log shows `1 recent` or `relevant`. | untested |
| 9 | Save, and exit Warband. | | untested |
| 10 | Restart Warband and calradia-server. | | untested |
| 11 | Load the save. | | untested |
| 12 | Speak to the NPC and ask about the sister again. | Still remembered. | untested |
| 13 | Start a new campaign (or load a save from before step 5). | | untested |
| 14 | Speak to the same NPC. | No memory of the sister. `--memory-report` shows a separate campaign (for a new campaign). | untested |
| 15 | Branch check: save, tell the NPC something new, then reload that save without saving and ask about it. | Not remembered. | untested |
| 16 | Cancel while the NPC is thinking, then speak again. | The canceled reply is never mentioned. | untested |
| 17 | Stop the model server (keep calradia-server running) and Say something. | "... could not answer (upstream_unavailable)." Nothing is stored. | untested |
| 18 | Regression: Camp → Talk with Hrodvar; Milestone 2 tests 5-9 in `http-ipc.md`. | Unchanged. | untested |

## What is automated, and what is not

Automated (`cargo test` in `calradia-server`, `uv run pytest`):

| Requirement | Test |
|---|---|
| Correct NPC identification (troop index to stable id, unknown/non-character troops rejected) | `v2_identifies_the_character_and_states_the_live_context`, `v2_request_validation_and_v1_compatibility`, `ids::tests` |
| Different personalities for different characters | `different_characters_get_different_profiles_and_faction_context`, `shipped_profiles_are_valid_and_distinct` |
| Correct faction and relationship context | the two tests above, `situation_states_only_what_the_game_sent` |
| Conversation persistence; memory after server restart | `memory_follows_turns_conversations_and_server_restarts` (two servers, one database file) |
| Separate campaigns and save branches | `savegame_branches_and_campaigns_are_isolated`, `chains_follow_the_savegame_branch` |
| Canceled conversations; undelivered replies | `canceled_and_undelivered_replies_are_not_remembered` |
| Duplicate requests (in memory and across restarts) | `duplicate_v2_talks_run_once_even_across_restarts`, `store_is_idempotent_and_reports_existing_turns` |
| Missing character profiles | `missing_profiles_fall_back_to_live_data`, `generic_profile_uses_the_campaigns_reputation` |
| Corrupt or unavailable memory storage | `unavailable_memory_fails_v2_talks_but_not_v1`, `unusable_databases_are_reported_not_fatal` |
| Oversized contexts | `oversized_context_is_bounded`, `recall_is_bounded_and_deterministic` |
| Model server failure | `model_failures_store_nothing` |
| Protocol compatibility | all Milestone 2 server tests unchanged; `v2_request_validation_and_v1_compatibility`; `test_url_templates_follow_the_protocol`, `test_protocol_constants_match_the_server` |
| Profile files | `rejects_bad_profiles_naming_the_file` |
| Game side: vanilla preserved, dialog lines appended, read-only code, single send site | `tests/test_calradia_ai.py` (plus the golden-file suites, unchanged) |

Manually verified: nothing yet in game. On 2026-09-25 the release binary was run by hand
with `--fake-llm`: a `/v2/talk`, a server restart, then a talk naming the first reply as
its head. The recall worked, as the log and `--memory-report` showed.

Untested until the checklist above is run in Warband: every in-game behavior, the four
engine assumptions above, and the quality of real model output.
