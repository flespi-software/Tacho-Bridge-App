<template>
  <q-layout view="lHh Lpr lFf">
    <q-header elevated>
      <q-toolbar>
        <!-- Title in the up of the app -->
        <q-toolbar-title class="q-ml-md">
          Tacho Bridge Application
          <q-icon name="mdi-record-circle-outline" class="q-ml-md" />
        </q-toolbar-title>

        <!-- Button of the Dialog of the server configuration -->
        <div class="q-pa-xs q-gutter-sm">
          <q-btn flat round icon="mdi-cog" @click="config = true" />
          <!-- Dialog window for the entering the Server Address value -->
          <q-dialog v-model="config" persistent>
            <q-card style="min-width: 350px">
              <q-card-section>
                <div class="text-h6">Server configuration</div>
              </q-card-section>

              <q-card-section class="q-pt-none">
                <q-input
                  label="App ident"
                  :dense="dense"
                  v-model="identInput"
                  autofocus
                  @keyup.enter="config = false"
                  :error="!isIndetValid"
                  error-message="The identifier must have the prefix TBA + 13 digits. For example: TBA0000000000001."
                />
                <q-input
                  label="Server address"
                  :dense="dense"
                  v-model="host"
                  autofocus
                  @keyup.enter="config = false"
                />
                <q-select
                  v-model="selectedTheme"
                  :options="themeOptions"
                  label="Theme"
                  @update:model-value="changeTheme"
                />
              </q-card-section>
              <q-card-actions align="right" class="text-primary">
                <q-btn flat label="Cancel" v-close-popup />
                <q-btn
                  flat
                  label="Save"
                  v-close-popup
                  @click="saveServerConfig(host, identInput, selectedTheme)"
                />
              </q-card-actions>
            </q-card>
          </q-dialog>
        </div>
      </q-toolbar>
    </q-header>

    <q-page-container>
      <router-view />
    </q-page-container>
  </q-layout>
</template>

<script setup lang="ts">
import { useQuasar, Notify } from 'quasar'
import { ref, computed, defineComponent } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { listen, emit } from '@tauri-apps/api/event'
import 'animate.css'

const TBA_IDENT_REGEXP = /^TBA\d{13}$/ // Regular expression for the company card number
const isIndetValid = computed(() => TBA_IDENT_REGEXP.test(identInput.value))
// const ident = ref('') // App ident. The unique identifier of the application. Config
const ident = ref('') // Input field for ident without prefix
const identInput = computed({
  get: () => `TBA${ident.value}`, // Без пробела
  set: (val) => {
    ident.value = val.replace(/^TBA/, '') // Убираем "TBA", если пользователь вводит его вручную
  },
})
// Server configuration dialog
const config = ref(false) // Config dialog
const host = ref('') // Server address. Config
// const dark_theme = ref(''); // dark_theme of the application (dark or light). Config
const dense = ref(true) // Dense mode

/*
  /////////// Dark theme switcher ///////////
*/
const $q = useQuasar()
const selectedTheme = ref('') // Default theme
const themeOptions = ['Auto', 'Dark', 'Light']

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
//////////////////////////////////////////////

// Save the server configuration
const saveServerConfig = async (host: string, ident: string, theme: string) => {
  console.log(`server_address: ${host}, ident: ${ident}, theme: ${theme}`)

  try {
    // Update the configuration with the new card number in the dynamic cache
    const response = await invoke('update_server', {
      host: host,
      ident: ident,
      theme: theme,
    })

    console.log('Response from update_server:', response)

    Notify.create({
      message: 'Server configuration has been updated.',
      color: 'green',
      position: 'bottom',
      timeout: 3000,
    })

    // Launch a manual refresh of server connections.
    // await invoke('manual_sync_cards', { readername: "", restart: true })  // restart CARDS connections
    await invoke('manual_sync_cards', {
      readername: "",
      restart: true,
    })
    console.log('Server configuration updated successfully_1')
    await invoke('app_connection') // restart APP connection
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

defineOptions({
  name: 'MainLayout',
})

defineComponent({
  setup() {
    return {
      saveServerConfig, // Save the server configuration
    }
  },
})

listen('global-config-server', (event) => {
  // Global configuration event
  const payload = event.payload as {
    host: string
    ident: string
    dark_theme: string
  }
  console.log('host:', payload.host, 'ident:', payload.ident, 'dark_theme:', payload.dark_theme)

  host.value = payload.host
  identInput.value = payload.ident

  // update the theme value in the application
  changeTheme(payload.dark_theme)
  selectedTheme.value = payload.dark_theme
}).catch((error) => {
  console.error('Error listening to global-config-server:', error)
})

listen('global-notification', (event) => {
  // Global configuration event
  const payload = event.payload as {
    notification_type: string
    message: string
  }

  console.log('global-notification:', payload.notification_type, 'message:', payload.message)

  if (payload.notification_type === 'access') {
    Notify.create({
      message:
        "The application cannot access the directory '~/Documents/tba' and cannot continue to operate. Perhaps such a directory has already been created by another version of the program, therefore it has local access restrictions. A possible solution may be: rename the current directory, for example, to tba1 and restart the application. It will create a new directory with the necessary access rights.",
      color: 'red',
      position: 'bottom',
      timeout: 999000,
    })
  } else if (payload.notification_type === 'version') {
    Notify.create({
      message:
        "The application cannot access the directory '~/Documents/tba' and cannot continue to operate. Perhaps such a directory has already been created by another version of the program, therefore it has local access restrictions. A possible solution may be: rename the current directory, for example, to tba1 and restart the application. It will create a new directory with the necessary access rights.",
      color: 'green',
      position: 'bottom',
      timeout: 15000,
      classes: 'animate__animated animate__shakeX',
    })
  } else {
    console.log('global-notification: unknown type:', payload.notification_type)
  }
}).catch((error) => {
  console.error('Error listening to global-notification:', error)
})

// Generate an event to inform the back-end that the front-end is loaded.
// To correctly display states in the application.
emit('frontend-loaded', { message: 'Hello from frontend!' }).catch((error) => {
  console.error('Error emitting frontend-loaded event:', error)
})
</script>
