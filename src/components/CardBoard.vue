<template>
  <!-- Unified card list: every reader, rack slot and configured card in one
       place, grouped by location, with search and status filters on top. -->
  <div class="card-board">
    <div class="cb-toolbar">
      <div class="cb-toolbar__inner">
        <div class="row no-wrap items-center q-gutter-x-sm">
          <div class="col cb-toolbar__total">{{ totalLine }}</div>
          <q-btn
            unelevated
            rounded
            no-caps
            dense
            color="positive"
            icon="mdi-plus"
            label="Add card"
            class="q-px-md"
            @click="openAddDialog"
          />
        </div>
        <div class="row no-wrap q-gutter-x-sm">
          <q-input
            v-model="search"
            class="col"
            outlined
            dense
            clearable
            placeholder="Name, card number, ICCID, company, slot"
            aria-label="Search cards"
          >
            <template #prepend><q-icon name="mdi-magnify" size="xs" /></template>
          </q-input>
          <q-select
            v-model="location"
            :options="locationOptions"
            emit-value
            map-options
            outlined
            dense
            options-dense
            aria-label="Location"
            class="cb-toolbar__loc"
          />
          <q-select
            v-model="sort"
            :options="SORT_OPTIONS"
            emit-value
            map-options
            outlined
            dense
            options-dense
            aria-label="Sort by"
            class="cb-toolbar__sort"
          />
        </div>
        <div class="cb-chips">
          <button
            v-for="c in chips"
            :key="c.id"
            type="button"
            class="cb-chip"
            :class="{ 'cb-chip--on': status === c.id }"
            :aria-pressed="status === c.id"
            @click="status = status === c.id ? 'all' : c.id"
          >
            <span class="cb-dot cb-dot--chip" :class="`cb-dot--${c.dot}`" />
            {{ c.label }}
            <b>{{ c.count }}</b>
          </button>
          <button v-if="filtering" type="button" class="cb-reset" @click="resetFilters">
            Reset
          </button>
        </div>
      </div>
    </div>

    <div class="cb-scroll">
      <div class="cb-scroll__inner">
        <!-- v-show rather than v-if: a group hidden by a filter keeps its
             rack scan timer instead of restarting it on the way back. -->
        <CardGroup
          v-for="v in views"
          v-show="v.visible"
          :key="v.group.id"
          :group="v.group"
          :rows="v.rows"
          :limit="v.limit"
          :open="v.open"
          :open-row="openRow"
          :now="now"
          :cards="cards"
          :save="save"
          :remove="remove"
          @toggle="toggleGroup(v.group.id, !v.open)"
          @show-all="showAll[v.group.id] = true"
          @pick-issue="(st) => pickIssue(v.group.id, st)"
          @row-toggle="(key) => (openRow = openRow === key ? null : key)"
          @row-close="openRow = null"
        />
        <div v-if="narrowing && !views.some((v) => v.visible)" class="cb-nothing">
          No cards match the filter.
          <button type="button" class="cb-reset" @click="resetFilters">Reset</button>
        </div>
      </div>
    </div>

    <q-dialog v-model="addOpen">
      <q-card style="min-width: 420px">
        <q-card-section class="row items-center q-pb-sm">
          <q-icon name="mdi-card-plus" size="28px" color="primary" class="q-mr-sm" />
          <div class="text-h6">Add card</div>
          <q-space />
          <q-btn flat round dense icon="mdi-close" v-close-popup />
        </q-card-section>

        <q-separator />

        <q-card-section class="q-pt-md q-pb-sm q-gutter-y-sm">
          <q-input
            :model-value="addNumber"
            label="Card Number"
            outlined
            dense
            autofocus
            maxlength="16"
            input-class="cb-mono"
            :error="!!addNumber && !!addNumberError"
            :error-message="addNumberError"
            :hint="addNumber ? (addNumberError ? '' : 'Looks good') : 'Printed on the card'"
            @update:model-value="(v) => (addNumber = String(v ?? '').toUpperCase())"
          >
            <template #prepend><q-icon name="mdi-numeric" size="xs" /></template>
          </q-input>
          <q-input v-model="addName" label="Card Name" outlined dense @keyup.enter="saveNewCard">
            <template #prepend><q-icon name="mdi-tag-outline" size="xs" /></template>
          </q-input>
          <q-select
            v-model="addIccid"
            :options="unknownOptions"
            emit-value
            map-options
            outlined
            dense
            options-dense
            label="Link to an inserted card (optional)"
            hint="Unknown cards currently in readers and racks. Leave empty to add the card without ICCID."
          >
            <template #prepend><q-icon name="mdi-chip" size="xs" /></template>
          </q-select>
        </q-card-section>

        <q-card-actions align="right" class="q-px-md q-pb-md">
          <q-btn flat no-caps label="Cancel" color="grey-7" v-close-popup />
          <q-btn
            unelevated
            rounded
            no-caps
            label="Save"
            class="cb-btn-primary q-px-md"
            :disable="!!addNumberError"
            :loading="adding"
            @click="saveNewCard"
          />
        </q-card-actions>
      </q-card>
    </q-dialog>
  </div>
