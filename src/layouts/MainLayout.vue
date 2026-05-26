<template>
  <q-layout view="lHh Lpr lFf">
    <q-header elevated>
      <q-toolbar>
        <q-toolbar-title class="q-ml-md">
          Tacho Bridge Application
          <q-icon name="mdi-record-circle-outline" class="q-ml-md" :color="appConnected ? 'green' : undefined" />
        </q-toolbar-title>

        <div class="q-pa-xs q-gutter-sm">
          <q-btn flat round icon="mdi-cog" @click="configOpen = true" />
          <ServerConfigDialog v-model="configOpen" />
        </div>
      </q-toolbar>
    </q-header>

    <q-page-container>
      <router-view />
    </q-page-container>
  </q-layout>
</template>

<script setup lang="ts">
import { Notify } from 'quasar'
import { ref, onMounted, onUnmounted } from 'vue'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import 'animate.css'
import ServerConfigDialog from 'src/components/ServerConfigDialog.vue'

defineOptions({
  name: 'MainLayout',
})

const configOpen = ref(false)
const appConnected = ref(false)

// Registered Tauri listeners. Stored so we can detach them on unmount —
// otherwise stale handlers keep mutating dead reactive state and pile up
// across hot reloads in dev.
const unlistenFns: UnlistenFn[] = []

const ACCESS_NOTIFICATION_TEXT =
  "The application cannot access the directory '~/Documents/tba' and cannot continue to operate. Perhaps such a directory has already been created by another version of the program, therefore it has local access restrictions. A possible solution may be: rename the current directory, for example, to tba1 and restart the application. It will create a new directory with the necessary access rights."

onMounted(async () => {
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
