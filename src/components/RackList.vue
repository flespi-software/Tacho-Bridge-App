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

    <!-- Cards held in the rack. Empty for now: server-side rack control that
         reports the cards is not implemented yet. -->
    <div class="rack-cards">
      <div v-if="rack.cards.length === 0" class="rack-cards-empty text-grey-6">
        <q-icon name="mdi-card-search-outline" size="xs" class="q-mr-xs" />
        <template v-if="rack.connected">Waiting for cards from server…</template>
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
        <div class="col q-ml-sm">
          <div v-if="card.name" class="text-weight-medium">{{ card.name }}</div>
          <div v-if="card.card_number" class="text-grey-8">{{ card.card_number }}</div>
          <div v-else class="text-grey-6">card present</div>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import type { RackState } from './models'

defineProps<{
  rack: RackState | null
}>()
</script>

<!-- Styles live in src/css/app.scss alongside the readers block so the rack
     matches it and follows the light/dark theme. -->

