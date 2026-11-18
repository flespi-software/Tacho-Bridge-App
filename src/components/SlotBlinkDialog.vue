<template>
  <q-dialog
    :model-value="slotNumber !== null"
    :persistent="
      phase === 'starting' || phase === 'stopping' || (untilStopped && phase === 'running')
    "
    @update:model-value="(v) => !v && requestClose()"
  >
    <q-card class="cb-blink">
      <q-card-section class="row items-center no-wrap q-pb-sm">
        <q-icon
          name="mdi-lightbulb-on-outline"
          size="26px"
          class="cb-blink__bulb q-mr-sm"
          :class="{ 'cb-blink__bulb--on': phase === 'running' }"
        />
        <div class="text-h6">{{ heading }}</div>
        <q-space />
        <q-btn
          flat
          round
          dense
          icon="mdi-close"
          aria-label="Close"
          :disable="busy"
          @click="requestClose"
        />
      </q-card-section>
      <q-card-section class="q-pt-xs">
        <template v-if="phase === 'ask' || phase === 'starting'">
          <div class="cb-blink__text">
            Blink the indicator of slot {{ slotNumber }} on {{ rackName }} to find it on the rack.
            {{
              untilStopped
                ? 'It will blink until you stop it.'
                : 'The indicator blinks for 6 seconds.'
            }}
          </div>
          <q-checkbox
            v-model="untilStopped"
            label="Blink until stopped"
            :disable="busy"
            class="q-mt-sm"
          />
        </template>
        <template v-else-if="phase === 'running' || phase === 'stopping'">
          <div class="cb-blink__text">Look for the blinking slot {{ slotNumber }} on the rack.</div>
          <q-linear-progress
            :value="progress"
            :indeterminate="untilStopped || phase === 'stopping'"
            size="6px"
            rounded
            color="primary"
            track-color="grey-5"
            class="q-mt-md"
            instant-feedback
            :aria-label="untilStopped ? 'Blinking until stopped' : 'Blink duration'"
          />
        </template>
        <div v-if="error" class="cb-blink__text cb-tone--danger q-mt-sm">{{ error }}</div>
      </q-card-section>
      <q-card-actions align="right" class="q-px-md q-pb-md">
        <template v-if="phase === 'ask' || phase === 'starting'">
          <q-btn flat no-caps label="Cancel" color="grey-7" :disable="busy" @click="requestClose" />
          <q-btn
            unelevated
            no-caps
            label="Start blinking"
            class="cb-btn-primary q-px-md"
            :loading="phase === 'starting'"
            @click="start"
          />
        </template>
        <q-btn
          v-else-if="phase === 'running' || phase === 'stopping'"
          unelevated
          no-caps
          label="Stop blinking"
          icon="mdi-stop"
          class="cb-btn-primary"
          :loading="phase === 'stopping'"
          @click="stopBlink"
        />
        <q-btn v-else flat no-caps label="Close" color="grey-7" @click="requestClose" />
      </q-card-actions>
    </q-card>
  </q-dialog>
</template>

<script setup lang="ts">
import { computed, onUnmounted, ref, watch } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { useTauriListeners } from 'src/composables/useTauriListeners'

const props = defineProps<{ rackId: string; rackName: string; slotNumber: number | null }>()
const emit = defineEmits<{ (e: 'close'): void }>()
type Phase = 'ask' | 'starting' | 'running' | 'stopping' | 'error'
const phase = ref<Phase>('ask')
const progress = ref(0)
const error = ref('')
const untilStopped = ref(false)
const busy = computed(() => phase.value === 'starting' || phase.value === 'stopping')
let timer: ReturnType<typeof setInterval> | undefined
let opening = 0
let activeTarget: { rackId: string; slot: number; continuous: boolean } | undefined

const heading = computed(() =>
  phase.value === 'running' || phase.value === 'stopping'
    ? `Slot ${props.slotNumber} is blinking`
    : `Blink slot ${props.slotNumber}?`,
)
const ERRORS: Record<string, string> = {
  not_local: 'The app does not drive this rack right now, so it cannot blink its slots.',
  card_busy: 'A card in this slot is authenticating right now. Try again when it has finished.',
  already_blinking: 'This slot is already blinking.',
  led_rejected: 'The rack did not take the indicator command.',
  no_indicator: 'The rack profile has no indicator commands, so slots cannot blink.',
  bad_slot: 'The rack has no such slot.',
  rack_gone: 'The rack has disconnected. Blinking has stopped.',
}

