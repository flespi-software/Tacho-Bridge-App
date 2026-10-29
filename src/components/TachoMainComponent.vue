<template>
  <CardBoard
    :readers="state.readers"
    :racks="racks"
    :cards="state.cards"
    :save="saveCardNumber"
    :remove="removeCard"
  />
</template>
<script setup lang="ts">
import CardBoard from './CardBoard.vue'
import type { SmartCard, Reader, RackState } from './models'
import { shallowRef, reactive, onMounted, inject } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { emit } from '@tauri-apps/api/event'
import { useTauriListeners, LAYOUT_LISTENERS_READY } from 'src/composables/useTauriListeners'
import { notifyError, TOAST_LONG } from 'src/composables/notify'

// reactive state for the readers and cards
const state = reactive<{ readers: Reader[]; cards: Record<string, SmartCard> }>({
  readers: [],
  cards: {},
})

// Card rack states, pushed from the backend via `rack-state` as the full list
// keyed by rack id. Empty until the backend reports a rack at least once;
// racks that disconnect stay listed with connected=false. Always replaced
// wholesale, never mutated, so a shallow ref is enough and the payload is not
// deep-proxied on every push.
const racks = shallowRef<RackState[]>([])

// Registered Tauri listeners. Kept in an array so we can detach them all
const { on } = useTauriListeners()
const layoutListenersReady = inject(LAYOUT_LISTENERS_READY, Promise.resolve())

////////////////////////// Listening for the event from the backend //////////////////////////
// Narrow runtime guard for the global-cards-sync payload. We do not trust the
// shape blindly — if the backend ever changes the contract, this guard fails
// closed (the event is ignored) instead of letting the UI throw at runtime.
type CardsSyncPayload = {
  iccid: string
  reader_name: string
  card_state: string
  card_number: string
  online?: boolean
  authentication?: boolean
}
function isCardsSyncPayload(raw: unknown): raw is CardsSyncPayload {
  if (!raw || typeof raw !== 'object') return false
  const p = raw as Record<string, unknown>
  return (
    typeof p.iccid === 'string' &&
    typeof p.reader_name === 'string' &&
    typeof p.card_state === 'string' &&
    typeof p.card_number === 'string'
  )
}

function handleCardsSync(raw: unknown): void {
  if (!isCardsSyncPayload(raw)) {
    console.warn('global-cards-sync: ignoring malformed payload', raw)
    return
  }
  console.debug('event payload: ', raw)

  const name = raw.reader_name
  const card_number = raw.card_number

  // PC/SC reports UNKNOWN/IGNORE for a reader that is gone from the system —
  // unplugged, or renamed by the OS after sleep/wake (the same physical reader
  // often comes back under a new name). Remove the row instead of keeping a
  // ghost entry forever; the reader's new name arrives as a separate event.
  if (/\b(UNKNOWN|IGNORE)\b/.test(raw.card_state)) {
    const goneIndex = state.readers.findIndex((reader) => reader.name === name)
    if (goneIndex !== -1) {
      state.readers.splice(goneIndex, 1)
    }
    return
  }

  // The PCSC monitor sends the bitflags Debug form "State(CHANGED | PRESENT)";
  // the MQTT emitter sends a bare "PRESENT". Parse the parenthesized form and
  // fall back to the raw string, then pick the first meaningful flag — the
  // positional [1] this used to be broke on single-flag and no-CHANGED forms.
  const inner = raw.card_state.match(/\(([^)]*)\)/)?.[1] ?? raw.card_state
  const flags = inner
    .split('|')
    .map((s) => s.trim())
    .filter(Boolean)
  const status = flags.find((f) => f !== 'CHANGED') ?? flags[0] ?? ''

  const iccid = raw.iccid
  // Find the index of the reader with the same name
  const index = state.readers.findIndex((reader) => reader.name === name)
  // The PC/SC monitor reports a card without the session flags (null), the
  // card's MQTT session reports only them: an event that does not carry the
  // flags keeps those of the same card.
  const prev = index !== -1 ? state.readers[index] : undefined
  const sameCard = iccid !== '' && prev?.iccid === iccid
  const next: Reader = {
    name,
    status,
    iccid,
    card_number,
    online: raw.online ?? (sameCard ? prev?.online : undefined),
    authentication: raw.authentication ?? (sameCard ? prev?.authentication : undefined),
  }
  if (index !== -1) {
    // The card's MQTT session repeats its state on every keep-alive: an
    // unchanged reader is not replaced, or the whole board would rebuild.
    if (prev && sameReader(prev, next)) return
    state.readers[index] = next
  } else {
    state.readers.push(next)
  }
}

