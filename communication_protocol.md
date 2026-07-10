# TBA <-> Server Communication Protocol

This document describes how the Tacho Bridge App (TBA) communicates with the
server: what connections it opens, which messages travel over them, when and
why. It documents the **transport envelope** implemented by TBA. TBA is a
bridge: the server decides *what* to do; TBA executes and reports back.

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

| Connection | client_id | Purpose |
|------------|-----------|---------|
| App | `TBA` + 13 digits (e.g. `TBA1740000000000`) | Application presence: one per running TBA instance. |
| Card | 16-character company card number | One per inserted card; carries the card's authentication traffic. |
| Rack | 16 characters: brand prefix + zero padding + device serial (e.g. `LISLE00000SC1799`) | One per connected serial card-rack device; carries opaque serial exchanges. |

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
{ "finish": false, "payload": "<hex>" }
```

| `finish` | `payload` | Meaning | TBA reply |
|----------|-----------|---------|-----------|
| `false` | empty | Session start: the server asks for the card's ATR. Also aborts a previous unfinished session (the card is reset). | `{ "payload": "<ATR hex>" }` |
| `false` | APDU hex | Transmit the APDU to the card. | `{ "payload": "<card response hex>" }` |
| `true` | — | Authentication session finished. TBA records the result and resets the card. | `{ "payload": "" }` |

On an unrecoverable card error TBA replies with the standard status word
`6F00` so the server always gets an answer.

### Card communication protocol (T0/T1)

The T protocol used to talk to a card is a per-card persisted property:

- on the first connection of a configured card it is derived from the ATR and
  stored in the local config (`t_protocol: "T0"` / `"T1"`);
- every later connect and every reset/reconnect uses the stored value;
- the value can be overridden manually in the config file.

**TBD:** reporting the active protocol to the server alongside the ATR, and a
server-driven protocol change (reconnect the card with the requested T and
persist it).

## 6. Rack connection payloads

The rack connection is a transparent byte pipe between the server and the
serial device. TBA does not build, parse, or interpret these bytes — the wire
protocol of the device is owned entirely by the server.

Incoming command shape:

```json
{ "serial_cmd": "<hex>" }
```

TBA decodes the hex, writes the raw bytes to the serial port, reads the reply
and publishes it back:

```json
{ "serial_resp": "<hex>" }
```

Safety bounds on the read: reply size cap (64 KB) and an overall read
deadline, protecting against a misbehaving device.

**TBD (planned envelope extensions):**

- server-supplied timing parameters per exchange (reply idle timeout, overall
  deadline);
- a generic repeat/poll form of the exchange (all byte patterns supplied by
  the server, TBA only compares them blindly);
- server-driven lifecycle of per-card connections for cards sitting in rack
  slots (open/close a card connection on the server's instruction), so that a
  rack card is served by the same card-connection contract as a PC/SC card;
- rack card presence monitoring.

## 7. App connection

The application-level connection identifies a running TBA instance (presence,
diagnostics). It follows the same topic scheme; currently it carries no
command traffic.

## 8. Configuration inputs

What TBA needs to know locally to serve the protocol:

- `server.host` — where to connect;
- per-card entries in the config: company card number (used as the card
  connection `client_id`), ICCID (to match an inserted card to its number),
  `t_protocol` (see §5);
- everything else is server-driven.
