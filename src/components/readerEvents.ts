/**
 * The reader list of the board, kept from the backend's `global-cards-sync`
 * events. Pure functions over a plain array (the component passes its
 * reactive one), so the merging rules are unit tested apart from the UI.
 */
import type { Reader } from './models'

/** Payload of one `global-cards-sync` event. */
export type CardsSyncPayload = {
  iccid: string
  reader_name: string
  card_state: string
  card_number: string
  online?: boolean | null
  authentication?: boolean | null
  problem?: string | null
}

/**
 * Narrow runtime guard: if the backend ever changes the contract, the event
 * is ignored instead of letting the UI throw at runtime.
 */
export function isCardsSyncPayload(raw: unknown): raw is CardsSyncPayload {
  if (!raw || typeof raw !== 'object') return false
  const p = raw as Record<string, unknown>
  return (
    typeof p.iccid === 'string' &&
    typeof p.reader_name === 'string' &&
    typeof p.card_state === 'string' &&
    typeof p.card_number === 'string'
  )
}

/**
 * The PCSC monitor sends the bitflags Debug form "State(CHANGED | PRESENT)";
 * the MQTT emitter sends a bare "PRESENT". Parse the parenthesized form and
 * fall back to the raw string, then pick the first meaningful flag — the
 * positional [1] this used to be broke on single-flag and no-CHANGED forms.
 */
export function readerStatus(cardState: string): string {
  const inner = cardState.match(/\(([^)]*)\)/)?.[1] ?? cardState
  const flags = inner
    .split('|')
    .map((s) => s.trim())
    .filter(Boolean)
  return flags.find((f) => f !== 'CHANGED') ?? flags[0] ?? ''
}

function sameReader(a: Reader, b: Reader): boolean {
  return (
    a.status === b.status &&
    a.iccid === b.iccid &&
    a.card_number === b.card_number &&
    a.online === b.online &&
    a.authentication === b.authentication &&
    a.problem === b.problem
  )
}

/**
 * Applies one event to the reader list, in place. Returns whether the list
 * changed: the card's MQTT session repeats its state on every keep-alive, and
 * an unchanged reader is not replaced, or the whole board would rebuild.
 */
export function applyReaderEvent(readers: Reader[], event: CardsSyncPayload): boolean {
  const name = event.reader_name
  const index = readers.findIndex((reader) => reader.name === name)

  // PC/SC reports UNKNOWN/IGNORE for a reader that is gone from the system —
  // unplugged, or renamed by the OS after sleep/wake (the same physical reader
  // often comes back under a new name). Remove the row instead of keeping a
  // ghost entry forever; the reader's new name arrives as a separate event.
  if (/\b(UNKNOWN|IGNORE)\b/.test(event.card_state)) {
    if (index === -1) return false
    readers.splice(index, 1)
    return true
  }

  const iccid = event.iccid
  // The PC/SC monitor reports a card without the session flags (null), the
  // card's MQTT session reports only them: an event that does not carry the
  // flags keeps those of the same card.
  const prev = index !== -1 ? readers[index] : undefined
  const sameCard = iccid !== '' && prev?.iccid === iccid
  const next: Reader = {
    name,
    status: readerStatus(event.card_state),
    iccid,
    card_number: event.card_number,
    online: event.online ?? (sameCard ? prev?.online : undefined),
    // An offline session authenticates nothing: its flag is not inherited.
    authentication:
      event.online === false
        ? false
        : (event.authentication ?? (sameCard ? prev?.authentication : undefined)),
    // every event carries it: the backend keeps it per card, not per emitter
    problem: event.problem ?? undefined,
  }
  if (prev) {
    if (sameReader(prev, next)) return false
    readers[index] = next
  } else {
    readers.push(next)
  }
  return true
}

/**
 * A card removed from the config: the reader keeps the inserted card, but no
 * longer its number and session.
 */
export function unlinkCard(readers: Reader[], cardNumber: string): void {
  readers.forEach((reader, i) => {
    if (reader.card_number === cardNumber) {
      readers[i] = {
        ...reader,
        card_number: '',
        online: undefined,
        authentication: undefined,
        problem: undefined,
      }
    }
  })
}
