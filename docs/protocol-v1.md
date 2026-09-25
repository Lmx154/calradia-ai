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
| (c) | WF and reg0 ≠ tx_rid | A stray reply: drop it and do NOT complete the transport. |
| (d) | anything else (empty body = transport failure, or malformed) | Complete the transport as failed. |

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
