<template>
  <div style="width: 600px; max-width: 100%">
    <div class="readers-container">
      <div v-if="state.readers.length === 0" class="empty-state">
        <q-icon name="mdi-card-search-outline" class="empty-state-icon" />
        <div class="empty-state-title">No connected smart card readers</div>
        <div class="empty-state-subtitle">Connect a smart card reader to get started</div>
      </div>
      <div v-for="reader in state.readers" :key="reader.name" class="row reader-row">
        <q-item class="col-6" style="min-height: 50px" dense>
          <q-item-section avatar>
            <q-icon name="mdi-usb-port" :color="reader.status !== 'UNKNOWN' ? 'green' : 'red'" />
          </q-item-section>
          <q-item-section>
            <q-item-label caption lines="3" class="text-grey text-bold">
              <small>{{ reader.name }}</small>
            </q-item-label>
            <!--
              Last authentication state. Three modes:
                - processing (yellow) — active APDU exchange with the VU
                - success (green)    — last completed auth ended with finish:true
                - fail (red)         — last completed auth was aborted
              Only the status word itself is coloured; the prefix stays neutral.
            -->
            <q-item-label
              v-if="
                reader.card_number &&
                (authInProgress[reader.card_number] || state.cards[reader.card_number]?.last_auth)
              "
              caption
            >
              <template v-if="authInProgress[reader.card_number]">
                Last auth:
                <span class="text-amber-8 text-weight-medium">processing...</span>
              </template>
              <template v-else-if="state.cards[reader.card_number]?.last_auth">
                Last auth:
                {{ formatAuthDate(state.cards[reader.card_number]?.last_auth?.[0]) }}
                (<span
                  :class="
                    state.cards[reader.card_number]?.last_auth?.[1]
                      ? 'text-green-8'
                      : 'text-red text-weight-medium'
                  "
                  >{{ state.cards[reader.card_number]?.last_auth?.[1] ? 'success' : 'fail' }}</span
                >)
              </template>
            </q-item-label>
          </q-item-section>
        </q-item>
        <q-item class="col-6" style="min-height: 50px" dense v-if="reader.status !== 'UNKNOWN'">
          <q-item-section avatar>
            <q-icon v-bind="cardConnectedStatus(reader)" />
          </q-item-section>

          <q-item-section>
            <template v-if="!reader.card_number && reader.iccid">
              <q-item-label lines="1">UNKNOWN CARD</q-item-label>
              <q-item-label lines="1" caption>
                <q-chip
                  dense
                  size="sm"
                  color="blue-grey-2"
                  text-color="blue-grey-9"
                  class="text-bold"
                >
                  ICCID: {{ reader.iccid }}
                </q-chip>
              </q-item-label>
            </template>
            <template v-if="reader.card_number">
              <q-item-label
                v-if="state.cards && reader.card_number && state.cards[reader.card_number]"
                style="word-break: break-word; white-space: normal"
              >
                {{ state.cards[reader.card_number]?.name }}
              </q-item-label>
              <q-item-label>
                <span class="text-weight-medium">{{ reader.card_number }}</span>
                <span
                  v-if="state.cards[reader.card_number]?.structure_version"
                  class="text-grey-7 q-ml-xs"
                >
                  ({{ formatStructureVersion(state.cards[reader.card_number]?.structure_version) }})
                </span>
              </q-item-label>
              <q-item-label
                v-if="state.cards[reader.card_number]?.company_name"
                caption
                class="overflow-hidden ellipsis"
              >
                <q-icon name="mdi-domain" size="xs" class="q-mr-xs" />
                {{ state.cards[reader.card_number]?.company_name }}
              </q-item-label>
              <q-item-label v-if="state.cards[reader.card_number]?.expire" caption>
                <q-icon name="mdi-calendar" size="xs" class="q-mr-xs" />
                <span
                  :class="
                    isExpired(state.cards[reader.card_number]?.expire)
                      ? 'text-red text-weight-medium'
                      : ''
                  "
                >
                  {{ formatExpire(state.cards[reader.card_number]?.expire) }}
                </span>
              </q-item-label>
            </template>

            <q-item-label lines="1" v-if="!reader.iccid && !reader.card_number">
              <span class="text-weight-medium text-grey-6">EMPTY SLOT</span>
            </q-item-label>
          </q-item-section>
          <q-item-section side v-if="reader.iccid && !reader.card_number">
            <div class="text-grey-8 q-gutter-xs">
              <q-btn size="12px" flat dense round icon="mdi-link" @click="linkMode(reader.iccid)" />
            </div>
          </q-item-section>
        </q-item>
      </div>
    </div>
    <!-- Card rack blocks, below the plain readers. One block per connected
         rack (several racks on separate USB ports are all served), each rack
         holding many cards. -->
    <RackList v-for="r in racks" :key="r.client_id" :rack="r" @link="linkMode" />
    <SmartCardList
      ref="cardlist"
      :cards="state.cards"
      @add-card="addCard"
      @update-card="updateCard"
      @delete-card="removeCard"
    />
  </div>
</template>

<style scoped>
/* `.blinking-icon` and its @keyframes live in src/css/app.scss: the rack list
   needs the same animation, and a scoped copy cannot be shared (Vue rewrites
   keyframe names per component). */