</template>

<script setup lang="ts">
import { computed, onUnmounted, reactive, ref, watch } from 'vue'
import CardGroup from './CardGroup.vue'
import type { RackState, Reader, SmartCard } from './models'
import {
  buildGroups,
  matchesSearch,
  matchesStatus,
  newCardNumberError,
  NOT_INSERTED_GROUP,
  SORTERS,
  type SortKey,
  type StatusFilter,
} from './cardBoard'
import { notifySuccess, TOAST_SHORT } from 'src/composables/notify'

const props = defineProps<{
  readers: Reader[]
  racks: RackState[]
  cards: Record<string, SmartCard>
  save: (number: string, data: SmartCard) => Promise<boolean>
  remove: (number: string) => Promise<boolean>
}>()

const SORT_OPTIONS: { value: SortKey; label: string }[] = [
  { value: 'slot', label: 'By slot' },
  { value: 'name', label: 'By name' },
  { value: 'exp', label: 'By expiry' },
  { value: 'auth', label: 'By last auth' },
]

// Rows rendered per group before "Show all": a rack can hold 180+ cards, and
// a filtered view is short enough to show more at once.
const ROW_LIMIT = 10
const ROW_LIMIT_FILTERED = 40

const search = ref<string | null>('')
const location = ref<string>('all')
const status = ref<StatusFilter>('all')
const sort = ref<SortKey>('slot')
const openRow = ref<string | null>(null)
const showAll = reactive<Record<string, boolean>>({})

// Relative times ("5 min ago") and expiry checks follow the clock.
const now = ref(Date.now())
const clock = setInterval(() => (now.value = Date.now()), 30_000)
onUnmounted(() => clearInterval(clock))

// Which groups are expanded is remembered across launches. Storage may be
// unavailable (private mode, blocked site data), so every access is guarded
// and the defaults apply without it.
const OPEN_GROUPS_KEY = 'tba.cardBoard.openGroups'
const openGroups = reactive<Record<string, boolean>>(loadOpenGroups())

function loadOpenGroups(): Record<string, boolean> {
  try {
    const raw = localStorage.getItem(OPEN_GROUPS_KEY)
    const parsed: unknown = raw ? JSON.parse(raw) : null
    if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
      return Object.fromEntries(
        Object.entries(parsed).filter((kv): kv is [string, boolean] => typeof kv[1] === 'boolean'),
      )
    }
  } catch {
    // fall through to the defaults
  }
  return {}
}

function toggleGroup(id: string, open: boolean): void {
  openGroups[id] = open
  try {
    localStorage.setItem(OPEN_GROUPS_KEY, JSON.stringify(openGroups))
  } catch {
    // not persisted; the toggle still applies for this session
  }
}

// Readers and racks start expanded, the not-inserted list collapsed — the
// same as the old screen, where the card list was folded.
function isGroupOpen(id: string): boolean {
  return openGroups[id] ?? id !== NOT_INSERTED_GROUP
}

const groups = computed(() => buildGroups(props.readers, props.racks, props.cards))

