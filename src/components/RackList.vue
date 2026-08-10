<template>
  <!-- Card rack block. Shown only when a rack has ever been reported by the
       backend. Unlike a single reader, a rack is a horizontal block: a header
       with the rack name on top, and the list of its cards below. -->
  <div v-if="rack" class="rack-block">
    <!-- Header: rack identity + connection state -->
    <div class="rack-header row items-center no-wrap">
      <q-icon
        name="mdi-server-network"
        size="sm"
        :color="rack.connected ? 'green' : 'red'"
        class="q-mr-sm"
      />
      <div class="col">
        <div class="text-weight-bold">{{ rack.name }}</div>
        <div class="rack-subtitle text-grey-7">
          <span v-if="rack.serial">SN: {{ rack.serial }}</span>
          <span v-if="rack.manufacturer" class="q-ml-sm">{{ rack.manufacturer }}</span>
        </div>
      </div>
      <q-chip
        dense
        size="sm"
        :color="rack.connected ? 'green-2' : 'red-2'"
        :text-color="rack.connected ? 'green-9' : 'red-9'"
        class="text-bold"
      >
        {{ rack.connected ? 'connected' : 'disconnected' }}
      </q-chip>
    </div>

    <!-- Cards held in the rack, as reported by the server one `connect` at a
         time. While that series is still arriving we show an indeterminate
         progress bar: the server sends no total, so there is no honest
         percentage to display (see `scanning` below). -->
    <div class="rack-cards">
      <!-- Only the two terminal states live here; the scan indicator sits BELOW
           the card list (see after the v-for), because on a large rack the
           cards arrive one `connect` at a time and the scan keeps running long
           after the first one lands. -->
      <div v-if="rack.cards.length === 0 && !scanning" class="rack-cards-empty text-grey-6">
        <q-icon name="mdi-card-search-outline" size="xs" class="q-mr-xs" />
        <template v-if="rack.connected">No cards in the rack</template>
        <template v-else>Rack disconnected</template>
      </div>

      <div
        v-for="card in rack.cards"
        :key="card.slot"
        class="rack-card-row row items-center no-wrap"
      >
        <q-chip dense size="sm" color="blue-grey-2" text-color="blue-grey-9" class="text-bold">
          slot {{ card.slot }}
        </q-chip>
        <!-- Card state, mirroring the icon language of a plain reader row:
             green while the session is online, blinking during an active APDU
             exchange, outline when the card is present but not served. -->
        <q-icon v-bind="rackCardStatus(card)" class="q-ml-xs" />
        <div class="col q-ml-sm">
          <!-- configured card: name + number, like a card in a plain reader -->
          <template v-if="card.card_number">
            <div v-if="card.name" class="text-weight-medium">{{ card.name }}</div>
            <div class="text-grey-8">{{ card.card_number }}</div>
          </template>
          <!-- unconfigured card: same presentation as an unknown card in a reader -->
          <template v-else>
            <div class="text-weight-medium">UNKNOWN CARD</div>
            <q-chip
              v-if="card.iccid"
              dense
              size="sm"
              color="blue-grey-2"
              text-color="blue-grey-9"
              class="text-bold"
            >
              ICCID: {{ card.iccid }}
            </q-chip>
          </template>
        </div>
        <!-- link the unknown card to a configured number, same flow as for readers -->
        <div v-if="!card.card_number && card.iccid" class="text-grey-8 q-gutter-xs">
          <q-btn size="12px" flat dense round icon="mdi-link" @click="emit('link', card.iccid)" />
        </div>
      </div>

      <!-- Scan still in flight. Deliberately OUTSIDE the "list is empty" branch:
           a large rack reports its slots one `connect` at a time, so the bar has
           to survive the arrival of the first card and keep running underneath
           the rows that are already listed. Indeterminate because the server
           sends no total — there is no honest percentage to show. -->
      <div v-if="scanning" class="rack-scan text-grey-6">
        <div class="row items-center no-wrap q-mb-xs">
          <q-spinner size="xs" class="q-mr-xs" />
          <span>Scanning rack slots…</span>
        </div>
        <q-linear-progress indeterminate rounded size="4px" color="primary" class="rack-scan-bar" />
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { ref, watch, onUnmounted } from 'vue'
import type { RackCard, RackState } from './models'

