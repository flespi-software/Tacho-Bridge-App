<template>
  <div class="cb-row" :class="{ 'cb-row--open': open, 'cb-row--warn': open && unlinked }">
    <!-- Collapsed line: slot, status dot, name + number, auth + expiry -->
    <button
      type="button"
      class="cb-row__line"
      :aria-expanded="isEmpty ? undefined : open"
      :disabled="isEmpty"
      @click="emit('toggle')"
    >
      <span class="cb-row__slot">{{ entry.slotLabel }}</span>
      <span
        class="cb-dot"
        :class="[`cb-dot--${entry.status}`, { 'cb-dot--pulse': entry.status === 'active' }]"
      />
      <span class="cb-row__main">
        <span
          class="cb-row__title"
          :class="{ 'cb-row__title--warn': unlinked, 'cb-row__title--muted': isEmpty }"
        >
          {{ rowTitle }}
        </span>
        <span v-if="rowSub" class="cb-row__sub">{{ rowSub }}</span>
      </span>
      <span class="cb-row__meta">
        <span :class="`cb-tone--${auth.tone}`">{{ auth.text }}</span>
        <span :class="`cb-tone--${expiry.tone}`">{{ expiry.text }}</span>
      </span>
      <span v-if="unlinked && entry.iccid" class="cb-link-pill">
        <q-icon name="mdi-link-variant" size="14px" />
        Link
      </span>
    </button>

    <!-- Configured card: details, rename, remove -->
    <div v-if="open && !unlinked && !isEmpty" class="cb-panel">
      <div class="cb-facts">
        <div v-for="f in facts" :key="f.k" class="cb-fact">
          <span class="cb-fact__k">{{ f.k }}</span>
          <span class="cb-fact__v" :class="f.tone ? `cb-tone--${f.tone}` : ''" :title="f.v">{{
            f.v
          }}</span>
        </div>
      </div>

      <template v-if="entry.card">
        <div class="row no-wrap items-end q-gutter-x-sm">
          <q-input
            v-model="draft"
            class="col"
            label="Card name"
            outlined
            dense
            :disable="busy"
            @keyup.enter="saveName"
          />
          <q-btn
            unelevated
            rounded
            no-caps
            class="cb-btn-primary"
            label="Save"
            :disable="draft.trim() === entry.name || busy"
            :loading="busy && action === 'rename'"
            @click="saveName"
          />
        </div>

        <div v-if="!confirming">
          <q-btn
            outline
            rounded
            no-caps
            dense
            class="cb-btn-danger q-px-md"
            label="Remove"
            :disable="busy"
            @click="confirming = true"
          />
        </div>
        <div v-else class="cb-confirm row no-wrap items-center">
          <span class="col">{{ removeText }}</span>
          <q-btn
            flat
            rounded
            no-caps
            dense
            class="q-px-sm"
            label="Cancel"
            @click="confirming = false"
          />
          <q-btn
            unelevated
            rounded
            no-caps
            dense
            color="negative"
            class="q-px-md"
            label="Remove"
            :loading="busy && action === 'remove'"
            @click="doRemove"
          />
        </div>
      </template>
    </div>

    <!-- Unknown card: link to a configured card or add a new one -->
    <div v-if="open && unlinked" class="cb-panel cb-panel--warn">
      <template v-if="entry.iccid">
        <div class="cb-panel__text">
          This card is in {{ entry.place }} but is not in the configuration. Link it to a configured
          card or add it as a new one.
        </div>
        <div class="cb-tabs" role="tablist">
          <button
            type="button"
            role="tab"
            :aria-selected="tab === 'existing'"
            :class="{ 'cb-tabs__on': tab === 'existing' }"
            @click="tab = 'existing'"
          >
            Configured card · {{ linkable.length }}
          </button>
          <button
            type="button"
            role="tab"
            :aria-selected="tab === 'new'"
            :class="{ 'cb-tabs__on': tab === 'new' }"
            @click="tab = 'new'"
          >
            New card
          </button>
        </div>

        <template v-if="tab === 'existing'">
          <q-input
            v-model="linkQuery"
            outlined
            dense
            clearable
            placeholder="Cards in the configuration without ICCID"
          >
            <template #prepend><q-icon name="mdi-magnify" size="xs" /></template>
          </q-input>
          <div class="cb-picklist">
            <button
              v-for="c in linkShown"
              :key="c.number"
              type="button"
              class="cb-pick"
              :class="{ 'cb-pick--on': linkPick === c.number }"
              :aria-pressed="linkPick === c.number"
              @click="linkPick = c.number"
            >
              <span class="cb-radio"><span /></span>
              <span class="cb-pick__main">
                <span class="cb-pick__name">{{ c.card.name || c.number }}</span>
                <span class="cb-pick__sub">
                  <span class="cb-mono">{{ c.number }}</span>
                  · {{ c.card.company_name || 'company not read yet' }}
                </span>
              </span>
            </button>
            <div v-if="linkShown.length === 0" class="cb-picklist__empty">
              {{
                linkable.length
                  ? 'No configured card matches the search.'
                  : 'No configured cards without ICCID. Use “New card”.'
              }}
            </div>
          </div>
          <div class="row justify-end">
            <q-btn
              unelevated
              rounded
              no-caps
              class="cb-btn-primary"
              label="Link to selected card"
              :disable="!canLink || busy"
              :loading="busy && action === 'link'"
              @click="doLink"
            />
          </div>
        </template>

        <template v-else>
          <div class="row no-wrap q-gutter-x-sm">
            <q-input
              :model-value="newNumber"
              class="col"
              input-class="cb-mono"
              label="Card number"
              outlined
              dense
              maxlength="16"
              :error="!!newNumber && !!newNumberError"
              :error-message="newNumberError"
              hide-bottom-space
              @update:model-value="(v) => (newNumber = String(v ?? '').toUpperCase())"
            />
            <q-input v-model="newName" class="col" label="Card name" outlined dense />
          </div>
          <div class="row no-wrap items-center">
            <span class="col cb-hint">{{
              newNumber ? (newNumberError ? '' : 'Looks good') : 'The number printed on the card'
            }}</span>
            <q-btn
              unelevated
              rounded
              no-caps
              class="cb-btn-primary"
              label="Add and link"
              :disable="!!newNumberError || busy"
              :loading="busy && action === 'add'"
              @click="doAdd"
            />
          </div>
        </template>
      </template>
      <div v-else class="cb-panel__text">
        The card's ICCID could not be read, so it cannot be linked. Reinsert the card.
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import type { SmartCard } from './models'
import { formatAuthDate, formatCardMeta, formatExpire } from './cardFormatters'
import {
  authLine,
  daysToExpiry,
  expiryLine,
  linkableCards,
  newCardNumberError,
  EXPIRY_WARN_DAYS,
  type CardEntry,
  type Tone,
} from './cardBoard'
import { notifySuccess, TOAST_SHORT } from 'src/composables/notify'

