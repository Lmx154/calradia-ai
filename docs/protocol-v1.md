# Calradia AI protocol v1 (game ↔ calradia-server)

This is the binding contract between the Warband mod (`mods/calradia_ai/overlay`) and
`calradia-server`. The engine facts it relies on are in `docs/http-ipc.md`.
Constants live in `calradia-server/src/protocol.rs`, which is the source of truth. The
mod mirrors them, and `tests/test_calradia_ai.py` checks that the two agree.

## Transport

- Every request is `GET http://127.0.0.1:8766/v1/<op>?...`, sent through the one engine
  operation `send_message_to_url` with encode_url = 1.
- At most one request is outstanding at a time (see "Game state machine").

### Request URLs

```
/v1/talk?v=1&rid={regA}&job={regB}&npc={regC}&day={regD}&pname={s65}&msg={s66}&end=1
/v1/result?v=1&rid={regA}&job={regB}&end=1
/v1/cancel?v=1&rid={regA}&job={regB}&end=1
```

**Template rules:**
- The template is the quick-string operand of `send_message_to_url` itself. It is never
  pre-built with `str_store_string`, which would substitute values without encoding them.
- Metadata goes only in `{regN}` placeholders; digits pass through encoding unchanged.
- Free text goes only in `{sN}` placeholders, placed after all metadata.
- `end=1` comes last. `rid` comes second, so it survives truncation.
- Templates contain no `_`, no spaces and no `^`: quick strings store spaces as `_`.

**Server input rules**, applied after percent-decoding (`+` is decoded as space):
- `v` must equal 1.
- `rid` and `job` must be integers in 1..=999999999. `npc` must be a known NPC id.
  `day` must be in 0..=100000.
- `end=1` must be present. Otherwise the server answers code 4, with reason
  `truncated`.
- The raw request target must be at most 4096 bytes.
- `msg`, after sanitizing, must be 1..=300 characters. It is rejected (code 4
  `too_long` / `empty_msg`) and never truncated.
- `pname` is truncated to 32 characters.
- Input sanitizing is the same as output sanitizing, except that leftover non-ASCII
  becomes `?` and braces are kept.

## Response frame

The HTTP status is always 200 and the body is exactly `R|C|T|R`, with no CR or LF and no
trailing newline.

- **R** is the echoed rid, 1..=999999999. It is 0 if the request's rid was missing or
  invalid.
- **C** is the code:

