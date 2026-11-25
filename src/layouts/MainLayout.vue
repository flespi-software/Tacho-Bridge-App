<template>
  <q-layout view="lHh Lpr lFf">
    <q-header class="app-header">
      <q-toolbar class="app-toolbar">
        <div class="app-brand">
          <q-icon name="mdi-steering" size="22px" class="app-brand__icon" />
          <span>Tacho Bridge</span>
          <span class="app-brand__version">v{{ appVersion }}</span>
        </div>
        <q-space />

        <q-btn
          v-if="!appConnected && serverConfigured"
          flat
          round
          icon="mdi-refresh"
          color="warning"
          size="sm"
          aria-label="Reconnect"
          @click="reconnect"
        >
          <q-tooltip>Reconnect</q-tooltip>
        </q-btn>
        <span
          class="app-connection"
          :class="{ 'app-connection--offline': !appConnected }"
          role="status"
        >
          <q-icon
            :name="appConnected ? 'mdi-server-network' : 'mdi-server-network-off'"
            size="16px"
          />
          <span>{{ appConnected ? 'Connected' : 'Disconnected' }}</span>
        </span>
        <q-btn
          flat
          no-caps
          dense
          icon="mdi-cog-outline"
          label="Settings"
          class="app-settings-button"
          @click="configOpen = true"
        />
        <ServerConfigDialog v-model="configOpen" :config="serverConfig" />
        <CredentialsPromptDialog v-model="credentialsPromptOpen" />
        <CardsSyncDialog :conflict="cardsSyncConflict" />
      </q-toolbar>
    </q-header>

    <q-page-container>
      <router-view />
    </q-page-container>
    <ShutdownDialog :state="shutdownState" />
  </q-layout>
</template>

<script setup lang="ts">
import { useQuasar, Notify } from 'quasar'
import { ref, shallowRef, computed, onMounted, provide } from 'vue'
import { useTauriListeners, LAYOUT_LISTENERS_READY } from 'src/composables/useTauriListeners'
import {
  TBA_IDENT_REGEXP,
  isCardsSyncConflict,
  isGlobalConfigServerPayload,
  parseThemeMode,
  type CardsSyncConflict,
  type GlobalConfigServerPayload,
} from 'src/components/models'
import { invoke } from '@tauri-apps/api/core'
import 'animate.css'
import ServerConfigDialog from 'src/components/ServerConfigDialog.vue'
import CredentialsPromptDialog from 'src/components/CredentialsPromptDialog.vue'
import CardsSyncDialog from 'src/components/CardsSyncDialog.vue'
import ShutdownDialog from 'src/components/ShutdownDialog.vue'
import { isShutdownState, type ShutdownState } from 'src/components/shutdownProgress'
import { version as appVersion } from '../../package.json'

defineOptions({
  name: 'MainLayout',
})

const $q = useQuasar()
const configOpen = ref(false)
// The card list conflict the backend waits a choice for; null = none.
const cardsSyncConflict = shallowRef<CardsSyncConflict | null>(null)
const shutdownState = shallowRef<ShutdownState | null>(null)
let shutdownEventSeen = false
function handleShutdown(raw: unknown): void {
  if (raw !== null && !isShutdownState(raw)) return
  shutdownState.value = raw
  if (raw) {
    configOpen.value = false
    credentialsPromptOpen.value = false
  }
}
// Startup sign-in: shown once per webview load when authentication is on but
// no credentials are in effect (they were entered for a previous run only).
const credentialsPromptOpen = ref(false)
let credentialsPromptShown = false
const appConnected = ref(false)
const serverHost = ref('')
const serverIdent = ref('')
// The last `global-config-server` payload, handed to the settings dialog. The
// dialog does not listen on its own: this listener is part of the ready
// promise below, so the payload replayed on `frontend-loaded` always lands.
const serverConfig = shallowRef<GlobalConfigServerPayload | null>(null)
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

/// The update toast. Clicking an action dismisses it, so a failed install
/// brings it back: the backend announces a version only once.
function showUpdateNotification(message: string): void {
  Notify.create({
    message: message || 'Application update is available.',
    color: 'primary',
    position: 'bottom',
    // Stays until an action is clicked: the backend announces a version once,
    // and an unattended machine must still show it when someone gets back.
    // One group, so a newer announcement replaces the old prompt in place.
    timeout: 0,
    group: 'tba-update',
    actions: [
      {
        label: 'Install & restart',
        color: 'white',
        handler: () => {
          void invoke('install_update').catch((e) => {
            // On success the app restarts and this never runs.
            console.error('install_update failed:', e)
            Notify.create({
              message: `Update installation failed: ${String(e)}`,
              color: 'red',
              position: 'bottom',
              timeout: 8000,
            })
            showUpdateNotification(message)
          })
        },
      },
      { label: 'Later', color: 'white' },
    ],
  })
}