const props = defineProps<{
  entry: CardEntry
  open: boolean
  now: number
  cards: Record<string, SmartCard>
  // Both resolve to whether the change was persisted; failures are reported
  // to the user by the caller.
  save: (number: string, data: SmartCard) => Promise<boolean>
  remove: (number: string) => Promise<boolean>
}>()

const emit = defineEmits<{
  (e: 'toggle'): void
  (e: 'close'): void
}>()

const unlinked = computed(() => props.entry.status === 'unlinked')
const isEmpty = computed(() => props.entry.status === 'empty')

const rowTitle = computed(() => {
  if (unlinked.value) return 'Unknown card'
  return props.entry.name || props.entry.number
})
const rowSub = computed(() => {
  if (isEmpty.value) return 'no card inserted'
  if (unlinked.value) return props.entry.iccid ? `ICCID ${props.entry.iccid}` : 'ICCID not read'
  return props.entry.name ? props.entry.number : ''
})

const auth = computed(() => authLine(props.entry, props.now))
const expiry = computed(() => expiryLine(props.entry, props.now))

const facts = computed(() => {
  const card = props.entry.card
  const days = daysToExpiry(props.entry, props.now)
  const last = card?.last_auth
  const out: { k: string; v: string; tone?: Tone }[] = [
    { k: 'Card number', v: props.entry.number },
    {
      k: 'ICCID',
      v: props.entry.iccid || 'not linked yet',
      ...(props.entry.iccid ? {} : { tone: 'warn' as const }),
    },
    { k: 'Company', v: card?.company_name || '— (read on first auth)' },
    { k: 'Address', v: card?.company_address || '—' },
    { k: 'Card', v: (card && formatCardMeta(card)) || '—' },
    {
      k: 'Expires',
      v: expiry.value.text ? formatExpire(card?.expire) + expirySuffix(days) : '—',
      ...(days !== null && days <= EXPIRY_WARN_DAYS ? { tone: 'danger' as const } : {}),
    },
    {
      k: 'Last auth',
      v: last ? `${formatAuthDate(last[0])} · ${last[1] ? 'success' : 'fail'}` : 'never',
      ...(last && !last[1] ? { tone: 'danger' as const } : {}),
    },
    { k: 'Location', v: props.entry.place },
  ]
  return out
})