| Code | Meaning |
|---|---|
| 0 | ready (T is the NPC's reply) |
| 1 | pending |
| 2 | failed |
| 3 | canceled |
| 4 | bad request (conflict, too_long, empty_msg, truncated, bad_version, bad_param) |
| 5 | unknown job (also after expiry or a server restart) |
| 6 | busy (store or queue full) |

- **T** is never empty. When C = 0 it is the reply text; otherwise it is a short reason
  token, such as `pending`, `timeout`, `upstream_unavailable`, `empty_reply`,
  `superseded` or `canceled`.
- The frame is at most 522 bytes. R and C are never truncated.

### `sanitize_text`

This one function is applied to every T, in this order:

1. Strip `<think>…</think>`, and an unclosed `<think>` through the end of the text.
2. Transliterate using a fixed table:
   - curly quotes become `'` or `"`;
   - dashes become `-`, and `…` becomes `...`;
   - Unicode spaces become a normal space;
   - Latin-1 and Latin Extended-A letters become their base letter (é→e, ß→ss, æ→ae).
3. Drop any other non-ASCII.
4. Replace control characters (0x00–0x1F and 0x7F) with a space.
5. Replace `|` with `/`, `{` with `(`, `}` with `)`, and `^` with a space.
6. Collapse whitespace runs and trim.
7. Cap at MAX_TEXT = 500 bytes. Cut at the last space at or before byte 497, or hard-cut
   there if that space is before byte 400, then append `...`.
8. If the result contains no letter [A-Za-z], the job FAILS with reason `empty_reply`.
   This rule guarantees T can never parse as an integer.

## Jobs (server)

- **Key:** the client job id.
- **Idempotency:** a talk with the same job id and the same fingerprint (npc, msg,
  pname, day) returns the job's current state and starts no new inference. The same job
  id with different parameters gets code 4 `conflict`.
- **Supersede:** a new job cancels any PENDING job for the same npc, with reason
  `superseded`.
- **States:** PENDING → READY | FAILED(reason) | CANCELED(reason).
  - Terminal states are immutable, and transitions happen under one mutex.
  - No lock is held during I/O.
  - Results that arrive after a job has left PENDING are discarded and logged.
- **Limits:**

| Limit | Value |
|---|---|
| Stored jobs | 64. When full, purge expired jobs first, then evict the oldest terminal job; otherwise answer code 6. |
| Queue / workers | queue of 8, 1 worker |
| Terminal job lifetime | 600 s |
| Deadline | 90 s from creation, enforced lazily on every access and by the worker |
| Upstream connect timeout | 2 s |
| Upstream read/write timeout | the time left before the deadline |
| Upstream response body | at most 256 KiB |

- **Cancel:** a PENDING job becomes CANCELED, and its upstream socket is shut down. A
  terminal job is left unchanged; the answer carries its code and a token. An unknown job
  gets code 5.
- **Result:** READY returns the reply, repeatedly, until the job expires. An unknown or
  expired job gets code 5.
- **Restart:** jobs are in memory only, so after a restart every old job answers code 5.
- **Latency:** every `/v1` handler answers within 1 s, even while the upstream is hung.
- **Side effects:** the endpoints never change game state. The server only reads the
  request and calls the model.

## Game state machine

- **Callback:** it copies reg0–reg2 and s0 before calling any other script. A frame is
  well-formed (WF) when num_ints = 3, num_strings = 1, reg0 = reg2 and reg0 > 0. Then:

| Case | Condition | Action |
|---|---|---|
| (a) | tx_rid = 0 | Drop the frame. |
| (b) | WF and reg0 = tx_rid | Complete the transport, then deliver the frame. |
| (c) | WF, reg0 ≠ tx_rid, and `$cai_tx_abandoned` > 0 | A late reply to a request abandoned by Reset: decrement the count, drop the frame, and do NOT complete the transport. |
| (d) | anything else (empty body = transport failure, malformed, or WF with the wrong rid when nothing is abandoned) | Complete the transport as failed. |

Amended on 2026-09-25 after in-game testing with `--fault wrong-rid`. The original rule
(c) dropped *every* wrong-rid frame, which left the transport busy with no request left
to complete it: the engine makes exactly one callback per request, and the mod sends only
one at a time. Reset increments `$cai_tx_abandoned`.
Case (b) must explicitly check that the received rid matches `tx_rid`; checking only
WF would accept a wrong-rid frame before it can reach case (d). The matching guard
was restored on 2026-09-25 after reviewing the initial amendment in `bef22d8`.

- **Sending:**
  - The only send site is `script_cai_tx_send`, and it sends only when `$cai_tx_rid`
    is 0.
  - rid and job ids are random: (r1*30000 + r2 + presentation_ms) mod 900000000 + 1.
  - A new rid is drawn again if it equals the previous one.
- **Stuck request:**
  - *Sent in the current presentation instance:* after 5 s, show "calradia-server is not
    answering; restart it". There is no reset; a server restart fires case (d).
  - *Predates the current instance:* count its age from presentation load. After 15 s,
    offer a Reset button labelled "restart calradia-server first", which sets tx_rid to 0.
- **Conversation:**
  - It is reset on every presentation load.
  - Logical cancel sets the conversation to CANCELED and marks a cancel as owed. It never
    touches the transport. The owed `/v1/cancel` is sent the next time the transport is
    idle.
  - Polls run every 750 ms while PENDING and the transport is idle. The game gives up
    after 120 s, which is longer than the server's 90 s deadline.
- **Registers:**
  - s66 holds the draft message.
  - s67 carries the reply from the callback to the presentation, together with the
    `$cai_reply_new` flag.
  - s65 is scratch for pname, set right before sending.

## Protocol v2: characters, live context and memory (Milestone 3)

v2 is additive. It adds one route, `/v2/talk`, used when the player picks **Speak freely.**
in a conversation with a lord or companion. `/v1/talk` (Hrodvar), `/v1/result` and
`/v1/cancel` are unchanged, and `/v1/result` and `/v1/cancel` serve jobs of both talk
versions. The response frame, the job states, the limits, the game state machine and the
callback are the same as in v1. A v1 client keeps working against a v2 server.

### Request URL

```
/v2/talk?v=2&rid={regA}&job={regB}&camp={regE}&conv={regF}&head={regG}&troop={regH}&day={regD}&fac={regI}&pfac={regJ}&frel={regK}&rel={regL}&rep={regM}&occ={regN}&st={regO}&ren={regP}&hon={regQ}&loc={regR}&ldist={regS}&pg={regT}&wars={regU}&f=2&whead={regV}&gold={regW}&pgold={regX}&hres={regY}&pname={s65}&nname={s50}&fname={s51}&pfname={s52}&lname={s53}&ruler={s54}&spouse={s55}&father={s56}&msg={s66}&end=1
```

The template rules of v1 apply. `script_cai_store_context` fills reg43..reg59 and
s50..s56 right before the send. Every value is read from the game at that moment:

| Field | Game source | Server rule |
|---|---|---|
| `v` | | must equal 2 (`/v2/talk` with `v=1`, or `/v1/*` with `v=2`, is `bad_version`) |
| `camp` | `$cai_campaign` | 1..=999999999 |
| `conv` | `$cai_conv_id`, drawn at the first Say of each talk window | 1..=999999999 |
| `head` | `$cai_mem_head`: job id of the last reply shown in this save, 0 if none | 0..=999999999 |
| `troop` | `$cai_talk_troop` (`$g_talk_troop` when the option was picked) | index into `ID_troops.py`; must be a companion, king, lord, pretender or lady |
| `day` | `store_current_day` | 0..=100000 |
| `fac` | `store_troop_faction` of the NPC | index into `ID_factions.py` |
| `pfac` | `$players_kingdom` (0 = none) | index into `ID_factions.py` |
| `frel` | `store_relation` of `fac` and `fac_player_faction` (< 0 = hostile) | signed |
| `rel` | NPC's `slot_troop_player_relation` (-100..100) | signed |
| `rep` | NPC's `slot_lord_reputation_type` (lrep_*) | unsigned |
| `occ` | NPC's `slot_troop_occupation` (slto_*) | unsigned |
| `st` | status bits, below | unsigned |
| `ren` | player's `slot_troop_renown` | signed |
| `hon` | `$player_honor` | signed |
| `loc` | the town, castle or village nearest to `p_main_party` (0 = none) | index into `ID_parties.py` |
| `ldist` | `store_distance_to_party_from_party` to it (map units) | unsigned |
| `pg` | `troop_get_type` of the player (1 = female) | 0 or 1 |
| `wars` (optional) | the active realms (`kingdoms_begin`..`kingdoms_end`) at war with the NPC's faction: bit k = `fac_player_supporters_faction` + k | 0..=127; if absent, nothing is said about wars |
| `pname`, `nname`, `fname`, `pfname`, `lname` | names of the player, NPC, NPC's faction, player's faction (empty if none), location | sanitized; `pname` cut to 32, the others to 40 characters |
| `ruler`, `spouse`, `father` (optional) | names of the NPC's liege (leader of the NPC's realm, when that is not the NPC), spouse (other than the player) and father (`slot_troop_father`) | as the names above; empty or absent = none |
| `msg` | the player's message | as in v1 |

