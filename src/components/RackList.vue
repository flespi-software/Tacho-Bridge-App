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
           the card list (see after the v-for), because the rack reports its
           slots one `connect` at a time and the scan keeps running long after
           the first card lands. -->
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
import { computed } from 'vue'
import type { RackCard, RackState } from './models'
import { cardStatusIcon } from './cardFormatters'

const props = defineProps<{
  rack: RackState | null
}>()

const emit = defineEmits<{
  (e: 'link', iccid: string): void
}>()

/**
 * The rack is still being enumerated: connected, and the backend has not yet
 * reported the scan as finished.
 *
 * Derived rather than tracked. An earlier version inferred the end of the scan
 * from a silence timeout, which could only ever be wrong in one direction or
 * the other — too short cut the indicator off mid-scan, too long left it
 * spinning after the last card. `scan_complete` is the real signal (the backend
 * raises it when the server arms the rack's presence watch, which happens once
 * discovery has walked the rack), so there is nothing left to time out.
 */
const scanning = computed(() => {
  const rack = props.rack
  return rack !== null && rack.connected && !rack.scan_complete
})

/**
 * Status icon for a rack card, from the shared vocabulary the readers list uses.
 * A card reported by the rack is always physically present, so `present` is
 * fixed — the rack has no "empty slot" row.
 */
function rackCardStatus(card: RackCard) {
  return cardStatusIcon(
    {
      present: true,
      linked: !!card.card_number,
      online: card.online,
      authentication: card.authentication,
    },
    '22px',
  )
}
</script>

<!-- Styles live in src/css/app.scss alongside the readers block so the rack
     matches it and follows the light/dark theme. -->