// A rack that disappears must not leave the selector pointing at nothing.
watch(groups, (gs) => {
  if (location.value !== 'all' && !gs.some((g) => g.id === location.value)) location.value = 'all'
})

const query = computed(() => (search.value ?? '').trim().toLowerCase())
const narrowing = computed(() => !!query.value || status.value !== 'all')
const filtering = computed(() => narrowing.value || location.value !== 'all')

const locationOptions = computed(() => [
  { value: 'all', label: 'All locations' },
  ...groups.value.map((g) => ({ value: g.id, label: g.label })),
])

const inLocation = computed(() =>
  groups.value.filter((g) => location.value === 'all' || g.id === location.value),
)

const chips = computed(() => {
  const pool = inLocation.value
    .flatMap((g) => g.entries)
    .filter((e) => e.status !== 'empty' && matchesSearch(e, query.value))
  const defs: { id: StatusFilter; label: string; dot: string }[] = [
    { id: 'all', label: 'All', dot: 'offline' },
    { id: 'active', label: 'Authenticating', dot: 'active' },
    { id: 'online', label: 'Online', dot: 'online' },
    { id: 'offline', label: 'Offline', dot: 'offline' },
    { id: 'unlinked', label: 'Unknown', dot: 'unlinked' },
    { id: 'attention', label: 'Attention', dot: 'attention' },
  ]
  return defs.map((d) => ({
    ...d,
    count: pool.filter((e) => matchesStatus(e, d.id, now.value)).length,
  }))
})

const views = computed(() => {
  const shown = new Set(inLocation.value.map((g) => g.id))
  return groups.value.map((group) => {
    const rows = group.entries
      .filter((e) => matchesSearch(e, query.value) && matchesStatus(e, status.value, now.value))
      .sort(SORTERS[sort.value])
    return {
      group,
      rows,
      visible: shown.has(group.id) && (!narrowing.value || rows.length > 0),
      // A filtered view opens every matching group, or the matches would be
      // hidden behind a fold the user never opened.
      open: narrowing.value || isGroupOpen(group.id),
      limit: showAll[group.id] ? rows.length : narrowing.value ? ROW_LIMIT_FILTERED : ROW_LIMIT,
    }
  })
})

function plural(n: number, one: string): string {
  return `${n} ${n === 1 ? one : `${one}s`}`
}

const totalLine = computed(() => {
  const total = groups.value.reduce(
    (n, g) => n + g.entries.filter((e) => e.status !== 'empty').length,
    0,
  )
  const parts = [plural(total, 'card')]
  if (props.racks.length) parts.push(plural(props.racks.length, 'rack'))
  parts.push(plural(props.readers.length, 'reader'))
  return parts.join(' · ')
})

function pickIssue(groupId: string, st: StatusFilter): void {
  search.value = ''
  location.value = groupId
  status.value = st
  openRow.value = null
}

function resetFilters(): void {
  search.value = ''
  status.value = 'all'
  location.value = 'all'
}

// Add card dialog
const addOpen = ref(false)
const adding = ref(false)
const addNumber = ref('')
const addName = ref('')
const addIccid = ref('')

const addNumberError = computed(() => newCardNumberError(addNumber.value.trim(), props.cards))

const unknownOptions = computed(() => [
  { value: '', label: '— none —' },
  ...groups.value
    .flatMap((g) => g.entries)
    .filter((e) => e.status === 'unlinked' && e.iccid)
    .map((e) => ({ value: e.iccid, label: `${e.place} · ${e.iccid}` })),
])

function openAddDialog(): void {
  addNumber.value = ''
  addName.value = ''
  addIccid.value = ''
  addOpen.value = true
}

async function saveNewCard(): Promise<void> {
  const number = addNumber.value.trim()
  if (adding.value || newCardNumberError(number, props.cards)) return
  adding.value = true
  // Only the form-owned fields; the backend keeps the rest.
  const saved = await props.save(number, { name: addName.value.trim(), iccid: addIccid.value })
  adding.value = false
  if (!saved) return
  addOpen.value = false
  notifySuccess(`Card ${number} saved`, TOAST_SHORT)
}
</script>
