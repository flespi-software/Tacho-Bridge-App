<template>
  <!-- One location (USB readers, a rack, or not-inserted cards): a sticky
       header with the group's health at a glance, and its rows below. -->
  <section class="cb-group">
    <div class="cb-group__head" :class="{ 'cb-group__head--open': open }">
      <button type="button" class="cb-group__toggle" :aria-expanded="open" @click="emit('toggle')">
        <q-icon
          name="mdi-chevron-right"
          size="20px"
          class="cb-group__chevron"
          :class="{ 'cb-group__chevron--open': open }"
        />
        <q-icon :name="icon" size="22px" :class="iconClass" />
        <span class="cb-group__titles">
          <span class="cb-group__titleline">
            <span class="cb-group__title">{{ group.title }}</span>
            <span class="cb-group__subtitle" :class="{ 'cb-tone--danger': group.down }">
              {{ scanning ? 'scanning slots…' : group.subtitle }}
            </span>
          </span>
          <span class="cb-segs">
            <span
              v-for="s in segments"
              :key="s.status"
              :class="`cb-seg--${s.status}`"
              :style="{ flexGrow: s.n }"
            />
          </span>
        </span>
      </button>
      <div class="cb-group__stats">
        <span class="cb-group__count">{{ countLine }}</span>
        <!-- The slot table of a rack: counters here, the slot grid on click.
             Nothing to show until the server has sent a table. -->
        <button
          v-if="slotsLineText"
          type="button"
          class="cb-slots-toggle"
          :class="{ 'cb-slots-toggle--open': slotsOpen }"
          :aria-expanded="slotsOpen"
          @click="slotsOpen = !slotsOpen"
        >
          {{ slotsLineText }}
        </button>
        <span class="cb-group__issues">
          <button
            v-for="i in issues"
            :key="i.status"
            type="button"
            class="cb-issue"
            :class="`cb-issue--${i.status}`"
            @click="emit('pick-issue', i.status)"
          >
            {{ i.label }}
          </button>
          <span v-if="allGood" class="cb-tone--ok">all good</span>
        </span>
      </div>
    </div>
    <!-- Indeterminate: the server sends no total, so there is no honest
         percentage to show while the rack is still being enumerated. -->
    <q-linear-progress
      v-if="scanning"
      indeterminate
      size="2px"
      color="primary"
      class="rack-scan-bar"
    />

    <!-- Slot grid: one framed block per contiguous run of readers, its slot
         range cut into the top line of the frame; rows of ten in address
         order, every cell numbered. The gaps between DIP-set blocks are
         captions between the frames, not cells, so the rows never shift. -->
    <div v-if="slotsOpen && blocks.length" class="cb-slots">
      <template v-for="b in blocks" :key="b.from">
        <div v-if="b.gapBefore" class="cb-slots__gap">
          no readers at {{ b.gapBefore[0] }}–{{ b.gapBefore[1] }}
        </div>
        <fieldset class="cb-slots__block">
          <legend class="cb-slots__caption">{{ b.from }} – {{ b.to }}</legend>
          <div v-for="row in b.rows" :key="row[0]!.slot" class="cb-slots__row">
            <span
              v-for="c in row"
              :key="c.slot"
              class="cb-slot"
              :class="[`cb-slot--${c.cls}`, c.status ? `cb-slot--${c.status}` : '']"
              :title="c.title"
            >
              {{ c.slot }}
            </span>
          </div>
          <div class="cb-slots__summary">{{ b.summary }}</div>
        </fieldset>
      </template>
      <div class="cb-slots__legend">
        <span><i class="cb-slot cb-slot--free" /> free</span>
        <span><i class="cb-slot cb-slot--faulty" /> reader not answering</span>
        <span><i class="cb-slot cb-slot--card" /> card, not identified</span>
        <span><i class="cb-slot cb-slot--discovered cb-slot--online" /> card online</span>
        <span><i class="cb-slot cb-slot--discovered cb-slot--active" /> authenticating</span>
        <span><i class="cb-slot cb-slot--discovered cb-slot--offline" /> card offline</span>
        <span><i class="cb-slot cb-slot--discovered cb-slot--unlinked" /> unknown card</span>
        <span><i class="cb-slot cb-slot--error" /> error</span>
      </div>
    </div>

    <div v-if="open">
      <!-- In the tile views the rows flow in a grid, and an opened card's
           panel spans the full grid width under its tile (see .cb-tiles). -->
      <div
        v-if="shownRows.length"
        :class="view === 'list' ? undefined : ['cb-tiles', `cb-tiles--${view}`]"
      >
        <CardRow
          v-for="e in shownRows"
          :key="e.key"
          :entry="e"
          :open="openRow === e.key"
          :view="view"
          :now="now"
          :cards="cards"
          :save="save"
          :remove="remove"
          @toggle="emit('row-toggle', e.key)"
          @close="emit('row-close')"
        />
      </div>
      <div v-if="rows.length === 0" class="cb-group__empty">{{ emptyText }}</div>
      <button
        v-if="rows.length > shownRows.length"
        type="button"
        class="cb-group__more"
        @click="emit('show-all')"
      >
        Show all {{ rows.length }} cards
      </button>
    </div>
  </section>