// Closes the shown access toast. A grouped repeat replaces the toast in place,
// so the latest create() result always closes the one on screen.
let dismissAccess: (() => void) | null = null

/// Notification kinds the backend can raise, and how each is surfaced.
/// Keyed lookup rather than an if/else ladder so adding a kind is one entry.
const NOTIFICATION_HANDLERS: Record<string, (message: string) => void> = {
  access: () => {
    dismissAccess = Notify.create({
      message: ACCESS_NOTIFICATION_TEXT,
      color: 'red',
      position: 'bottom',
      // Raised once at startup and fatal, so it stays until dismissed; a
      // repeat joins the shown toast instead of stacking a second one.
      timeout: 0,
      group: 'tba-access',
      actions: [{ label: 'Dismiss', color: 'white' }],
    })
  },

  // The backend loaded the config after a failed startup: the access problem
  // is gone, so its toast is too.
  access_restored: () => {
    dismissAccess?.()
    dismissAccess = null
  },

  // Backend message names the new version; install_update downloads,
  // verifies the signature, installs and restarts the app.
  update: showUpdateNotification,

  // Backend message names the COM port occupied by another application.
  // Grouped: every webview reload replays it, and one toast is enough.
  port_busy: (message) =>
    Notify.create({
      message: message || 'The card rack COM port is busy by another application.',
      group: 'tba-port-busy',
      badgeStyle: 'display: none',
      color: 'orange',
      position: 'bottom',
      timeout: 999000,
      actions: [{ label: 'Dismiss', color: 'white' }],
    }),

  // The broker refused the credentials (or requires some): the message names
  // which; the settings dialog is where to fix it.
  auth_rejected: (message) =>
    Notify.create({
      message: message || 'The server rejected the authentication.',
      group: 'tba-auth-rejected',
      badgeStyle: 'display: none',
      color: 'red',
      position: 'bottom',
      timeout: 15000,
      actions: [{ label: 'Settings', color: 'white', handler: () => (configOpen.value = true) }],
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
  if (!isGlobalConfigServerPayload(raw)) {
    console.warn('global-config-server: ignoring malformed payload', raw)
    return
  }
  const payload = raw
  serverConfig.value = payload
  const theme = parseThemeMode(payload.dark_theme)
  if (theme) $q.dark.set(theme === 'Auto' ? 'auto' : theme === 'Dark')
  serverHost.value = payload.host ?? ''
  serverIdent.value = payload.ident ?? ''
  if (
    payload.auth_enabled === 'true' &&
    payload.auth_active !== 'true' &&
    !credentialsPromptShown &&
    !configOpen.value
  ) {
    credentialsPromptShown = true
    credentialsPromptOpen.value = true
  }
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

// Registered from setup, not onMounted: the page inside the layout mounts
// first and emits `frontend-loaded` once these are in place (see
// LAYOUT_LISTENERS_READY), so the replayed config and notifications land.
let statusEventSeen = false
const listenersReady = Promise.all([
  on('global-config-server', handleGlobalConfigServer),
  // The backend connects before the webview loads, so the status event may
  // have fired with no listener: subscribe first, then pull the cached
  // snapshot. An event that arrives in between is fresher than the snapshot
  // and must not be overwritten by it.
  on('app-connection-status', (raw) => {
    statusEventSeen = true
    appConnected.value = raw === true
  }),
  on('global-notification', handleGlobalNotification),
  on('cards-sync-conflict', (raw) => {
    cardsSyncConflict.value = isCardsSyncConflict(raw) ? raw : null
  }),
  on('app-shutdown', (raw) => {
    shutdownEventSeen = true
    handleShutdown(raw)
  }),
]).then(() => undefined)
provide(LAYOUT_LISTENERS_READY, listenersReady)

onMounted(async () => {
  await listenersReady
  void invoke<unknown>('get_shutdown_state')
    .then((raw) => {
      if (!shutdownEventSeen) handleShutdown(raw)
    })
    .catch((error) => console.error('get_shutdown_state failed:', error))
  try {
    const online = await invoke<boolean>('get_app_connection_status')
    if (!statusEventSeen) appConnected.value = online === true
  } catch (error) {
    console.error('get_app_connection_status failed:', error)
  }
})
</script>
