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
import { ref } from 'vue'
import { listen } from '@tauri-apps/api/event'
import 'animate.css'
import ServerConfigDialog from 'src/components/ServerConfigDialog.vue'

defineOptions({
  name: 'MainLayout',
})

const configOpen = ref(false)
const appConnected = ref(false)

listen('app-connection-status', (event) => {
  appConnected.value = event.payload as boolean
}).catch((error) => {
  console.error('Error listening to app-connection-status:', error)
})

listen('global-notification', (event) => {
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
</script>
