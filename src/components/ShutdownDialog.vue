<template>
  <q-dialog :model-value="state !== null" persistent :transition-duration="0" no-shake>
    <q-card class="app-shutdown" role="status" aria-live="polite" aria-labelledby="shutdown-title">
      <span class="app-shutdown__icon">
        <q-icon name="mdi-shield-check-outline" size="32px" />
      </span>
      <h2 id="shutdown-title">Safe shutdown</h2>
      <p>Preparing to close Tacho Bridge safely.</p>
      <div v-if="state && state.slots > 0" class="app-shutdown__slots">
        <q-icon name="mdi-server-network" size="16px" />
        {{ state.slots }} slot {{ state.slots === 1 ? 'indicator' : 'indicators' }}
      </div>
      <div class="app-shutdown__status">
        <span>{{ waiting ? 'Finishing up…' : phaseLabel }}</span>
        <span class="app-shutdown__percent">{{ Math.floor(progress * 100) }}%</span>
      </div>
      <q-linear-progress
        :value="progress"
        :indeterminate="waiting"
        color="primary"
        size="7px"
        rounded
        instant-feedback
        :aria-label="waiting ? 'Waiting for shutdown to finish' : 'Estimated shutdown progress'"
      />
      <div class="app-shutdown__hint">
        <q-spinner v-if="waiting" size="16px" color="primary" />
        <span>{{
          waiting ? 'Still working. The app will close automatically.' : estimateLabel
        }}</span>
      </div>
    </q-card>
  </q-dialog>
</template>

<script setup lang="ts">
import { computed, onUnmounted, ref, watch } from 'vue'
import { shutdownProgress, type ShutdownState } from './shutdownProgress'

const props = defineProps<{ state: ShutdownState | null }>()
const progress = ref(0)
let timer: ReturnType<typeof setInterval> | undefined

const waiting = computed(() => progress.value >= 1)
const phaseLabel = computed(() => {
  if (props.state?.phase === 'connections') return 'Closing connections…'
  if (props.state?.phase === 'installing') return 'Preparing to restart…'
  return 'Turning off slot indicators…'
})
const estimateLabel = computed(() => {
  const seconds = Math.max(1, Math.ceil((props.state?.estimated_ms ?? 0) / 1000))
  return `Estimated time: about ${seconds} ${seconds === 1 ? 'second' : 'seconds'}`
})

function clearTimer(): void {
  if (timer !== undefined) clearInterval(timer)
  timer = undefined
}

watch(
  () => props.state,
  (state) => {
    clearTimer()
    if (!state) {
      progress.value = 0
      return
    }
    const start = performance.now() - state.elapsed_ms
    const tick = () => {
      progress.value = shutdownProgress(state.estimated_ms, performance.now() - start)
    }
    tick()
    timer = setInterval(tick, 40)
  },
  { immediate: true },
)
onUnmounted(clearTimer)
</script>