`wars`, `ruler`, `spouse` and `father` were added after the first v2 build and are
optional, so a mod built from that commit still talks to this server. Signed values arrive
as `-N` (`%2DN` on the wire). Every integer must lie within
±`MAX_STAT` (1000000); anything missing or out of range is `bad_param`.

Status bits (`st`), mirrored in `module_scripts.py` as `CAI_ST_*`:

| Bit | Meaning |
|---|---|
| 1 `ST_IN_PARTY` | the NPC is in the player's party (`main_party_has_troop`) |
| 2 `ST_PLAYER_PRISONER` | the NPC is held by the player's party |
| 4 `ST_OTHER_PRISONER` | the NPC is held by another party |
| 8 `ST_FACTION_LEADER` | the NPC leads its faction |
| 16 `ST_MARSHAL` | the NPC is its faction's marshal |
| 32 `ST_SPOUSE` | the NPC is the player's spouse |
| 64 `ST_BETROTHED` | the NPC is betrothed to the player |
| 128 `ST_PLAYER_VASSAL` | the player has a kingdom and `$player_has_homage` = 1 |
| 256 `ST_PLAYER_RULER` | `$players_kingdom` is the player's own active kingdom and the player leads it |

The v2 URL is about 330 bytes plus the encoded names and message, well under
`MAX_TARGET`. The campaign state itself stays in the game; only these values travel.