</template>

<script setup lang="ts">
import { computed, onUnmounted, ref, watch } from 'vue'
import CardRow from './CardRow.vue'
import type { SmartCard } from './models'
import {
  slotBlocks,
  slotsLine,
  summarize,
  type CardEntry,
  type CardGroup,
  type CardView,
  type StatusFilter,
} from './cardBoard'

const props = defineProps<{
  group: CardGroup
  /** Rows matching the filters, sorted. */
  rows: CardEntry[]
  /** How many rows to render before the "Show all" button. */
  limit: number
  view: CardView
  open: boolean
  openRow: string | null
  now: number
  cards: Record<string, SmartCard>
  save: (number: string, data: SmartCard) => Promise<boolean>
  remove: (number: string) => Promise<boolean>
}>()

const emit = defineEmits<{
  (e: 'toggle'): void
  (e: 'show-all'): void
  (e: 'pick-issue', status: StatusFilter): void
  (e: 'row-toggle', key: string): void
  (e: 'row-close'): void
}>()

// An opened card stays rendered even past the limit: the limit shrinks when
// the window narrows into the list view, and cutting the card off there would
// silently drop its half-typed name or pending remove confirmation.
const shownRows = computed(() => {
  const openAt = props.openRow ? props.rows.findIndex((e) => e.key === props.openRow) : -1
  return props.rows.slice(0, Math.max(props.limit, openAt + 1))
})
const summary = computed(() => summarize(props.group, props.now))

// The slot grid of a rack is opened per group and survives the parent
// replacing the rack list on every `rack-state` event: the toggle lives here,
// the blocks are recomputed from the fresh table.
const slotsOpen = ref(false)
const slotsLineText = computed(() => (props.group.rack ? slotsLine(props.group.rack) : ''))
const blocks = computed(() =>
  props.group.rack ? slotBlocks(props.group.rack, props.group.entries) : [],
)

const icon = computed(() =>
  props.group.kind === 'rack'
    ? 'mdi-server-network'
    : props.group.kind === 'reader'
      ? 'mdi-usb-port'
      : 'mdi-card-bulleted-off-outline',
)
const iconClass = computed(() =>
  props.group.kind === 'none'
    ? 'cb-tone--icon'
    : props.group.down
      ? 'cb-tone--danger'
      : 'cb-tone--ok',
)

const segments = computed(() => {
  const c = summary.value.counts
  return (
    [
      { status: 'active', n: c.active },
      { status: 'online', n: c.online },
      { status: 'unlinked', n: c.unlinked },
      { status: 'offline', n: c.offline },
      { status: 'absent', n: c.absent },
    ] as const
  ).filter((s) => s.n > 0)
})

