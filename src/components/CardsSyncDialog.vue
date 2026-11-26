<template>
  <q-dialog :model-value="conflict !== null && !postponed" persistent>
    <q-card style="width: 460px; max-width: 95vw">
      <q-card-section class="row items-center q-pb-sm">
        <q-icon name="mdi-sync-alert" size="28px" color="warning" class="q-mr-sm" />
        <div class="text-h6">Card list differs from the server</div>
      </q-card-section>

      <q-separator />

      <q-card-section v-if="conflict" class="q-pt-md q-pb-sm">
        <div class="text-body2 text-grey-7 q-mb-md">
          The card list was changed both in this application and on the server. Choose which one to
          keep; until then the local list stays in use.
        </div>
        <div class="text-body2">
          Server:
          {{ conflict.server_cards === null ? 'unknown number of' : conflict.server_cards }}
          card(s), saved {{ serverUpdated }}
        </div>
        <div class="text-body2">Local: {{ conflict.local_cards }} card(s)</div>
        <div v-if="conflict.server_cards === null" class="text-caption text-grey-7 q-mt-sm">
          The server sent no card list this time, so it can be taken after the next connection.
        </div>
      </q-card-section>

      <q-card-actions align="right" class="q-px-md q-pb-md">
        <q-btn
          flat
          label="Later"
          color="grey-7"
          :disable="resolving !== null"
          @click="postponed = true"
        >
          <q-tooltip>
            Keep working with the local list; asked again on the next connection.
          </q-tooltip>
        </q-btn>
        <q-btn
          outline
          rounded
          label="Take from server"
          color="primary"
          :loading="resolving === 'server'"
          :disable="resolving !== null || conflict?.server_cards === null"
          @click="resolve('server')"
        >
          <q-tooltip>
            Replace the local list; the current config is kept as config.yaml.before-sync.
          </q-tooltip>
        </q-btn>
        <q-btn
          unelevated
          rounded
          label="Keep local"
          color="primary"
          :loading="resolving === 'local'"
          :disable="resolving !== null"
          @click="resolve('local')"
        >
          <q-tooltip>Replace the list on the server with the local one.</q-tooltip>
        </q-btn>
      </q-card-actions>
    </q-card>
  </q-dialog>
</template>

<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { notifyError, notifySuccess, TOAST_SHORT } from 'src/composables/notify'
import type { CardsSyncConflict } from 'src/components/models'

const props = defineProps<{
  // The open conflict; null when there is none.
  conflict: CardsSyncConflict | null
}>()

// "Later" hides the dialog until the backend reports the conflict again.
const postponed = ref(false)
const resolving = ref<'server' | 'local' | null>(null)

watch(
  () => props.conflict,
  () => {
    postponed.value = false
  },
)

const serverUpdated = computed(() =>
  props.conflict && props.conflict.server_updated > 0
    ? new Date(props.conflict.server_updated * 1000).toLocaleString()
    : 'at an unknown time',
)

// The backend clears the conflict (a null `cards-sync-conflict` event) once
// the choice is taken, which closes the dialog.
async function resolve(choice: 'server' | 'local') {
  if (resolving.value !== null) return
  resolving.value = choice
  try {
    await invoke('resolve_cards_sync', { choice })
    notifySuccess(
      choice === 'server' ? 'Card list taken from the server.' : 'Card list sent to the server.',
      TOAST_SHORT,
    )
  } catch (error) {
    console.error('Failed to resolve the card list conflict:', error)
    notifyError('Failed to synchronize the card list', error)
  } finally {
    resolving.value = null
  }
}
</script>