.toolbar-block {
  margin-bottom: 8px;
}
.custom-font-size-reader {
  font-size: 10px;
}
.header-flex-container {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding-right: 16px;
}
.card-number-dialog .q-card {
  width: 300px; /* Window width */
  max-width: 90vw; /* Maximum window width */
  height: 160px; /* Window height */
  max-height: 90vh; /* Maximum window height */
}
</style>

<script setup lang="ts">
import SmartCardList from './SmartCardList.vue'
import RackList from './RackList.vue'
import type { SmartCard, Reader, RackState } from './models'
import {
  formatStructureVersion,
  formatExpire,
  isExpired,
  formatAuthDate,
  cardStatusIcon,
} from './cardFormatters'
import { ref, reactive, computed, onMounted, onUnmounted } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { listen, emit, type UnlistenFn } from '@tauri-apps/api/event'
import { Notify } from 'quasar'

const cardlist = ref<null | {
  linkMode: (iccid: string) => void
  openAddDialog: () => void
}>(null)

// reactive state for the readers and cards
const state = reactive<{ readers: Reader[]; cards: Record<string, SmartCard> }>({
  readers: [],
  cards: {},
})

// Card rack states, pushed from the backend via `rack-state` as the full list
// keyed by client_id. Empty until the backend reports a rack at least once;
// racks that disconnect stay listed with connected=false.
const racks = ref<RackState[]>([])

// Registered Tauri listeners. Kept in an array so we can detach them all
// in onUnmounted — leaking listeners across HMR/navigation would let stale
// handlers keep mutating dead state and double-fire on remount.
const unlistenFns: UnlistenFn[] = []

// Transient "authentication in progress" flag per card_number, derived from
// the Reader.authentication field emitted by the backend via global-cards-sync.
// Persists only for the duration of an active APDU exchange; SmartCardList
// uses it to render the yellow "Last auth: processing..." line.
const authInProgress = computed<Record<string, boolean>>(() => {
  const map: Record<string, boolean> = {}
  for (const r of state.readers) {
    if (r.card_number && r.authentication) {
      map[r.card_number] = true
    }
  }
  return map
})

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
  console.log('event payload: ', raw)

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
  const next: Reader = {
    name,
    status,
    iccid,
    card_number,
    online: raw.online,
    authentication: raw.authentication,
  }
  if (index !== -1) {
    state.readers[index] = next
  } else {
    state.readers.push(next)
  }
}

// Runtime guard for one rack in the rack-state payload — fail closed on a
// malformed shape.
function isRackStatePayload(raw: unknown): raw is RackState {
  if (!raw || typeof raw !== 'object') return false
  const p = raw as Record<string, unknown>
  return (
    typeof p.client_id === 'string' &&
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

const saveCardNumber = async (cardNumber: string, content: SmartCard) => {
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
  } catch (error) {
    console.error(`Failed to update card ${cardNumber}:`, error)
    Notify.create({
      message: `Failed to save card ${cardNumber}: ${String(error)}`,
      color: 'red',
      position: 'bottom',
      timeout: 8000,
    })
  }
}

// Status icon for a card in a reader, from the shared vocabulary the rack list
// uses too. `iccid` present means a card is physically in the reader.
const cardConnectedStatus = (reader: Reader) =>
  cardStatusIcon({
    present: !!reader.iccid,
    linked: !!reader.card_number,
    online: reader.online,
    authentication: reader.authentication,
  })

// SmartCardList handlers
function linkMode(iccid: string) {
  cardlist.value?.linkMode(iccid)
  if (Object.values(state.cards)?.filter((card) => !card.iccid).length === 0) {
    cardlist.value?.openAddDialog()
  }
}
async function addCard(number: string, data: SmartCard) {
  await saveCardNumber(number, data)
}

async function updateCard(number: string, data: SmartCard) {
  await saveCardNumber(number, data)
}

// remove card func from the config
const removeCard = async (cardNumber: string) => {
  try {
    await invoke('remove_card', { cardnumber: cardNumber })
    console.log('Card removed:', cardNumber)
  } catch (error) {
    console.error('Failed to remove card:', error)
    Notify.create({
      message: `Failed to remove card ${cardNumber}: ${String(error)}`,
      color: 'red',
      position: 'bottom',
      timeout: 8000,
    })
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
  console.log('event payload: ', raw)

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
  try {
    const unlisten = await listen('global-cards-sync', (event) => handleCardsSync(event.payload))
    unlistenFns.push(unlisten)
  } catch (error) {
    console.error('Error listening to global-cards-sync:', error)
  }

  try {
    const unlisten = await listen('global-card-config-updated', (event) =>
      handleCardConfigUpdated(event.payload),
    )
    unlistenFns.push(unlisten)
  } catch (error) {
    console.error('Error listening to global-card-config-updated:', error)
  }

  try {
    const unlisten = await listen('rack-state', (event) => handleRackState(event.payload))
    unlistenFns.push(unlisten)
  } catch (error) {
    console.error('Error listening to rack-state:', error)
  }

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

onUnmounted(() => {
  while (unlistenFns.length > 0) {
    const fn = unlistenFns.pop()
    try {
      fn?.()
    } catch (e) {
      console.error('Error detaching Tauri listener:', e)
    }
  }
})
</script>
