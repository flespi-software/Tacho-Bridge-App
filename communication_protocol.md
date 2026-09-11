# TBA <-> Server Communication Protocol

This document describes how the Tacho Bridge App (TBA) communicates with the
server: what connections it opens, which messages travel over them, when and
why. It documents the **transport envelope** implemented by TBA. TBA is a
bridge: the server decides _what_ to do; TBA executes and reports back.

Status: living document. Sections marked **TBD** are planned and will be
filled in as the corresponding functionality lands.

## 1. Overview

TBA connects tachograph company cards (in local PC/SC readers or in a
multi-slot card rack on a serial port) to the server over MQTT. All exchanges
follow one rule:

> **The server initiates; TBA answers.** Every server message gets exactly one
> response from TBA. TBA never decides on its own what to send to a card or to
> a serial device — it forwards what the server built and returns what came
> back.

## 2. Transport

- MQTT v5 (`rumqttc`), host and port taken from the app configuration
  (`server.host`, entered in the UI).
- Keep-alive: 120 seconds.
- Reconnect: exponential backoff, 10 s initial, doubling up to a 300 s cap,
  reset after a successful connection.

## 3. Connection types

TBA maintains several MQTT connections in parallel, one per role. Each has its
own `client_id`, which is how the server tells them apart.

| Connection | client_id                                   | Purpose                                                           |
| ---------- | ------------------------------------------- | ----------------------------------------------------------------- |
| App        | `TBA` + 13 digits (e.g. `TBA1740000000000`) | Application presence: one per running TBA instance; also carries the traffic of every connected card rack (section 6). |
| Card       | 16-character company card number            | One per inserted card; carries the card's authentication traffic. |

Lifecycle: a card connection is opened when a configured card is inserted (in
a reader, or in a rack slot the server told TBA to serve) and closed when it
is removed. A serial card rack has no connection of its own: it is a
peripheral of the application, like a reader, addressed on the app
connection under the `rack/<serial>/` topic prefix from the moment its port
is open until it disappears.

## 4. Request/response model

The server sends a command as an MQTT PUBLISH to a topic of the form:

```
request/<request_id>/<sender>
```

- `<request_id>` — a number identifying the exchange;
- `<sender>` — the id of the platform entity the exchange belongs to.

TBA executes the command and publishes exactly one reply to the same topic
with the first segment replaced:

```
response/<request_id>/<sender>
```

### Idempotency

The server may re-send a command with the same `request_id` (e.g. after a
delivery delay). TBA remembers the last answered `request_id` per connection
and, on a repeat, re-publishes the cached response instead of executing the
command again. The cache is reset on every reconnect (CONNACK).

## 5. Card connection payloads

All payloads are JSON. Incoming command shape:

```json
{ "finish": false, "payload": "<hex>", "protocol": "T1" }
```

`protocol` is optional (see below); old servers do not send it.

| `finish` | `payload` | Meaning                                                                                                           | TBA reply                                      |
| -------- | --------- | ----------------------------------------------------------------------------------------------------------------- | ---------------------------------------------- |
| `false`  | empty     | Session start: the server asks for the card's ATR. Also aborts a previous unfinished session (the card is reset). | `{ "payload": "<ATR hex>", "protocol": "T0" }` |
| `false`  | APDU hex  | Transmit the APDU to the card.                                                                                    | `{ "payload": "<card response hex>" }`         |
| `true`   | —         | Authentication session finished. TBA records the result and resets the card.                                      | `{ "payload": "" }`                            |

On an unrecoverable card error TBA replies with the standard status word
`6F00` so the server always gets an answer.

### Card communication protocol (T0/T1)

The T protocol is a property of the (card, vehicle unit) pair, so the server
owns it per tracker and drives it per session; TBA-local state is only a
fallback. Sources in priority order:

1. **Server-requested (per session).** An incoming command may carry an
   optional `"protocol": "T0" | "T1"` field. TBA honors it **only on session
   start** (empty payload): switching means a physical card reset, so a
   differing value in a mid-session command is ignored with a warning. The
   value is session-scoped and never persisted to the local config. The
   session-start reply reports the actual protocol next to the ATR — the
   server seeds its per-tracker value from it, and the field's presence tells
   the server this TBA version understands `protocol` requests.
