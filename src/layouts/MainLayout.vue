<template>
  <q-layout view="lHh Lpr lFf">
    <q-header elevated>
      <q-toolbar>
        <q-icon name="mdi-steering" size="24px" class="q-ml-md q-mr-sm" style="opacity: 0.8" />
        <q-toolbar-title>
          Tacho Bridge
        </q-toolbar-title>

        <q-btn flat round :icon="themeIcon" :color="themeColor" size="sm" @click="cycleTheme">
          <q-tooltip>{{ themeLabel }}</q-tooltip>
        </q-btn>
        <q-btn
          v-if="!appConnected && serverConfigured"
          flat
          round
          icon="mdi-refresh"
          color="warning"
          size="sm"
          @click="reconnect"
        >
          <q-tooltip>Reconnect</q-tooltip>
        </q-btn>
        <q-btn
          flat
          round
          :icon="appConnected ? 'mdi-server-network' : 'mdi-server-network-off'"
          :color="appConnected ? 'light-green-13' : 'grey-5'"
          size="sm"
          @click="configOpen = true"
        >
          <q-tooltip>{{ appConnected ? 'Connected' : 'Disconnected' }} — Server configuration</q-tooltip>
        </q-btn>
        <ServerConfigDialog v-model="configOpen" />
      </q-toolbar>
    </q-header>

    <q-page-container>
      <router-view />
    </q-page-container>
  </q-layout>
</template>

<script setup lang="ts">
import { useQuasar, Notify } from 'quasar'
import { ref, computed, onMounted, onUnmounted } from 'vue'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { invoke } from '@tauri-apps/api/core'
import 'animate.css'
import ServerConfigDialog from 'src/components/ServerConfigDialog.vue'

defineOptions({
  name: 'MainLayout',
})

const TBA_IDENT_REGEXP = /^TBA\d{13}$/

const $q = useQuasar()
const configOpen = ref(false)
const appConnected = ref(false)
const serverHost = ref('')
const serverIdent = ref('')
const serverConfigured = computed(() => serverHost.value.length > 0 && TBA_IDENT_REGEXP.test(serverIdent.value))

async function reconnect() {
  try {
    await invoke('app_connection')
  } catch (error) {
    console.error('Reconnect failed:', error)
  }
}

const themeIcon = computed(() => {
  if ($q.dark.mode === 'auto') return 'mdi-brightness-auto'
  return $q.dark.isActive ? 'mdi-weather-night' : 'mdi-white-balance-sunny'
})

const themeLabel = computed(() => {
  if ($q.dark.mode === 'auto') return 'Auto'
  return $q.dark.isActive ? 'Dark' : 'Light'
})

const themeColor = computed(() => {
  if ($q.dark.mode === 'auto') return 'grey-5'
  return $q.dark.isActive ? 'light-blue-3' : 'yellow'
})

function cycleTheme() {
  if ($q.dark.mode === false) {
    $q.dark.set(true)
  } else if ($q.dark.mode === true) {
    $q.dark.set('auto')
  } else {
    $q.dark.set(false)
  }
  // Persist right away: this button is the only theme control, so the choice
  // must survive a restart without a trip through the server dialog's Save.
  invoke('update_theme', { theme: themeLabel.value }).catch((error) => {
    console.error('Failed to persist theme:', error)
  })
}

// Registered Tauri listeners. Stored so we can detach them on unmount —
// otherwise stale handlers keep mutating dead reactive state and pile up
// across hot reloads in dev.
const unlistenFns: UnlistenFn[] = []

const ACCESS_NOTIFICATION_TEXT =
  "The application cannot access the directory '~/Documents/tba' and cannot continue to operate. Perhaps such a directory has already been created by another version of the program, therefore it has local access restrictions. A possible solution may be: rename the current directory, for example, to tba1 and restart the application. It will create a new directory with the necessary access rights."

onMounted(async () => {
  try {
    const unlisten = await listen('global-config-server', (event) => {
      const payload = event.payload as { dark_theme?: string; host?: string; ident?: string }
      if (payload.dark_theme === 'Dark') $q.dark.set(true)
      else if (payload.dark_theme === 'Auto') $q.dark.set('auto')
      else if (payload.dark_theme === 'Light') $q.dark.set(false)
      // empty/unknown (old config without an appearance section): keep the current mode
      serverHost.value = payload.host ?? ''
      serverIdent.value = payload.ident ?? ''
    })
    unlistenFns.push(unlisten)
  } catch (error) {
    console.error('Error listening to global-config-server:', error)
  }

  try {
    const unlisten = await listen('app-connection-status', (event) => {
      appConnected.value = event.payload === true
    })
    unlistenFns.push(unlisten)
  } catch (error) {
    console.error('Error listening to app-connection-status:', error)
  }

  try {
    const unlisten = await listen('global-notification', (event) => {
      const raw = event.payload
      if (!raw || typeof raw !== 'object') return
      const payload = raw as { notification_type?: unknown; message?: unknown }
      const type = typeof payload.notification_type === 'string' ? payload.notification_type : ''
      const message = typeof payload.message === 'string' ? payload.message : ''

      console.log('global-notification:', type, 'message:', message)

      if (type === 'access') {
        Notify.create({
          message: ACCESS_NOTIFICATION_TEXT,
          color: 'red',
          position: 'bottom',
          timeout: 999000,
        })
      } else if (type === 'version') {
        // Use the backend-provided message verbatim — it contains the new
        // version string and the download URL.
        Notify.create({
          message: message || 'A new version is available.',
          color: 'green',
          position: 'bottom',
          timeout: 15000,
          classes: 'animate__animated animate__shakeX',
        })
      } else {
        console.log('global-notification: unknown type:', type)
      }
    })
    unlistenFns.push(unlisten)
  } catch (error) {
    console.error('Error listening to global-notification:', error)
  }
})

onUnmounted(() => {
  while (unlistenFns.length > 0) {
    const fn = unlistenFns.pop()
    try {
      fn?.()
    } catch (e) {
      console.error('Error detaching Tauri listener:', e)
    }
  }
})
</script>
