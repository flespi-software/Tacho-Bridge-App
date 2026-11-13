<template>
  <!-- Blink a rack slot to find it on the rack: ask, then a progress line for
       as long as the slot blinks, then the dialog closes by itself. -->
  <q-dialog :model-value="slotNumber !== null" @update:model-value="(v) => !v && close()">
    <q-card class="cb-blink">
      <q-card-section class="row items-center no-wrap q-pb-sm">
        <!-- The rack's two indicator colours: split while asking, alternating
             in the rack's rhythm while the slot blinks. -->
        <q-icon
          name="mdi-lightbulb-on-outline"
          size="26px"
          class="cb-blink__bulb q-mr-sm"
          :class="{ 'cb-blink__bulb--on': phase === 'running' }"
        />
        <div class="text-h6">{{ heading }}</div>
        <q-space />
        <q-btn flat round dense icon="mdi-close" aria-label="Close" @click="close" />
      </q-card-section>

      <q-card-section class="q-pt-xs">
        <div v-if="phase === 'ask' || phase === 'starting'" class="cb-blink__text">
          The indicator of slot {{ slotNumber }} on {{ rackName }} blinks for a few seconds so you
          can find it on the rack.
        </div>
        <template v-else-if="phase === 'running'">
          <div class="cb-blink__text">Look for the blinking slot {{ slotNumber }} on the rack.</div>
          <q-linear-progress
            :value="progress"
            size="6px"
            rounded
            color="primary"
            track-color="grey-5"
            class="q-mt-md"
            instant-feedback
          />
        </template>
        <div v-else class="cb-blink__text cb-tone--danger">{{ error }}</div>
      </q-card-section>

      <q-card-actions v-if="phase !== 'running'" align="right" class="q-px-md q-pb-md">
        <template v-if="phase === 'ask' || phase === 'starting'">
          <q-btn flat no-caps label="No" color="grey-7" @click="close" />
          <q-btn
            unelevated
            rounded
            no-caps
            label="Yes"
            class="cb-btn-primary q-px-md"
            :loading="phase === 'starting'"
            @click="start"
          />
        </template>
        <q-btn v-else flat no-caps label="Close" color="grey-7" @click="close" />
      </q-card-actions>
    </q-card>
  </q-dialog>
</template>

<script setup lang="ts">
import { computed, onUnmounted, ref, watch } from 'vue'
import { invoke } from '@tauri-apps/api/core'

const props = defineProps<{
  rackId: string
  rackName: string
  /** The slot to blink; null keeps the dialog closed. */
  slotNumber: number | null
}>()

const emit = defineEmits<{
  (e: 'close'): void
}>()

type Phase = 'ask' | 'starting' | 'running' | 'error'
const phase = ref<Phase>('ask')
const progress = ref(0)
const error = ref('')
let timer: ReturnType<typeof setInterval> | undefined

const heading = computed(() =>
  phase.value === 'running'
    ? `Slot ${props.slotNumber} is blinking`
    : `Blink slot ${props.slotNumber}?`,
)

// Refusals of the backend (`local::blink`), in the user's words.
const ERRORS: Record<string, string> = {
  not_local: 'The app does not drive this rack right now, so it cannot blink its slots.',
  card_busy: 'A card in this slot is authenticating right now. Try again when it has finished.',
  already_blinking: 'This slot is already blinking.',
  led_rejected: 'The rack did not take the indicator command.',
  no_indicator: 'The rack profile has no indicator commands, so slots cannot blink.',
  bad_slot: 'The rack has no such slot.',
}

function stopTimer(): void {
  if (timer !== undefined) clearInterval(timer)
  timer = undefined
}

function close(): void {
  stopTimer()
  emit('close')
}

// Every opening starts from the question. The blink itself keeps running on
// the rack if the dialog is closed early; only the progress line goes away.
// Each opening has its own number, so the answer to a request sent from an
// earlier opening (closed and reopened on the same slot meanwhile) is ignored.
let opening = 0
watch(
  () => props.slotNumber,
  (slot) => {
    stopTimer()
    opening++
    if (slot === null) return
    phase.value = 'ask'
    progress.value = 0
    error.value = ''
  },
  { immediate: true },
)
onUnmounted(stopTimer)

async function start(): Promise<void> {
  const slot = props.slotNumber
  if (slot === null || phase.value !== 'ask') return
  const mine = opening
  phase.value = 'starting'
  let length: number
  try {
    length = await invoke<number>('blink_rack_slot', { rackId: props.rackId, slot })
  } catch (e) {
    if (opening !== mine) return
    const code = String(e)
    error.value = ERRORS[code] ?? `The slot could not blink (${code}).`
    phase.value = 'error'
    return
  }
  // the dialog was closed (or reopened) while the rack answered
  if (opening !== mine) return
  phase.value = 'running'
  // A plain interval rather than requestAnimationFrame, which WebKit pauses
  // for a window that is not on screen: the dialog must still close on time.
  const started = Date.now()
  timer = setInterval(() => {
    progress.value = Math.min(1, (Date.now() - started) / length)
    if (progress.value >= 1) close()
  }, 50)
}
</script>