2. **Local config.** `t_protocol: "T0"` / `"T1"` per card: written from the
   ATR on the first connection of a configured card, used for every connect
   and reset/reconnect when the server does not request a protocol (old
   servers), can be overridden manually in the config file.

An unknown `protocol` value is ignored with a warning. Old TBA versions
ignore the request field entirely and reply without the `protocol` field.

## 6. Rack traffic on the app connection

A serial card rack is served over the **app connection**: every rack message
travels under the topic prefix `rack/<serial>/`, where `<serial>` is the rack
device serial reduced to `[0-9A-Z]` (e.g. `SC1234`). Several racks ride the
same connection, each under its own prefix. The tails have no `/<sender>`
segment: the server is the only sender on this path.

| Direction    | Topic                            | Payload                                | When                                                                              |
| ------------ | -------------------------------- | -------------------------------------- | --------------------------------------------------------------------------------- |
| TBA → server | `rack/<serial>/link`             | link snapshot (below)                 | the rack's port is open; repeated for every open rack after each CONNACK of the app connection |
| TBA → server | `rack/<serial>/link`             | `{"state":"down"}`                     | the rack's port was closed or lost                                                |
| TBA → server | `rack/<serial>/response/<id>`    | response envelope (below)              | reply to `request/<id>` of this rack                                              |
| TBA → server | `rack/<serial>/watch`            | response envelope (below)              | the presence watch saw a change (below)                                           |
| server → TBA | `rack/<serial>/request/<id>`     | command envelope (below)               | one serial exchange with the rack                                                 |
| server → TBA | `rack/<serial>/watch`            | watch envelope (below)                 | arm or re-arm the presence watch                                                  |
| server → TBA | `rack/<serial>/release`          | `{"slot":N,"request_id":N}`             | release the matching discovery reservation                                      |
| server → TBA | `rack/<serial>/cards`            | `[{"slot":N,"iccid":"<16 hex>"},...]` | the complete set of cards of this rack to serve                                   |

`<id>` is counted by the server per rack. The request idempotency of section
4 applies per rack too, and its cache is reset on every CONNACK. A reply or a
watch report is delivered only to the connection its request or watch came
in on: once the app connection dropped or reconnected, the pending result is
discarded, never handed to the next connection.

TBA does not build, parse, or interpret the bytes it exchanges with the rack:
the wire protocol of the device is owned entirely by the server. TBA is a
transparent byte pipe between the server and the serial port.

### Link

`link up` announces an open rack and includes the identities of its live card
sessions, including sessions that survived the app connection:

```json
{"state":"up","guarded_discovery":true,"cards":[{"slot":1,"iccid":"<16 hex>"}]}
```

The server restores this inventory before checking the rack. A global status
omission must be confirmed by a per-slot status request before removing a
known card. Failed identity reads remain pending and retry on watch refresh.
The physical port disappearing closes its card sessions on TBA; `link down`
parks server state and preserves request numbering. A new app connection
starts new server state and therefore needs the inventory again.

