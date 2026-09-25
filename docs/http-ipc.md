# Warband ↔ external process over HTTP (Milestone 1)

This document describes how the unmodified native Linux Warband executable talks to a
local process. It uses only the engine's built-in operation `send_message_to_url`. There
is no Wine, no WSE, and the binary is not modified.

Each claim below is tagged with how it was established:
- **[live]**: observed in game on 2026-09-25 (Warband 1.174, Ubuntu 24.04, Steam .deb
  with the Legacy runtime).
- **[static]**: established by disassembling `mb_warband_linux` 1.174. The binary still
  exports C++ symbol names in `.dynsym`, so functions can be identified by name.
- **[docs]**: comes from the Module System's own comments.

## The interface

| Item | Details |
|---|---|
| Operation | `(send_message_to_url, <string_id>, <encode_url>)`, opcode 380 (`header_operations.py:130`) |
| Callback | `script_game_receive_url_response`. Inputs: `num_integers`, `num_strings`. Values arrive in `reg0…` and `s0…` |
| Transport | libcurl, loaded from the system at runtime (`libcurl-gnutls.so.4`) |

## Verified behavior

**Request**
- It works in **single player**. No check restricts it to multiplayer. **[static, live]**
- The request is **asynchronous**. Each call starts a new thread. The game never freezes,
  even when the server hangs. **[static, live]**
- The result is queued. `Application::FrameMove` drains the queue on the next frame and
  calls the script once per response, in the order the requests complete. There is no
  request ID. **[static]**
- The request is always an **HTTP GET**. There is no POST and no custom headers. The game
  sent only `Host` and `Accept: */*`, with no `User-Agent`. **[static, live]**
- The URL is the processed string, used as-is. With `<encode_url>` ≠ 0, only the values
  substituted into `{...}` placeholders are percent-encoded, and only `[0-9A-Za-z]` pass
  through unencoded. A literal `^` in the template becomes a newline. Bytes ≥ 0x80 are
  encoded incorrectly because of an engine bug. **[static]**

**Timeouts and errors**
- **No timeout is set.** If the server never answers, the worker thread stays blocked and
  the callback never runs. Observed: about a minute of normal play with the request still
  unanswered, and no side effects. **[static, live]**
- **HTTP status codes are ignored.** A 4xx or 5xx body is delivered like any other body,
  so the server must put the status in the body. **[static]**
- A **transport failure** (connection refused, DNS, empty reply) still calls the callback,
  with an empty body. The engine parses it as `num_integers = 1`, `reg0 = 0`,
  `num_strings = 0`. DNS failure also logs "Please check your internet connection." The
  in-game "could not be reached" message was confirmed with the server stopped.
  **[static, live]**

**Response parsing** **[static, docs]**
- The body is split on `|` with no trimming.
- A field is an integer if it is empty, or an optional `-` followed by digits. CR and LF
  are ignored. Any other field is a string.
- Integers go to `reg0…` and strings to `s0…`, at most 128 of each.

**Concurrency** **[static]**
- The response body is collected in one global buffer (`curl_return_data`) **without a
  lock**. Two requests in flight at the same time can corrupt each other.
- **Rule: at most one request in flight.** The mod enforces this with a single send
  site, `script_cai_tx_send`, which sends only when `$cai_tx_rid` is 0. See
  `docs/protocol-v1.md`.

## Protocol used by CalradiaAI

Milestone 1 used a simple `<status>|<message>` reply. Milestone 2 replaced it with the
protocol in [`protocol-v1.md`](protocol-v1.md): `R|C|T|R` frames, random request and job
ids, submit/poll/cancel jobs, and one sanitizer.

## Findings from in-game tests (Milestone 2, 2026-09-25)

- **Placeholders inside substituted text are expanded again.** **[live]**
  - A player message containing `{s0}` reached the server with `{s0}` replaced by the
    contents of string register s0. A `^` became a newline.
  - So braces and carets that the player types are interpreted by the engine, in both
    the request and the "You:" line.
  - This can only insert other text registers into the message, never change game state,
    and the server sanitizes whatever arrives.
  - Vanilla has no string operation to escape braces, so this is a known limitation.
- **The simple text box delivers its text as the player types.** Clicking Say without
  pressing Enter sends the current text. **[live]**
- **Logical cancel works as designed.** The owed `/v1/cancel` arrived about 0.6 s after
  the talk, the job became CANCELED, and the model's late result was discarded.
  **[live]**

## Limitations to design around in later milestones

- **GET only, and URL length.** Prompts must travel in the query string. The engine has
  no length cap, but libcurl, the server and the URL encoding all add limits. Use short
  IDs and let the server hold the context.
- **One request at a time, and no request ID.** Queue requests in module scripts, or put
  a correlation token in the response.
- **No timeout.** The player needs a way out, like the "Stop waiting…" option, or a
  game-time based expiry.
- **Menus aren't redrawn when a reply arrives.** A condition checked on an open menu is
  stale until the menu redraws.
- **Game-side text encoding:** stick to ASCII in both directions for now.

If these limits become blocking, there are few Linux-native alternatives that leave the
executable unmodified. `header_operations.py` defines no other file, socket or network
operation for module scripts. Its only I/O-related entries are `send_message_to_url` and
`str_encode_url`. Any other channel would have to come from outside the engine, such as
`LD_PRELOAD`-based interception. That counts as binary-level injection and needs a
separate decision.

## Test tools

- `calradia-server --fault {hang,close,empty,wrong-rid,malformed,delay,oversize-N,nonascii}`
  injects transport and protocol failures. It prints a loud banner when a fault is on.
- `calradia-server --fake-llm` answers in character without calling a model.
- `cargo test --offline --manifest-path calradia-server/Cargo.toml` runs the automated
  suite, which uses a fake upstream.