function expirySuffix(days: number | null): string {
  if (days === null) return ''
  if (days < 0) return ' · expired'
  return days <= EXPIRY_WARN_DAYS ? ` · in ${days} d` : ''
}

// Panel state, reset every time the row opens.
const draft = ref('')
const confirming = ref(false)
const tab = ref<'existing' | 'new'>('existing')
const linkQuery = ref<string | null>('')
const linkPick = ref<string | null>(null)
const newNumber = ref('')
const newName = ref('')
const busy = ref(false)
const action = ref<'rename' | 'remove' | 'link' | 'add' | null>(null)

const linkable = computed(() => linkableCards(props.cards))
const linkShown = computed(() => {
  const q = (linkQuery.value ?? '').trim().toLowerCase()
  if (!q) return linkable.value
  return linkable.value.filter((c) =>
    `${c.card.name ?? ''} ${c.number} ${c.card.company_name ?? ''}`.toLowerCase().includes(q),
  )
})
const canLink = computed(() => linkable.value.some((c) => c.number === linkPick.value))
const newNumberError = computed(() => newCardNumberError(newNumber.value.trim(), props.cards))

watch(
  () => props.open,
  (open) => {
    if (!open) return
    draft.value = props.entry.name
    confirming.value = false
    tab.value = linkable.value.length ? 'existing' : 'new'
    linkQuery.value = ''
    linkPick.value = linkable.value[0]?.number ?? null
    newNumber.value = ''
    newName.value = ''
  },
  { immediate: true },
)

async function run(
  kind: NonNullable<typeof action.value>,
  task: () => Promise<boolean>,
): Promise<boolean> {
  if (busy.value) return false
  busy.value = true
  action.value = kind
  try {
    return await task()
  } finally {
    busy.value = false
    action.value = null
  }
}

async function saveName(): Promise<void> {
  const name = draft.value.trim()
  if (!props.entry.card || name === props.entry.name) return
  // Only the form-owned fields: the backend keeps every absent field, so a
  // rename cannot clobber metadata the sniffer wrote meanwhile. The ICCID is
  // echoed unchanged — the backend never rewrites an ICCID that is already set.
  const ok = await run('rename', () =>
    props.save(props.entry.number, { name, iccid: props.entry.card?.iccid ?? '' }),
  )
  if (ok) notifySuccess('Name saved', TOAST_SHORT)
}

async function doRemove(): Promise<void> {
  const number = props.entry.number
  const ok = await run('remove', () => props.remove(number))
  if (!ok) return
  confirming.value = false
  emit('close')
  notifySuccess(`Card ${number} removed from the configuration`, TOAST_SHORT)
}

const removeText = computed(() =>
  props.entry.status === 'absent'
    ? `Remove ${props.entry.number} from the configuration?`
    : `Remove ${props.entry.number} from the configuration? Its session will be closed.`,
)

async function doLink(): Promise<void> {
  const pick = linkable.value.find((c) => c.number === linkPick.value)
  if (!pick) return
  const iccid = props.entry.iccid
  // The backend sets the name together with a first ICCID, so the current
  // name is sent back to keep it.
  const ok = await run('link', () => props.save(pick.number, { name: pick.card.name ?? '', iccid }))
  if (!ok) return
  emit('close')
  notifySuccess(`Linked ${iccid} to ${pick.card.name || pick.number}`, TOAST_SHORT)
}

async function doAdd(): Promise<void> {
  const number = newNumber.value.trim()
  if (newCardNumberError(number, props.cards)) return
  const iccid = props.entry.iccid
  const ok = await run('add', () => props.save(number, { name: newName.value.trim(), iccid }))
  if (!ok) return
  emit('close')
  notifySuccess(`Card ${number} added and linked`, TOAST_SHORT)
}
</script>
