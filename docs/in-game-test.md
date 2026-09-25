# In-game test of Calradia AI (Milestones 3-6)

One play session covers every in-game check of Milestones 3 to 6, with the real model
(Qwen 3.5 9B through llama-swap). It takes about an hour. The work is split in two:

- **You (the player)** play and say what you saw: for example "step 4 ok", or "step 9:
  nothing appeared". A screenshot helps when something looks wrong.
- **The agent on your desktop** prepares everything before you start, runs
  calradia-server while you play, restarts it when a step needs that, and checks the
  server's side of each step (`uv run python tools/calradia_check.py report`). It fills
  in the Result column with the date. Its instructions are in [`CLAUDE.md`](../CLAUDE.md).

Model replies vary. A step that depends on what the model chooses (an action, a letter)
may need a second try. The agent can see in the log whether the model proposed something
and why the server refused it.

## Before you start (agent)

1. `uv run python tools/calradia_check.py all` passes (tests, install, pipeline). Its
   warnings, if any, are passed on to the player.
2. `uv run python tools/calradia_check.py serve` is running (port 8766, real model,
   prompts logged).
3. The player starts Warband, picks **CalradiaAI** in the launcher, and starts a
   **new game**. Saves from an older build of the mod are not supported.

## Part 1: conversations and memory (Milestone 3)

| # | You do | You should see | Agent checks | Result / date |
|---|---|---|---|---|
| 1 | Camp → **Talk with Hrodvar.** Say "Hello". | Hrodvar answers. | `report`: one reply. | untested |
| 2 | Hire a companion from a tavern. Open the party screen, select them, **Talk**, and pick **Speak freely.** (the last option). Ask "Who are you?" | A window titled with their name. They answer in their own voice: Borcha says "boss", talks of horses and debts, and so on. All vanilla options are still listed above "Speak freely.". | The talk is logged for that `trp_npc*` with its profile. The reply fits the profile in `calradia-server/characters/`. | untested |
| 3 | Close the window. | Back in the conversation at "Anything else?". | | untested |
| 4 | Ride up to any lord, talk, and pick **Speak freely.** Ask "Where are we, whom do you serve, and what do you think of me?" | The answer matches the game: the nearest settlement, the lord's realm, and warmth or coldness matching your relation (see the character screen). | The logged prompt's situation matches the lord's realm, relation and location. | untested |
| 5 | Tell the lord: "My sister Ylfa keeps an inn in Sargoth." Close the window and leave the conversation. | | | untested |
| 6 | Talk to the same lord again, **Speak freely.**, and ask "Do you remember my sister?" | He remembers (Ylfa, the inn, Sargoth). | The talk recalled 1 recent turn. | untested |
| 7 | Save (slot A) and quit Warband. **Agent:** `stop`, then `serve` again. Start Warband, load slot A, and ask the same lord about your sister again. | Still remembered. | The memory database shows the campaign. The talk recalled the earlier turns. | untested |
| 8 | Save (slot B). Tell the lord "I buried a chest of silver under the old oak by the river." Close. Load slot B **without saving**. Ask him "What did I tell you about a chest of silver?" | He knows nothing about a chest. | The recall does not include the chest turn: it is not on slot B's chain. | untested |
| 9 | Say something and click **Cancel** while the lord is thinking. Then ask "What did you just say?" | The canceled reply is never mentioned. | The canceled job is logged CANCELED and not recalled. | untested |
| 10 | **Agent:** `stop`, then `serve --skip-model-check --upstream http://127.0.0.1:9/v1`. Say something to any character. | "... could not answer (upstream_unavailable)." No crash. | FAILED upstream_unavailable; nothing stored. Then `stop` and `serve` again. | untested |

## Part 2: the world (Milestone 4)

| # | You do | You should see | Agent checks | Result / date |
|---|---|---|---|---|
| 11 | Travel the map for 2 or 3 game days. | No stutter, and no error messages. | World nodes are stored each day (`report`, memory database). Any news appears under "World news". | untested |
| 12 | **Agent:** name a realm that `report` lists news for (a war, a peace, a castle taken, a lord captured or defecting). Talk to a lord of that realm and ask "What news from the realm?" | He knows the news that concerns him and his realm, and does not invent other news. | The talk's prompt lists those events (`logs/server-*.log`, with `--log-prompts`). | untested |
| 13 | **Agent:** `stop` for about one game day while you travel, then `serve` again. | Nothing unusual in game. | Forwarding resumes: new world nodes appear after the restart. | untested |

## Part 3: actions (Milestone 5)

Actions come from the model. If nothing happens, try again once or twice in other words.
The agent can tell you whether the model proposed something that the server refused (for
example because the purse was too small).

| # | You do | You should see | Agent checks | Result / date |
|---|---|---|---|---|
| 14 | Insult a lord ("You are a coward and a fool"). | Sometimes Native's message that your relation with him has changed, and the character screen shows it. | `Actions`: a validated kind 1 (regard), or a refusal with its reason. Never more than 5 in total for one lord on one game day. | untested |
| 15 | Ask a friendly, rich lord (a king is best) for money: "My men are starving, my lord. Could you spare 200 denars?" | If he offers: the status reads "... offers you N denars.", **Accept** and **Refuse** appear, and **Say** is faded. **Accept** adds N denars to your gold. | A validated kind 2 (gift of gold). The next talk reports it as accepted. | untested |
| 16 | Tell a companion "If you are short of coin, tell me how much you need." If they ask, click **Refuse**. Talk again and ask "Are you angry about the money?" | The status reads "... asks you for N denars.". After **Refuse**, no gold moves, and they know you refused. | A validated kind 3 (request for gold). The next prompt says "; refused". | untested |
| 17 | When any character offers or asks for gold, close the window instead of answering. | No gold moves. | The next talk reports it as refused. | untested |

## Part 4: characters with aims of their own (Milestone 6)

| # | You do | You should see | Agent checks | Result / date |
|---|---|---|---|---|
| 18 | Play about a week of game time (travel, fight, trade). | At most one letter a day from a ruler, a claimant or someone you spoke with. Most days bring none. Letters read in character. A change of attitude comes with a letter; a quarrel between lords comes as a rumour line. | `Planning`: one plan a day. Few answers are rejected, and none are cut off. Initiatives delivered match the letters you saw. | untested |
| 19 | Talk to a king (or a lord you spoke with before) and ask "What do you want most now?" | An answer consistent with the plan the agent found, and in character. | The talk's prompt contains "Your private aims". | untested |
| 20 | While a plan is being made (the agent says when the log shows "planning for ..."), talk to someone. | The reply is not delayed by the planning. | "background task preempted by a talk" in the log. | untested |

## Part 5: nothing else changed

| # | You do | You should see | Agent checks | Result / date |
|---|---|---|---|---|
| 21 | Use lords' and companions' normal dialogs: quests, recruiting, joining a realm, trading. | All as in Native. "Speak freely." is only ever the last option. | No errors in the log. | untested |
| 22 | Optional: start another new game and talk to a lord from Part 1. | No memory of you. | A second campaign in the memory database. | untested |

## Known open points to watch

- Opening the window from a conversation (`start_presentation` from a dialog) has never
  been done in Native. If the window does not appear, or appears only after the
  conversation ends, report it: the fallback is to open it through a game menu.
- The map distance words ("at", "near", "in open country") and the text box and button
  positions are guesses until this test.
