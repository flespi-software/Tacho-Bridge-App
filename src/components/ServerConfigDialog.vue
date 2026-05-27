<template>
  <q-dialog :model-value="modelValue" persistent @update:model-value="$emit('update:modelValue', $event)">
    <q-card style="width: 420px; max-width: 90vw">
      <q-card-section class="row items-center q-pb-sm">
        <q-icon name="mdi-server-network" size="28px" color="primary" class="q-mr-sm" />
        <div class="text-h6">Server configuration</div>
        <q-space />
        <q-btn flat round dense icon="mdi-close" v-close-popup />
      </q-card-section>

      <q-separator />

      <q-card-section class="q-pt-md q-pb-sm">
        <q-input
          label="App ident"
          outlined
          dense
          v-model="identInput"
          autofocus
          maxlength="16"
          @keyup.enter="$emit('update:modelValue', false)"
          :error="!isIdentValid"
          error-message="Must be TBA + 13 digits, e.g. TBA0000000000001"
          hide-bottom-space
          class="q-mb-sm"
        >
          <template v-slot:prepend>
            <q-icon name="mdi-identifier" size="xs" />
          </template>
        </q-input>
        <q-input
          label="Server address"
          outlined
          dense
          v-model="hostValue"
          @keyup.enter="$emit('update:modelValue', false)"
        >
          <template v-slot:prepend>
            <q-icon name="mdi-server-network" size="xs" />
          </template>
        </q-input>
      </q-card-section>

      <q-card-actions align="right" class="q-px-md q-pb-md">
        <q-btn flat label="Cancel" color="grey-7" v-close-popup />
        <q-btn
          unelevated
          rounded
          label="Save"
          color="primary"
          v-close-popup
          @click="saveServerConfig"
        />
      </q-card-actions>
    </q-card>
  </q-dialog>
</template>

<script setup lang="ts">
import { useQuasar, Notify } from 'quasar'
import { ref, computed } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

defineProps<{
  modelValue: boolean
}>()

defineEmits<{
  'update:modelValue': [value: boolean]
}>()

const TBA_IDENT_REGEXP = /^TBA\d{13}$/
const ident = ref('')
const identInput = computed({
  get: () => `TBA${ident.value}`,
  set: (val) => {
    ident.value = val.startsWith('TBA') ? val.slice(3) : ''
  },
})
const isIdentValid = computed(() => TBA_IDENT_REGEXP.test(identInput.value))

const hostValue = ref('')

const $q = useQuasar()

function currentThemeLabel(): string {
  if ($q.dark.mode === 'auto') return 'Auto'
  return $q.dark.isActive ? 'Dark' : 'Light'
}

const saveServerConfig = async () => {
  const theme = currentThemeLabel()
  console.log(`server_address: ${hostValue.value}, ident: ${identInput.value}, theme: ${theme}`)

  try {
    const response = await invoke('update_server', {
      host: hostValue.value,
      ident: identInput.value,
      theme,
    })

    console.log('Response from update_server:', response)

    Notify.create({
      message: 'Server configuration has been updated.',
      color: 'green',
      position: 'bottom',
      timeout: 3000,
    })

    await invoke('manual_sync_cards', {
      readername: "",
      restart: true,
    })
    console.log('Server configuration updated successfully_1')
    await invoke('app_connection')
    console.log('Server configuration updated successfully_2')
  } catch (error) {
    console.error('Error updating server configuration:', error)
    Notify.create({
      message: 'Failed to update server configuration.',
      color: 'red',
      position: 'bottom',
      timeout: 3000,
    })
  }
}

listen('global-config-server', (event) => {
  const payload = event.payload as {
    host: string
    ident: string
  }
  hostValue.value = payload.host
  identInput.value = payload.ident
}).catch((error) => {
  console.error('Error listening to global-config-server:', error)
})
</script>
