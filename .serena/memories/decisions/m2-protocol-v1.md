# Milestone 2 game<->server protocol v1 (architect decision 2026-09-25, pending lead acceptance)

Could not write an ADR: codebase-memory has no index for this repo (manage_adr "project not found").

## Transport (game)
- rid and job ids are RANDOM ints 1..999,999,999: (r1*30000 + r2 + presentation_ms) mod 900000000 + 1, r1/r2 from store_random_in_range [0,30000). Never a saved monotonic counter: saved counters rewind on savegame load, so an old callback could match a reused rid.
- All send_message_to_url calls live in ONE script (script_cai_tx_send). Send only if $cai_tx_rid == 0.
- Callback classification (copy reg0..reg2 and s0 first). WF means num_ints==3, num_strings==1, reg0==reg2, reg0>0:
  - $cai_tx_rid==0: drop.
  - WF and reg0==$cai_tx_rid: complete the transport and accept the payload.
  - WF and reg0!=$cai_tx_rid: STRAY. Drop it and DO NOT complete the transport.
  - Anything else (empty-body transport failure, malformed): complete the transport as failed.
- Stuck request: if it was sent in the current presentation instance ($cai_tx_inst==$cai_prsnt_inst), there is no reset; tell the player to restart calradia-server (closing the socket produces a callback; experiment E2 must confirm this). If it predates the instance (stale saved flag), a Reset button appears after 15 s of presentation time. This reset is the ONLY path that can overlap requests.
- Conversation state is presentation-scoped: reset on ti_on_presentation_load. Logical cancel never touches the transport; it sets an owed /v1/cancel (best effort).

## Wire
- Request: /v1/{talk,result,cancel}?v=1&rid={reg}&job={reg}[&npc={reg}&day={reg}&pname={s65}&msg={s66}]&end=1. The template is a quick-string operand given directly to send_message_to_url with encode_url=1. Metadata goes first, only as {regN}; free text goes last; end=1 comes last (missing means truncated, code 4). No '_' (quick strings map '_' to space), no spaces, no '^' in templates.
- Response: "R|C|T|R" exactly. C: 0 ready, 1 pending, 2 failed, 3 canceled, 4 bad request (incl. conflict/too long/truncated/bad version), 5 unknown job, 6 busy. T is non-empty, ASCII, contains >=1 letter, has no | { } ^ CR LF, and is <= 500 bytes (word-boundary cut + "..."). The cap applies to T only. Max frame 522 bytes. R=0 if the request rid is invalid.
- Server caps: msg 1..300 chars (reject, never truncate), pname <= 32, target <= 4096 bytes. The server never echoes request text.

## Jobs (server)
- The client-chosen job id is idempotent on (npc,msg,pname,day); different params give 4 "conflict". A new job supersedes (cancels) any pending job for the same npc. The store holds 64 jobs and the queue 8 (overflow gives 6). Terminal TTL is 600 s; deadline 90 s from creation (the game gives up at 120 s). There is 1 worker; no lock is held during I/O; single CAS transition out of PENDING; terminal states are immutable. Cancel shuts down the upstream socket. Unknown or expired gives 5. In memory only (restart gives 5).
- Every /v1 handler answers within 1 s regardless of upstream state (the game's stuck logic relies on this).
- Deps: serde_json only (already in ~/.cargo registry cache); the upstream HTTP client stays std TcpStream (deadline + cancel by shutdown). The NPC registry is a static Rust table; id 1 is the only entry.

## UI
- The camp menu option replaces both M1 options and opens prsnt_cai_talk (appended at the END of presentations; prsntf_manual_end_only). No dialog changes in M2.
- s66 holds the draft (from text-box state_change), s67 holds the reply hand-off (callback, then the next run frame copies it to the overlay). s65 is scratch for pname right before send. Polling every 750 ms.

## Verification rules
- Game-side behaviour is "verified" ONLY by the dated in-game checklist, driven by the server's --fault modes. pytest checks structure and constants only; Rust tests check the server only.
- Pre-implementation experiments: E1 URL-encoding round trip, E2 killing a hung server yields a callback, E3 reply length 500/2000 in the overlay, E4 text-box event timing vs the Say click.