### Jobs

- **Idempotency (in memory):** as in v1, keyed by job id. Two v2 talks are the same talk
  when every field above is equal.
- **Supersede:** a new v2 talk cancels the PENDING job of the same character in the same
  campaign.
- **Durable idempotency:** before generating, the worker looks up (camp, job) in the memory
  database. If it is stored with the same parameters, the stored reply is the answer, with
  no new generation and no new record (a retried talk after a server restart). If it is
  stored with other parameters, the job fails with `conflict`.
- **New FAILED reasons:** `memory_unavailable` (the database could not be opened, checked or
  migrated; see `--memory-db`), `memory_error` (a query failed), `conflict` (above).

## Memory

The server keeps conversation memory in SQLite (`calradia-server/src/memory.rs`; default
`$XDG_DATA_HOME/calradia-ai/memory.sqlite3`, or `~/.local/share/...`). Tables:
`campaigns`, `conversations` (one per talk window, keyed by (camp, conv)) and `turns`
(one player message and the NPC's reply, keyed by (camp, job)). Every write happens in one
transaction; the stored reply is exactly the text the game is sent.

### The memory chain

Each turn stores the `head` that its talk carried as `parent_job`. The game sets
`$cai_mem_head` to a job id **only after it has shown that reply** in the talk window, and
the variable is saved with the game. So the turns reachable from a head through
`parent_job` are exactly the replies the player saw in the history of that savegame, and
nothing else:

- **Save branches.** Reloading an older save sends an older head. Replies from the
  abandoned branch are not on the chain and are never recalled, even though they stay in
  the database. Nothing has to detect the reload.
- **Campaigns.** A new campaign starts with `$cai_campaign` = 0 and `$cai_mem_head` = 0.
  The campaign id is drawn at the first message and saved with the game. A new campaign
  has an empty chain, and turns are looked up by (camp, job), so campaigns cannot share
  memories even if two of them drew the same id.
- **Unknown head.** If the head is not stored (a deleted or different database), the server
  logs a warning and the NPC talks without memories. Nothing is mixed in.

### Commit and delivery policy

1. A successful, non-empty reply is stored **before** the job becomes READY, so the game
   can never show a reply that memory lacks. If storing fails, the job FAILS with
   `memory_error` and nothing is shown.
2. Failed, timed-out, empty and canceled generations are never stored. A reply that
   arrives after its job was canceled, superseded or timed out is not stored either.
3. A stored reply is **generated**, not read. It enters memory only when the player has
   seen it: the game shows it, makes it the head, and the next talk carries that head
   (`delivered_at` is set then). A reply that was stored but never shown (the player
   canceled or closed the window before the poll returned it, or the server restarted
   before the poll) is never recalled.
4. A reply shown but not saved (the player quits without saving) is forgotten on reload,
   because the saved head is older. This matches what the reloaded save experienced.

### Recall

For each v2 talk the worker recalls, from the character's turns on the chain (at most
`MAX_CHAIN` = 5000 turns are followed):

