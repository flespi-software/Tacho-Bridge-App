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

| Connection | client_id                                                                            | Purpose                                                                     |
| ---------- | ------------------------------------------------------------------------------------ | --------------------------------------------------------------------------- |
| App        | `TBA` + 13 digits (e.g. `TBA1740000000000`)                                          | Application presence: one per running TBA instance.                         |
| Card       | 16-character company card number                                                     | One per inserted card; carries the card's authentication traffic.           |
| Rack       | 16 characters: brand prefix + zero padding + device serial (e.g. `LISLE00000SC1234`) | One per connected serial card-rack device; carries opaque serial exchanges. |

Lifecycle: a card connection is opened when a configured card is inserted and
closed when it is removed. A rack connection is opened when a supported serial
device is detected on USB and closed when it disappears.

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

## 6. Rack connection payloads

The rack connection is a transparent byte pipe between the server and the
serial device. TBA does not build, parse, or interpret these bytes — the wire
protocol of the device is owned entirely by the server.

Incoming command shape (only `serial_cmd` is required; every other field is
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

The whole envelope is executed atomically on the port: exchanges of parallel
card sessions of one rack queue up FIFO and interleave at envelope
granularity.

The response is published for **every** request, always in one shape:

```json
{ "serial_resp": "<hex>", "serial_err": "" }
```

`serial_err` is `""` on success or one of: `no_reply` (device stayed silent),
`write_failed`, `bad_hex` (malformed envelope), `truncated` (reply hit the
64 KB cap; the partial hex is still supplied). Only successful exchanges are
cached for `request_id` idempotency — a repeat after an error retries the
device.

### Card spawn (`connect`) and the rack link report

When the server has identified a card in a rack slot, it publishes a spawn
instruction on the rack connection — topic `connect`:

```json
{ "iccid": "<16 hex>", "slot": 3 }
```

TBA resolves the ICCID to the company card number through its local config
(unknown ICCID — logged and skipped) and opens a regular card connection for
it (same contract as §5), backed by the rack serial link instead of PC/SC. If
a PC/SC connection for that card number is already active, the spawn is
skipped — one `client_id` never gets two connections.

Right after CONNACK such a rack-backed card connection publishes a one-shot
**rack link report** — topic `rack`:

```json
{ "iccid": "<16 hex>", "slot": 3 }
```

It binds the card session to its slot on the server; without it the server
treats the card as reader-backed. On this connection the server then sends
serial envelopes (this section) instead of the §5 card payloads. All
rack-backed card connections are closed when the rack disconnects.

### Card presence watch (`watch`) and card removal (`disconnect`)

After discovery the server arms a client-side presence watch — topic `watch`
on the rack connection:

```json
{ "cmd": "<hex>", "interval_ms": 1000, "idle_ms": 50, "deadline_ms": 2000 }
```

TBA re-executes the opaque `cmd` every `interval_ms` through the same FIFO
port queue and publishes the reply back (topic `watch`, the standard
`serial_resp`/`serial_err` envelope) **only when its bytes change**. Arming
again replaces the loop and resets the change baseline, so the first reply
after (re)arming is always published — this is how the server catches states
that changed while it was busy. TBA compares bytes blindly; what the command
means and what changed is decided entirely by the server.

When the server concludes a card left its slot, it publishes a removal notice
on the rack connection — topic `disconnect`:

```json
{ "iccid": "<16 hex>", "slot": 3 }
```

TBA closes the rack-backed card connection of that card (if one was spawned)
and removes the card from the rack UI. Newly inserted cards need no special
message: the server discovers them from a watch update and sends a regular
`connect` spawn.

## 7. App connection

The application-level connection identifies a running TBA instance (presence,
diagnostics). It follows the same topic scheme. The only command it carries
is the log fetch (below).

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
