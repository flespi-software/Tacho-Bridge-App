<template>
  <q-layout view="lHh Lpr lFf">
    <q-header elevated>
      <q-toolbar>
        <q-icon name="mdi-steering" size="24px" class="q-ml-md q-mr-sm" style="opacity: 0.8" />
        <q-toolbar-title> Tacho Bridge </q-toolbar-title>

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
        <q-icon
          :name="appConnected ? 'mdi-server-network' : 'mdi-server-network-off'"
          :color="appConnected ? 'light-green-13' : 'grey-5'"
          size="20px"
          class="q-mr-xs"
        >
          <q-tooltip>{{ appConnected ? 'Connected' : 'Disconnected' }}</q-tooltip>
        </q-icon>
        <q-btn flat round icon="mdi-cog" color="grey-4" size="sm" @click="configOpen = true">
          <q-tooltip>Settings</q-tooltip>
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
import { ref, computed, onMounted } from 'vue'
import { useTauriListeners } from 'src/composables/useTauriListeners'
import { TBA_IDENT_REGEXP } from 'src/components/models'
import { invoke } from '@tauri-apps/api/core'
import 'animate.css'
import ServerConfigDialog from 'src/components/ServerConfigDialog.vue'

defineOptions({
  name: 'MainLayout',
})

const $q = useQuasar()
const configOpen = ref(false)
const appConnected = ref(false)
const serverHost = ref('')
const serverIdent = ref('')
const serverConfigured = computed(
  () => serverHost.value.length > 0 && TBA_IDENT_REGEXP.test(serverIdent.value),
)

async function reconnect() {
  try {
    await invoke('app_connection')
  } catch (error) {
    console.error('Reconnect failed:', error)
  }
}

const { on } = useTauriListeners()

const ACCESS_NOTIFICATION_TEXT =
  "The application cannot access the directory '~/Documents/tba' and cannot continue to operate. Perhaps such a directory has already been created by another version of the program, therefore it has local access restrictions. A possible solution may be: rename the current directory, for example, to tba1 and restart the application. It will create a new directory with the necessary access rights."

/// Notification kinds the backend can raise, and how each is surfaced.
/// Keyed lookup rather than an if/else ladder so adding a kind is one entry.
const NOTIFICATION_HANDLERS: Record<string, (message: string) => void> = {
  access: () =>
    Notify.create({
      message: ACCESS_NOTIFICATION_TEXT,
      color: 'red',
      position: 'bottom',
      timeout: 999000,
    }),

  // Backend message names the new version; install_update downloads,
  // verifies the signature, installs and restarts the app.
  update: (message) =>
    Notify.create({
      message: message || 'Application update is available.',
      color: 'primary',
      position: 'bottom',
      timeout: 999000,
      actions: [
        {
          label: 'Install & restart',
          color: 'white',
          handler: () => {
            void invoke('install_update').catch((e) => {
              // On success the app restarts and this never runs; a silent
              // catch here meant a failed download looked like nothing
              // happened after the user clicked "Install & restart".
              console.error('install_update failed:', e)
              Notify.create({
                message: `Update installation failed: ${String(e)}. You can retry from the update notification.`,
                color: 'red',
                position: 'bottom',
                timeout: 8000,
              })
            })
          },
        },
        { label: 'Later', color: 'white' },
      ],
    }),

  // Backend message names the COM port occupied by another application.
  port_busy: (message) =>
    Notify.create({
      message: message || 'The card rack COM port is busy by another application.',
      color: 'orange',
      position: 'bottom',
      timeout: 999000,
    }),

  // Use the backend-provided message verbatim — it contains the new
  // version string and the download URL.
  version: (message) =>
    Notify.create({
      message: message || 'A new version is available.',
      color: 'green',
      position: 'bottom',
      timeout: 15000,
      classes: 'animate__animated animate__shakeX',
    }),
}

function handleGlobalConfigServer(raw: unknown): void {
  const payload = raw as { dark_theme?: string; host?: string; ident?: string }
  if (payload.dark_theme === 'Dark') $q.dark.set(true)
  else if (payload.dark_theme === 'Auto') $q.dark.set('auto')
  else if (payload.dark_theme === 'Light') $q.dark.set(false)
  // empty/unknown (old config without an appearance section): keep the current mode
  serverHost.value = payload.host ?? ''
  serverIdent.value = payload.ident ?? ''
}

function handleGlobalNotification(raw: unknown): void {
  if (!raw || typeof raw !== 'object') return
  const payload = raw as { notification_type?: unknown; message?: unknown }
  const type = typeof payload.notification_type === 'string' ? payload.notification_type : ''
  const message = typeof payload.message === 'string' ? payload.message : ''

  console.log('global-notification:', type, 'message:', message)

  const handler = NOTIFICATION_HANDLERS[type]
  if (handler) handler(message)
  else console.log('global-notification: unknown type:', type)
}

onMounted(async () => {
  await on('global-config-server', handleGlobalConfigServer)
  // The backend connects before the webview loads, so the status event may
  // have fired with no listener: subscribe first, then pull the cached
  // snapshot. An event that arrives in between is fresher than the snapshot
  // and must not be overwritten by it.
  let statusEventSeen = false
  await on('app-connection-status', (raw) => {
    statusEventSeen = true
    appConnected.value = raw === true
  })
  await on('global-notification', handleGlobalNotification)
  try {
    const online = await invoke<boolean>('get_app_connection_status')
    if (!statusEventSeen) appConnected.value = online === true
  } catch (error) {
    console.error('get_app_connection_status failed:', error)
  }
})
</script>