function sameReader(a: Reader, b: Reader): boolean {
  return (
    a.status === b.status &&
    a.iccid === b.iccid &&
    a.card_number === b.card_number &&
    a.online === b.online &&
    a.authentication === b.authentication
  )
}

// Runtime guard for one rack in the rack-state payload — fail closed on a
// malformed shape.
function isRackStatePayload(raw: unknown): raw is RackState {
  if (!raw || typeof raw !== 'object') return false
  const p = raw as Record<string, unknown>
  return (
    typeof p.id === 'string' &&
    typeof p.connected === 'boolean' &&
    typeof p.name === 'string' &&
    Array.isArray(p.cards)
  )
}

function handleRackState(raw: unknown): void {
  // The backend always sends the full rack list, so the local one is replaced
  // wholesale — no delta merging.
  if (!Array.isArray(raw) || !raw.every(isRackStatePayload)) {
    console.warn('rack-state: ignoring malformed payload', raw)
    return
  }
  racks.value = raw
}

///////////////////////////// Dialog window for entering the Card Number value /////////////////////////////

const saveCardNumber = async (cardNumber: string, content: SmartCard): Promise<boolean> => {
  console.log(`Card Number: ${cardNumber}, Card iccid: ${content.iccid}`)

  // The backend reconnects the affected card itself after a successful save
  // (PCSC rescan + pending rack cards), no explicit sync call is needed here.
  // The local card list is NOT updated here: on success the backend emits
  // `global-card-config-updated`, the single source of truth — an optimistic
  // local write would show a "saved" card that a failed write never persisted.
  try {
    // The command rejects with a human-readable reason (e.g. an ICCID already
    // linked to another card), so the message below can be shown verbatim.
    await invoke('update_card', {
      cardnumber: cardNumber,
      content: content,
    })
    console.log('Card number updated successfully')
    return true
  } catch (error) {
    console.error(`Failed to update card ${cardNumber}:`, error)
    notifyError(`Failed to save card ${cardNumber}`, error, TOAST_LONG)
    return false
  }
}

// Removes a card from the config; resolves to whether it was removed. As with
// a save, the local list is updated by the backend's config event only.
const removeCard = async (cardNumber: string): Promise<boolean> => {
  try {
    await invoke('remove_card', { cardnumber: cardNumber })
    console.log('Card removed:', cardNumber)
    return true
  } catch (error) {
    console.error('Failed to remove card:', error)
    notifyError(`Failed to remove card ${cardNumber}`, error, TOAST_LONG)
    return false
  }
}

function handleCardConfigUpdated(raw: unknown): void {
  if (!raw || typeof raw !== 'object') {
    console.warn('global-card-config-updated: ignoring malformed payload', raw)
    return
  }
  const payload = raw as { card_number?: unknown; content?: unknown }
  if (typeof payload.card_number !== 'string' || payload.card_number.length === 0) {
    console.warn('global-card-config-updated: missing card_number', raw)
    return
  }
  console.debug('event payload: ', raw)

  if (payload.content && typeof payload.content === 'object') {
    state.cards[payload.card_number] = { ...(payload.content as SmartCard) }
  } else {
    delete state.cards[payload.card_number]
  }
}

onMounted(async () => {
  // Register listeners BEFORE notifying the backend that we're loaded.
  // Otherwise the initial sync burst can race the channel registration and
  // arrive into the void, leaving the UI stuck on stale empty state.
  await on('global-cards-sync', handleCardsSync)
  await on('global-card-config-updated', handleCardConfigUpdated)
  await on('rack-state', handleRackState)

  // The layout's listeners too (notifications, server config).
  await layoutListenersReady

  // Now that the listeners are wired, tell the backend it can start
  // emitting initial state. The replay bursts one event per EXISTING card and
  // nothing for absent ones — clear the map first so the replay is
  // authoritative and cards deleted while the webview was away don't linger.
  state.cards = {}
  try {
    await emit('frontend-loaded', { message: 'Hello from frontend!' })
  } catch (error) {
    console.error('Error emitting frontend-loaded event:', error)
  }
})
</script>
