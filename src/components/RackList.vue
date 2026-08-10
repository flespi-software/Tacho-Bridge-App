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
      <div v-if="rack.cards.length === 0" class="rack-cards-empty text-grey-6">
        <template v-if="!rack.connected">
          <q-icon name="mdi-card-search-outline" size="xs" class="q-mr-xs" />
          Rack disconnected
        </template>
        <!-- Scan in flight: no card reported yet, and the window has not
             elapsed. Indeterminate on purpose — a percentage here would be
             invented, since nothing tells us how many slots are being read. -->
        <template v-else-if="scanning">
          <div class="row items-center no-wrap q-mb-xs">
            <q-spinner size="xs" class="q-mr-xs" />
            <span>Scanning rack slots…</span>
          </div>
          <q-linear-progress
            indeterminate
            rounded
            size="4px"
            color="primary"
            class="rack-scan-bar"
          />
        </template>
        <!-- Window elapsed with nothing reported: the rack really is empty.
             Keeping the bar running here would imply work that has finished. -->
        <template v-else>
          <q-icon name="mdi-card-search-outline" size="xs" class="q-mr-xs" />
          No cards in the rack
        </template>
      </div>

      <div
        v-for="card in rack.cards"
        :key="card.slot"
        class="rack-card-row row items-center no-wrap"
      >
        <q-chip dense size="sm" color="blue-grey-2" text-color="blue-grey-9" class="text-bold">
          slot {{ card.slot }}
        </q-chip>
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
    </div>
  </div>
</template>

<script setup lang="ts">
import { ref, watch, onUnmounted } from 'vue'
import type { RackState } from './models'

const props = defineProps<{
  rack: RackState | null
}>()

const emit = defineEmits<{
  (e: 'link', iccid: string): void
}>()

/** How long a connected rack may stay silent before we call it empty. */
const SCAN_WINDOW_MS = 12000

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
// a fresh connection, and each newly reported card. Re-arming on every card
// keeps a slow trickle of `connect` messages from being cut off mid-scan, which
// a single fixed timeout from connect time would do on a full rack.
watch(
  () => [props.rack?.connected ?? false, props.rack?.cards.length ?? 0] as const,
  ([connected, cardCount], previous) => {
    if (!connected) {
      // Disconnected racks show their own message; no scan is in flight.
      stopScanTimer()
      scanning.value = false
      return
    }
    const [wasConnected, previousCount] = previous ?? [false, 0]
    if (!wasConnected || cardCount !== previousCount) {
      armScanWindow()
    }
  },
  { immediate: true },
)

// The timer outlives the component otherwise, and would write to a ref that no
// longer renders anything.
onUnmounted(stopScanTimer)
</script>

<!-- Styles live in src/css/app.scss alongside the readers block so the rack
     matches it and follows the light/dark theme. -->