const props = defineProps<{
  rack: RackState | null
}>()

const emit = defineEmits<{
  (e: 'link', iccid: string): void
}>()

/**
 * Fallback only: how long the rack may stay silent before we give up waiting.
 *
 * The scan normally ends on the backend's `scan_complete` flag, which the
 * server raises when it finishes enumerating the rack. This timeout exists for
 * the case where that signal never arrives (older server that does not arm the
 * presence watch), so the indicator cannot spin forever. It is re-armed on
 * every change to the card list, so it only ever bounds the gap between two
 * reports, never the whole scan.
 */
const SCAN_WINDOW_MS = 30000

// True while we still expect `connect` messages for this rack. The server
// reports discovered cards one at a time and never says "scan finished", so
// this is a time window rather than a real completion signal: it opens when the
// rack connects and closes once the rack has been quiet for SCAN_WINDOW_MS.
const scanning = ref(false)
let scanTimer: ReturnType<typeof setTimeout> | undefined

function stopScanTimer(): void {
  if (scanTimer !== undefined) {
    clearTimeout(scanTimer)
    scanTimer = undefined
  }
}

/** (Re)opens the scan window — the rack is connected and something may still arrive. */
function armScanWindow(): void {
  stopScanTimer()
  scanning.value = true
  scanTimer = setTimeout(() => {
    scanning.value = false
    scanTimer = undefined
  }, SCAN_WINDOW_MS)
}

// Drive the window off the two things that mean "the rack is still working":
// a fresh connection, and any change to the reported card list. Re-arming on
// every change keeps a slow trickle of `connect` messages from being cut off
// mid-scan, which a single fixed timeout from connect time would do on a full
// rack. The list is fingerprinted by slot+iccid rather than just counted: a
// slot being reassigned (card swapped) leaves the count identical but still
// means the rack is actively reporting.
watch(
  () =>
    [
      props.rack?.connected ?? false,
      props.rack?.scan_complete ?? false,
      (props.rack?.cards ?? []).map((c) => `${c.slot}:${c.iccid ?? ''}`).join(','),
    ] as const,
  ([connected, scanComplete, fingerprint], previous) => {
    if (!connected) {
      // Disconnected racks show their own message; no scan is in flight.
      stopScanTimer()
      scanning.value = false
      return
    }
    // The backend says enumeration is over — end the indicator at once instead
    // of waiting out the fallback window. This is what stops the bar from
    // lingering for half a minute after every card is already on screen.
    if (scanComplete) {
      stopScanTimer()
      scanning.value = false
      return
    }
    const [wasConnected, , previousFingerprint] = previous ?? [false, false, '']
    if (!wasConnected || fingerprint !== previousFingerprint) {
      armScanWindow()
    }
  },
  { immediate: true },
)

/**
 * Icon spec for one rack card, mirroring `cardConnectedStatus` in the readers
 * block so both lists speak the same visual language:
 *   blinking green — an APDU exchange is in progress on this card
 *   solid green    — session is up and idle
 *   grey outline   — card present and configured, but no session yet
 *   orange plus    — card present but not linked to a card number
 */
function rackCardStatus(card: RackCard) {
  if (!card.card_number) {
    return { name: 'mdi-card-plus-outline', color: 'orange', size: '22px' }
  }
  if (card.online && card.authentication) {
    return { name: 'mdi-smart-card', color: 'green', size: '22px', class: 'blinking-icon' }
  }
  if (card.online) {
    return { name: 'mdi-smart-card', color: 'green', size: '22px' }
  }
  return { name: 'mdi-smart-card-outline', color: 'grey', size: '22px' }
}

// The timer outlives the component otherwise, and would write to a ref that no
// longer renders anything.
onUnmounted(stopScanTimer)
</script>

<!-- Styles live in src/css/app.scss alongside the readers block so the rack
     matches it and follows the light/dark theme. -->
