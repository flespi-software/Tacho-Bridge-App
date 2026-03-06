<template>
  <q-dialog :model-value="modelValue" persistent @update:model-value="$emit('update:modelValue', $event)">
    <q-card style="min-width: 350px">
      <q-card-section>
        <div class="text-h6">Server configuration</div>
      </q-card-section>

      <q-card-section class="q-pt-none">
        <q-input
          label="App ident"
          dense
          v-model="identInput"
          autofocus
          @keyup.enter="$emit('update:modelValue', false)"
          :error="!isIdentValid"
          error-message="The identifier must have the prefix TBA + 13 digits. For example: TBA0000000000001."
        />
        <q-input
          label="Server address"
          dense
          v-model="hostValue"
          @keyup.enter="$emit('update:modelValue', false)"
        />
        <div class="q-mt-md row items-center">
          <div class="text-caption text-grey q-mr-md">Theme</div>
          <q-btn-toggle
            v-model="selectedTheme"
            no-caps
            rounded
            unelevated
            toggle-color="primary"
            :options="[
              { icon: 'mdi-white-balance-sunny', value: 'Light' },
              { icon: 'mdi-brightness-auto', value: 'Auto' },
              { icon: 'mdi-weather-night', value: 'Dark' },
            ]"
            @update:model-value="changeTheme"
          />
        </div>
      </q-card-section>
      <q-card-actions align="right" class="text-primary">
        <q-btn flat label="Cancel" v-close-popup />
        <q-btn
          flat
          label="Save"
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
    ident.value = val.replace(/^TBA/, '')
  },
})
const isIdentValid = computed(() => TBA_IDENT_REGEXP.test(identInput.value))

const hostValue = ref('')

const $q = useQuasar()
const selectedTheme = ref('')

const changeTheme = (value: string) => {
  switch (value) {
    case 'Auto':
      $q.dark.set('auto')
      break
    case 'Dark':
      $q.dark.set(true)
      break
    case 'Light':
      $q.dark.set(false)
      break
    default:
      console.log('Unknown theme value:', value)
  }
}

const saveServerConfig = async () => {
  console.log(`server_address: ${hostValue.value}, ident: ${identInput.value}, theme: ${selectedTheme.value}`)

  try {
    const response = await invoke('update_server', {
      host: hostValue.value,
      ident: identInput.value,
      theme: selectedTheme.value,
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

// Listen for config updates from backend
listen('global-config-server', (event) => {
  const payload = event.payload as {
    host: string
    ident: string
    dark_theme: string
  }
  console.log('host:', payload.host, 'ident:', payload.ident, 'dark_theme:', payload.dark_theme)

  hostValue.value = payload.host
  identInput.value = payload.ident

  changeTheme(payload.dark_theme)
  selectedTheme.value = payload.dark_theme
}).catch((error) => {
  console.error('Error listening to global-config-server:', error)
})
</script>