- the conversation in progress: its last 8 turns, as chat messages;
- the last 4 turns of earlier conversations;
- up to 3 older turns sharing the most keywords (words of 4+ letters, minus common ones)
  with the player's message, newer first among equals;
- when the character first spoke with the player, and how many times.

Selection is deterministic. There are no embeddings or summaries, and records are never
rewritten. The prompt (profile, live state, memories, rules and chat) is kept under
12000 characters by dropping relevant memories, then recent ones, then the oldest turns
of the conversation in progress.

## Protocol v2, Milestones 4-6: world nodes, actions and initiatives

### The v2 frame

Every `/v2/event`, `/v2/world` and `/v2/tick` answer, and every answer about a `/v2/talk`
sent with `f=2` (including `/v1/result` and `/v1/cancel` for that job), is a **v2 frame**:

```
R|C|T|K|N|W|X|R
```

It is the v1 frame with four integers before the trailing R, so the engine delivers
`reg0 = R, reg1 = C, reg2..reg5 = K N W X, reg6 = R, s0 = T` (7 integers, 1 string). The
callback accepts a frame as well-formed with 3 or 7 integers. K..X are 0 unless stated.
Every rule for R, C and T is unchanged; the frame is at most 580 bytes. v1 routes, and v2
talks without `f=2` (a mod built before Milestone 5), keep v1 frames.

### World nodes (Milestone 4)

The game's background sender (`script_cai_background_send`, one simple trigger that
runs every map frame) sends at most one node at a time, only while the transport is idle
and the talk window is closed:

```
/v2/event?v=2&rid={regA}&job={regB}&camp={regE}&whead={regV}&day={regD}&idx={regZ}&type={regAA}&time={regAB}&actor={regAC}&center={regAD}&clord={regAE}&cfac={regAF}&troop={regAG}&tfac={regAH}&fac={regAI}&pname={s65}&pfname={s52}&end=1
/v2/world?v=2&rid={regA}&job={regB}&camp={regE}&whead={regV}&day={regD}&alive={regAJ}&pname={s65}&pfname={s52}&wars={s53}&owners={s54}&lords={s55}&lords2={s56}&end=1
/v2/tick?v=2&rid={regA}&job={regB}&camp={regE}&whead={regV}&day={regD}&head={regG}&pname={s65}&pfname={s52}&end=1
```

(From here on, `{regX}` letters simply name registers; see the overlay for which register
each stands for. `job` is the node's id, drawn like a job id.)

- **`/v2/event`** forwards Native's log entry number `idx` (`$cai_log_sent + 1`, up to
  `$num_log_entries`), read from the `trp_log_array_*` slots: type, time in hours,
  actor, center, center lord, center faction, troop, troop faction, faction (indices,
  -1 for none). The server turns it into one sentence using
  `calradia-server/world/log_entries.toml` (one entry per `logent_*` type, with who is
  involved, which realms it is news for, and its importance; noise and never-logged
  types are skipped).