function stopTimer(): void {
  if (timer !== undefined) clearInterval(timer)
  timer = undefined
}
function releaseContinuous(): void {
  const target = activeTarget
  activeTarget = undefined
  if (target?.continuous) {
    void invoke('stop_blink_rack_slot', { rackId: target.rackId, slot: target.slot }).catch((e) =>
      console.error('Stopping slot blink failed:', e),
    )
  }
}
function closeLocal(): void {
  stopTimer()
  activeTarget = undefined
  opening++
  emit('close')
}
async function requestClose(): Promise<void> {
  if (busy.value) return
  if (activeTarget?.continuous) {
    await stopBlink()
    return
  }
  // A timed blink closed early keeps running in the backend: stop it, or the
  // slot answers "already blinking" until its time is up.
  const target = activeTarget
  if (target && phase.value === 'running') {
    void invoke('stop_blink_rack_slot', { rackId: target.rackId, slot: target.slot }).catch((e) =>
      console.error('Stopping slot blink failed:', e),
    )
  }
  closeLocal()
}
async function stopBlink(): Promise<void> {
  const target = activeTarget
  if (!target || busy.value) return
  const mine = opening
  stopTimer()
  phase.value = 'stopping'
  error.value = ''
  try {
    await invoke('stop_blink_rack_slot', { rackId: target.rackId, slot: target.slot })
    if (opening === mine) closeLocal()
  } catch (e) {
    if (opening !== mine) return
    phase.value = 'running'
    error.value = `The indicator could not be stopped. Try again (${String(e)}).`
  }
}

watch(
  () => props.slotNumber,
  (slot) => {
    stopTimer()
    releaseContinuous()
    opening++
    if (slot === null) return
    phase.value = 'ask'
    untilStopped.value = false
    progress.value = 0
    error.value = ''
  },
  { immediate: true },
)
onUnmounted(() => {
  opening++
  stopTimer()
  releaseContinuous()
})

const { on } = useTauriListeners()
const blinkListenerReady = on('rack-slot-blink-ended', (raw) => {
  if (!raw || typeof raw !== 'object' || (phase.value !== 'running' && phase.value !== 'starting'))
    return
  const event = raw as { rack_id?: unknown; slot?: unknown; reason?: unknown }
  if (event.rack_id !== activeTarget?.rackId || event.slot !== activeTarget?.slot) return
  const reason = String(event.reason)
  if (reason === 'done' || reason === 'stopped') {
    closeLocal()
    return
  }
  stopTimer()
  activeTarget = undefined
  error.value = ERRORS[reason] ?? 'Blinking has stopped.'
  phase.value = 'error'
})

async function start(): Promise<void> {
  const slot = props.slotNumber
  if (slot === null || phase.value !== 'ask') return
  const mine = opening
  const target = { rackId: props.rackId, slot, continuous: untilStopped.value }
  activeTarget = target
  phase.value = 'starting'
  let length: number
  try {
    await blinkListenerReady
    if (opening !== mine) return
    length = await invoke<number>('blink_rack_slot', {
      rackId: target.rackId,
      slot,
      untilStopped: target.continuous,
    })
  } catch (e) {
    if (opening !== mine) return
    if (String(e) === 'already_blinking') {
      // A blink this dialog did not start (webview reload, closed dialog):
      // attach to it so it can be stopped from here.
      target.continuous = true
      untilStopped.value = true
      phase.value = 'running'
      return
    }
    activeTarget = undefined
    error.value = ERRORS[String(e)] ?? `The slot could not blink (${String(e)}).`
    phase.value = 'error'
    return
  }
  if (opening !== mine) {
    if (target.continuous)
      void invoke('stop_blink_rack_slot', { rackId: target.rackId, slot }).catch((e) =>
        console.error('Stopping stale slot blink failed:', e),
      )
    return
  }
  if (phase.value !== 'starting') return
  phase.value = 'running'
  if (target.continuous) return
  const started = performance.now()
  timer = setInterval(() => {
    progress.value = Math.min(1, (performance.now() - started) / Math.max(1, length))
    if (progress.value >= 1) closeLocal()
  }, 50)
}
</script>