const countLine = computed(() => {
  const { counts, total } = summary.value
  const online = counts.online + counts.active
  if (props.group.kind === 'none') return `${total} ${total === 1 ? 'card' : 'cards'}`
  if (props.group.kind === 'reader') return `${online} online`
  return `${online} / ${total} online`
})

// Shortcuts into the filters: each badge narrows the list to this group and
// that status. A disconnected rack is offline as a whole, so its per-card
// offline/attention counts would only repeat the header.
const issues = computed(() => {
  const { counts, attention } = summary.value
  const out: { status: StatusFilter; label: string }[] = []
  if (counts.unlinked) out.push({ status: 'unlinked', label: `${counts.unlinked} unknown` })
  if (attention && !props.group.down)
    out.push({ status: 'attention', label: `${attention} attention` })
  if (counts.offline && props.group.kind !== 'none' && !props.group.down)
    out.push({ status: 'offline', label: `${counts.offline} offline` })
  return out
})
const allGood = computed(
  () =>
    issues.value.length === 0 &&
    !props.group.down &&
    props.group.kind !== 'none' &&
    summary.value.total > 0,
)

const emptyText = computed(() => {
  if (props.group.entries.length) return 'No cards in this group match the filter'
  if (props.group.kind === 'reader') return 'No connected smart card readers'
  if (props.group.kind === 'rack') {
    if (props.group.down) return 'Rack disconnected'
    return scanning.value ? 'Scanning rack slots…' : 'No cards in the rack'
  }
  return 'No cards'
})

/**
 * Silence fallback for the scan indicator. `scan_complete` is the real signal
 * (the backend raises it when the server arms the rack's presence watch), but
 * it only ever arrives from the server — an older server that never sends
 * `watch`, or a broker that stays unreachable, would leave the indicator (and
 * suppress the "no cards" message) forever. So the indicator also ends after
 * this long without any progress: no new card reported, no state change. Long
 * enough that a slow slot walk (a few seconds per card) cannot trip it.
 */
const SCAN_SILENCE_TIMEOUT_MS = 30_000

const scanTimedOut = ref(false)
let scanTimer: ReturnType<typeof setTimeout> | undefined

function clearScanTimer() {
  if (scanTimer !== undefined) {
    clearTimeout(scanTimer)
    scanTimer = undefined
  }
}

// Any progress (connect, a new card row, a scan_complete flip) restarts the
// silence window; a finished or disconnected rack needs no timer at all.
//
// The sources are an ARRAY OF GETTERS on purpose: Vue compares each source's
// value element-wise, so the callback runs only when one of the three values
// actually changes. A single getter returning a tuple would produce a fresh
// array each run (compared with Object.is → always different), and since the
// parent replaces the whole rack list on every `rack-state` event, the timer
// would then restart on every emit from ANY rack — keeping the fallback from
// ever firing exactly where it is needed (an older server that never sends
// scan_complete, with any card actively exchanging).
watch(
  [
    () => props.group.rack?.connected,
    () => props.group.rack?.scan_complete,
    () => props.group.rack?.cards.length,
  ],
  ([connected, scanComplete]) => {
    clearScanTimer()
    scanTimedOut.value = false
    if (connected && !scanComplete) {
      scanTimer = setTimeout(() => {
        scanTimedOut.value = true
      }, SCAN_SILENCE_TIMEOUT_MS)
    }
  },
  { immediate: true },
)

onUnmounted(clearScanTimer)

/**
 * The rack is still being enumerated: connected, the backend has not yet
 * reported the scan as finished, and the silence fallback has not fired.
 */
const scanning = computed(
  () => !!props.group.rack?.connected && !props.group.rack.scan_complete && !scanTimedOut.value,
)
</script>