- **`/v2/world`** is the daily snapshot: `alive` (bit k = realm `kingdoms_begin + k`
  active), and lists of `.`-separated numbers (the game builds them digit by digit):
  `wars` (1 per realm pair i < j at war, 21 values), `owners` (faction of each of the 70
  walled centers), `lords` + `lords2` (faction * 2 + 1 if held prisoner, for the 132
  kings, lords and claimants, in two halves). The server compares it with the previous
  snapshot on the same chain and records what changed: wars and peace, realms falling or
  rising, towns and castles changing hands, lords changing allegiance, lords captured or
  released. The first snapshot is a baseline.
- **`/v2/tick`** is the daily tick (Milestone 6, below). `head` is the conversation head.

Each node's parent is `whead` (`$cai_world_head`), exactly like conversation turns: the
game makes a node its world head only after the server answered READY, and that head is
saved with the game, so a reloaded save knows only the events of its own past. A retried
node (same id, same content) gets the same answer; the same id with other content is code
4 `conflict`, and the game then draws a new id. After a failure the sender waits one game
hour; a node rejected twice is skipped, so one bad entry cannot block the rest.

`/v2/talk` carries `whead`. The talk's prompt then lists what the character knows
(`world::recall`, at most 10, oldest first): events that involved them (their last 5),
news of their realm (last 4), notable deeds of the player (last 3), and major events of
the last 30 days (last 3).

### Actions (Milestone 5)

With `f=2`, a talk's prompt tells the character it may end its reply with one line
`ACTION: <deed> <number>`: `relation -3..3` (its regard for the player), `give <denars>`
(nobles only, from their purse), `ask <denars>` (from the player). The server strips the
line from the spoken text and validates it (`calradia-server/src/actions.rs`):

| Rule | Value |
|---|---|
| relation | 1..3 either way; at most 5 in total per character per game day |
| give | 10..1000 denars; a noble (king, lord, claimant, lady) not held by the player, with at least twice that in `gold` |
| ask | 10..5000 denars; the player carries at least that (`pgold`) |
| gold cooldown | one give or ask per character per 3 game days |

A valid action rides on the READY frame: `K` = `ACT_RELATION` 1 / `ACT_GIVE` 2 /
`ACT_ASK` 3, `N` = amount, `W` = the character's troop. The game checks again that W is the
character in the window. A relation change is carried out when the reply is shown; gold
waits for **Accept** / **Refuse**. `script_cai_execute_action` (the only mod script that
writes game state) re-checks the bounds and the purses and executes with vanilla
operations and scripts. The outcome (`OUT_ACCEPTED` 1, `OUT_DECLINED` 2, `OUT_FAILED` 3)
goes out with the next talk as `hres` and is stored on that turn, so memory records what
each branch of the save did with the offer. `--no-actions` turns proposals off.

### Initiatives (Milestone 6)

A tick is a world node that also (a) may carry one **initiative** that a character
decided on earlier, and (b) queues one planning task. Planning runs in the background and
yields to any talk (a talk arriving stops it). It picks the stalest "important" character
(a ruler or claimant with a profile, or anyone the player has spoken with), with no plan
or a plan at least 7 days old on this chain. It asks the model, as that character's
private mind, for JSON: a goal, a plan, and optionally one act:

| Act | K | N | X | Game effect |
|---|---|---|---|---|
| letter | `INIT_LETTER` 10 | 0 | 0 | a letter to the player (`dialog_box`) |
| attitude | `INIT_ATTITUDE` 11 | -2..2 | 0 | the character's regard for the player changes, with a letter |
| rivalry | `INIT_RIVALRY` 12 | -2..2 | the other lord's troop | relation between the two lords changes; a rumour line is shown |

T is the letter or rumour; W the character. Acts are validated when planned (bounds,
text, a real target lord other than itself) and again by the game. An initiative is
delivered only if it was decided on this save's chain, within 10 days, not already
delivered on this chain, at most one per game day, and at most one per character per 5
days. Plans and recent deeds go into that character's conversation prompt ("Your private
aims", "What you did of your own accord lately"). `--no-autonomy` turns planning and
delivery off. Defections, war declarations and army movements are not among the acts.
