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
import { isCardConfigPayload, isRackStatePayload } from './models'
import { applyReaderEvent, isCardsSyncPayload, unlinkCard } from './readerEvents'
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

// Registered Tauri listeners, detached together when the component unmounts.
const { on } = useTauriListeners()
const layoutListenersReady = inject(LAYOUT_LISTENERS_READY, Promise.resolve())

////////////////////////// Listening for the event from the backend //////////////////////////

function handleCardsSync(raw: unknown): void {
  if (!isCardsSyncPayload(raw)) {
    console.warn('global-cards-sync: ignoring malformed payload', raw)
    return
  }
  console.debug('event payload: ', raw)
  applyReaderEvent(state.readers, raw)
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

///////////////////////////// Saving and removing cards /////////////////////////////

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
  if (!isCardConfigPayload(raw)) {
    console.warn('global-card-config-updated: ignoring malformed payload', raw)
    return
  }
  console.debug('event payload: ', raw)

  if (raw.content) {
    state.cards[raw.card_number] = { ...raw.content }
  } else {
    delete state.cards[raw.card_number]
    unlinkCard(state.readers, raw.card_number)
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