`guarded_discovery` enables slot ownership across the reset/select/read
sequence. The server adds `discovery_slot` to its discovery envelopes. TBA
returns `serial_err:"card_busy"` without writing to a slot in authentication;
the server defers it without removing the card or repainting its LED. Otherwise
TBA reserves the slot until `rack/<serial>/release` with the slot and the last
discovery request id. A stale release cannot free a newer reservation. A new
authentication waits for this reservation without holding the serial port.
Authentication ownership follows the card envelope's `finish` flag, survives
app reconnect, and ends on finish, card disconnect, or a 65 second inactivity
limit (the server's session gap is 60 seconds). Discovery reservations also
expire after 65 seconds and are cancelled when the app connection changes.

These fields are optional for older clients. Deploy the updated server before
TBA; preservation across app reconnect and ownership protection require both
sides of this extension.

### Serial exchange (`request` / `response`)

Command envelope (only `serial_cmd` is required; every other field is
optional):

```json
{
  "serial_cmd": "<hex>",
  "expect": "<hex>",
  "idle_ms": 50,
  "deadline_ms": 2000,
  "poll": { "cmd": "<hex>", "while": "<hex>", "interval_ms": 20, "deadline_ms": 5000 }
}
```

Semantics (all byte patterns are supplied by the server; TBA only compares
them blindly):

1. Write `serial_cmd` bytes, read one reply (`idle_ms` of line silence ends a
   reply; `deadline_ms` bounds the whole read; defaults 800 ms / 5 s).
2. Without `poll`, that reply is the result. With `poll`: if the reply
   differs from `expect`, it is returned immediately; otherwise TBA re-sends
   `poll.cmd` every `poll.interval_ms` while the device answers exactly
   `poll.while`, and returns the first differing reply. Still-matching at
   `poll.deadline_ms` returns the last reply — the server decodes the device
   state from it.

The whole envelope is executed atomically on the port: exchanges of the rack
and of the card sessions of that rack queue up FIFO and interleave at envelope
granularity.

If stale bytes are buffered before a new envelope, TBA drains them and waits
for 300 ms of silence (up to 1500 ms) before writing. If the line does not
settle, it returns `no_reply` without sending the command. Pending bytes
within the same envelope's poll loop are preserved as possible results.

The response is published for **every** request, always in one shape:

```json
{ "serial_resp": "<hex>", "serial_err": "" }
```

`serial_err` is `""` on success or one of: `no_reply` (no reply obtained, including
a command skipped because stale traffic did not settle),
`write_failed`, `bad_hex` (malformed envelope), `truncated` (reply hit the
64 KB cap, or a nonempty read ended by deadline, EOF, shutdown or IO error;
the partial hex is still supplied), and `card_busy` (slot ownership conflict). Only successful exchanges are
cached for `request_id` idempotency — a repeat after an error retries the
device.

After a failed exchange TBA resyncs the line before releasing the port: it
drains whatever arrives until the line has been silent for 300 ms (capped at
1.5 s in total). A device that missed its deadline does answer, just late, and
those bytes could otherwise contaminate the next exchange with an incomplete
frame tail. The drain is bounded, so it cannot guarantee recovery from a device
that resumes sending after the drain budget. The server still validates framing
and checksum for every received buffer.

### Card set (`cards`) and the rack link report

At the end of every discovery pass over a rack the server publishes the
complete set of cards it wants served — topic `rack/<serial>/cards`:

```json
[{ "slot": 1, "iccid": "<16 hex>" }, { "slot": 3, "iccid": "<16 hex>" }]
```

The set is the desired state, not a notice. TBA reconciles the rack's card
sessions with it, keyed by the `(slot, iccid)` pair: a listed card without a
session gets one, a session of that rack whose pair is not listed is closed.
For a new session TBA resolves the ICCID to the company card number through
its local config (unknown ICCID — shown in the UI, not served) and opens a
regular card connection for it (same contract as section 5), backed by the
rack serial link instead of PC/SC. If a PC/SC connection for that card number
is already active, the spawn is skipped — one `client_id` never gets two
connections. A card listed in a different slot than its live session was
opened for gets a new session: the server binds a session to its slot. An
empty set closes every session of the rack.

Right after CONNACK such a rack-backed card connection publishes a one-shot
**rack link report** — topic `rack`. It is repeated over a live session only
for the card set that follows a `link up` of its rack, i.e. after a full
re-discovery, because the server may then have rebuilt its card map from
scratch and needs to hear that the card is still served. Repeating it for
every card set would put one LED frame per card of the rack on the wire ahead
of the tracker exchanges — on a full rack that is minutes of the serial link
per set, and a set closes every discovery pass:

```json
{ "iccid": "<16 hex>", "slot": 3, "rack": "<serial>" }
```

It binds the card session to its slot on the server; without it the server
treats the card as reader-backed. `rack` is diagnostics only. On this
connection the server then sends serial envelopes (this section) instead of
the section 5 card payloads; their topics keep the section 4 shape
`request/<id>/<sender>` — the `rack/` prefix exists on the app connection
only. All rack-backed card connections are closed when the rack disconnects.

### Card presence watch (`watch`)

After discovery the server arms a client-side presence watch — topic
`rack/<serial>/watch`:

```json
{ "cmd": "<hex>", "interval_ms": 2000, "idle_ms": 300, "deadline_ms": 3000, "refresh_ms": 30000 }
```

TBA re-executes the opaque `cmd` every `interval_ms` through the same FIFO
port queue and publishes the reply back (topic `rack/<serial>/watch`, the
standard `serial_resp`/`serial_err` envelope) **when its bytes change or the refresh interval has elapsed** (`refresh_ms`,
default 30 seconds). The periodic snapshot retries failed discovery and
unconfirmed removals without a baseline re-arm loop.
Arming again replaces the loop and resets the change baseline, so the first
reply after (re)arming is always published — this is how the server catches
states that changed while it was busy. TBA compares bytes blindly; what the
command means and what changed is decided entirely by the server. Inserted
and removed cards need no special message: the server learns of them from a
watch report and publishes the updated card set.

The timings are the server's to choose and are not the same for every command.
A rack built of several blocks answers a whole-rack query (the watch command is
one) with pauses inside the frame, while it polls its blocks: the reply is one
frame but not one burst, so the server sends a wider `idle_ms` for it than for
a single-reader exchange. TBA logs why each read ended (`end=silence` — the
device finished, `end=deadline` — a bound cut the reply), which is what makes a
too-narrow bound visible on the client side.

## 7. App connection

The application-level connection identifies a running TBA instance (presence,
diagnostics). It follows the same topic scheme. The only command it carries
is the log fetch (below); everything under the `rack/` prefix belongs to the
card racks (section 6) and does not collide with it.

The username/password fields of the MQTT CONNECT packet are reserved for
future authorization and must not be used to carry anything else.

### Settings report

Since 0.8.0, right after the app connection is established, TBA publishes a
one-shot **settings report** — the only message the client sends on its own
initiative. Topic: `settings`. The payload is a JSON object keyed by setting
name, so more settings can be reported later without changing the topic or
the format:

```json
{
  "app_info": {
    "version": "0.8.0",
    "os": "Linux",
    "os_release": "6.1.0",
    "arch": "x86_64"
  }
}
```

- `version` — application version (Cargo package version);
- `os` / `os_release` — operating system type and release;
- `arch` — CPU architecture the binary runs on.

The report is re-sent on every reconnect (the server tracks it per
connection). On the server the values are exposed as a read-only "Application
Information" device setting; unknown top-level keys are ignored. Servers
without settings-report support reject the unknown topic, so the server side
must be deployed before a TBA release that sends it.

### Log fetch (`fetch_logs`)

The server requests the application log with a JSON publish to
`request/<request_id>/0` on the app connection:

```json
{ "name": "fetch_logs", "period": "1d" }
```

- `period` — the time span of the log to return, counted back from now:
  `"1d"` (one day), `"7d"` (one week) or `"30d"` (one month).

TBA slices the requested period out of its log files (the current `log.txt`
first; when it does not reach back far enough, the archived `log.1.txt`
generation is prepended), packs the slice into a single-entry **zip** archive
(the server media pipeline accepts zip only) and publishes it back in binary
chunks of up to 1 MiB:

```
logs/<request_id>/<seq>      # zip content, seq starts from 0
logs/<request_id>/done       # JSON finalizer
```

The finalizer is either a success summary or an error report; the error text
becomes the command failure reason on the server:

```json
{ "name": "tba_logs_20260730_0930_1d.zip", "size": 123456, "chunks": 1 }
{ "error": "no log entries for the last 1 day(s)" }
```

Idempotency: while an upload is running, a re-sent `fetch_logs` with any
`request_id` is dropped — the upload in flight produces the reply. Chunks of
an abandoned exchange are discarded by the server via the `request_id` check.

## 8. Configuration inputs

What TBA needs to know locally to serve the protocol:

- `server.host` — where to connect;
- per-card entries in the config: company card number (used as the card
  connection `client_id`), ICCID (to match an inserted card to its number),
  `t_protocol` (see §5);
- everything else is server-driven.
