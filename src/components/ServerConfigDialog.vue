<template>
  <q-dialog :model-value="modelValue" persistent @update:model-value="$emit('update:modelValue', $event)">
    <q-card style="width: 560px; max-width: 95vw">
      <q-card-section class="row items-center q-pb-sm">
        <q-icon name="mdi-cog" size="28px" color="primary" class="q-mr-sm" />
        <div class="text-h6">Settings</div>
        <q-space />
        <q-btn flat round dense icon="mdi-close" v-close-popup />
      </q-card-section>

      <q-separator />

      <q-card-section class="q-pt-md q-pb-sm">
        <div class="text-subtitle2 text-grey-7 q-mb-sm">Server</div>
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

      <q-separator inset />

      <q-card-section class="q-py-sm">
        <div class="text-subtitle2 text-grey-7 q-mb-sm">Appearance</div>
        <q-btn-toggle
          v-model="themeMode"
          dense
          unelevated
          toggle-color="primary"
          :options="[
            { label: 'Auto', value: 'Auto', icon: 'mdi-brightness-auto' },
            { label: 'Light', value: 'Light', icon: 'mdi-white-balance-sunny' },
            { label: 'Dark', value: 'Dark', icon: 'mdi-weather-night' },
          ]"
        />
      </q-card-section>

      <q-separator inset />

      <q-card-section class="q-py-sm">
        <div class="text-subtitle2 text-grey-7 q-mb-sm">Updates</div>
        <q-toggle
          v-model="betaUpdates"
          label="Receive pre-release updates (alpha/beta)"
          dense
          class="q-mb-sm"
        />
        <div class="row q-gutter-sm">
          <q-btn
            outline
            dense
            no-caps
            color="primary"
            icon="mdi-update"
            label="Check for updates"
            :loading="checking"
            @click="checkForUpdates"
          />
          <q-btn
            outline
            dense
            no-caps
            color="grey-7"
            icon="mdi-text-box-outline"
            label="Changelog"
            @click="openChangelog"
          />
        </div>
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

  <q-dialog v-model="changelogOpen">
    <!-- A dialog cannot outgrow the app window (it is part of the webview
         page), so it takes nearly the whole window instead. -->
    <q-card class="column no-wrap" style="width: 95vw; max-width: 95vw; height: 90vh">
      <q-card-section class="row items-center q-pb-sm col-auto">
        <q-icon name="mdi-text-box-outline" size="24px" color="primary" class="q-mr-sm" />
        <div class="text-h6">Changelog</div>
        <q-space />
        <q-btn flat round dense icon="mdi-close" v-close-popup />
      </q-card-section>
      <q-separator />
      <q-card-section class="col q-pt-sm" style="overflow-y: auto">
        <div v-for="section in changelogSections" :key="section.title" class="q-mb-md">
          <div class="text-subtitle2 text-primary">{{ section.title }}</div>
          <!-- overflow-wrap: long unbreakable strings (URLs in the changelog)
               must wrap instead of stretching the card sideways. Group
               headers (🛠 Fixes / 🆕 Features) are set off in bold. -->
          <div
            v-for="(line, i) in section.lines"
            :key="i"
            :class="/^[🛠🆕]/u.test(line) ? 'text-body2 text-weight-bold q-mt-sm' : 'text-body2 q-ml-sm'"
            style="overflow-wrap: anywhere"
          >
            {{ line }}
          </div>
        </div>
      </q-card-section>
    </q-card>
  </q-dialog>
</template>

<script setup lang="ts">
import { useQuasar, Notify } from 'quasar'
import { ref, computed, watch, onMounted, onUnmounted } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'

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
    // Strip the fixed prefix even when partially deleted, then keep digits only —
    // an edit that clips "TBA" must not wipe the digits the user already typed.
    ident.value = val
      .replace(/^T?B?A?/i, '')
      .replace(/\D/g, '')
      .slice(0, 13)
  },
})
const isIdentValid = computed(() => TBA_IDENT_REGEXP.test(identInput.value))

const hostValue = ref('')
// Update channel: off = stable releases only (default), on = pre-releases too.
const betaUpdates = ref(false)

const $q = useQuasar()

// Theme selector. Applies immediately (like the old header button did) and is
// also persisted right away, so it does not depend on pressing Save.
const themeMode = ref<'Auto' | 'Light' | 'Dark'>('Auto')
let syncingTheme = false
watch(themeMode, (mode) => {
  if (syncingTheme) return
  if (mode === 'Auto') $q.dark.set('auto')
  else $q.dark.set(mode === 'Dark')
  invoke('update_theme', { theme: mode }).catch((error) => {
    console.error('Failed to persist theme:', error)
  })
})

function currentThemeLabel(): string {
  if ($q.dark.mode === 'auto') return 'Auto'
  return $q.dark.isActive ? 'Dark' : 'Light'
}

// Forced update check. An available update raises the standard `update`
// notification from the backend; here we only need to voice "up to date".
const checking = ref(false)
const checkForUpdates = async () => {
  checking.value = true
  try {
    const result = await invoke<{ status: string; version: string }>('check_updates_now')
    if (result.status === 'up_to_date') {
      Notify.create({
        message: `You are running the latest version (${result.version}).`,
        color: 'green',
        position: 'bottom',
        timeout: 5000,
      })
    }
  } catch (error) {
    console.error('Update check failed:', error)
    Notify.create({
      message: `Update check failed: ${String(error)}`,
      color: 'red',
      position: 'bottom',
      timeout: 5000,
    })
  } finally {
    checking.value = false
  }
}

// Changelog viewer: the file is bundled into the binary at build time.
const changelogOpen = ref(false)
const changelogSections = ref<{ title: string; lines: string[] }[]>([])
const openChangelog = async () => {
  try {
    const text = await invoke<string>('get_changelog')
    const sections: { title: string; lines: string[] }[] = []
    for (const rawLine of text.split('\n')) {
      const line = rawLine.trimEnd()
      if (line.startsWith('### ')) {
        sections.push({ title: line.slice(4), lines: [] })
      } else if (sections.length > 0 && line.trim().length > 0) {
        sections[sections.length - 1]!.lines.push(line)
      }
    }
    // The file is oldest-first; the dialog shows the newest release on top.
    changelogSections.value = sections.reverse()
    changelogOpen.value = true
  } catch (error) {
    console.error('Failed to load changelog:', error)
  }
}

const saveServerConfig = async () => {
  const theme = currentThemeLabel()
  console.log(`server_address: ${hostValue.value}, ident: ${identInput.value}, theme: ${theme}`)

  try {
    const response = await invoke('update_server', {
      host: hostValue.value,
      ident: identInput.value,
      theme,
      betaUpdates: betaUpdates.value,
    })

    console.log('Response from update_server:', response)

    Notify.create({
      message: 'Settings have been updated.',
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
      message: 'Failed to update settings.',
      color: 'red',
      position: 'bottom',
      timeout: 3000,
    })
  }
}

// Registered Tauri listeners. Stored so we can detach them on unmount —
// a listener registered at setup top-level would outlive the component and
// keep mutating dead state, piling up across remounts and hot reloads.
const unlistenFns: UnlistenFn[] = []

onMounted(async () => {
  try {
    const unlisten = await listen('global-config-server', (event) => {
      const payload = event.payload as {
        host: string
        ident: string
        dark_theme?: string
        beta_updates?: string
      }
      hostValue.value = payload.host
      identInput.value = payload.ident
      betaUpdates.value = payload.beta_updates === 'true'
      if (payload.dark_theme === 'Auto' || payload.dark_theme === 'Light' || payload.dark_theme === 'Dark') {
        // Reflect the persisted mode without re-persisting it.
        syncingTheme = true
        themeMode.value = payload.dark_theme
        void Promise.resolve().then(() => (syncingTheme = false))
      }
    })
    unlistenFns.push(unlisten)
  } catch (error) {
    console.error('Error listening to global-config-server:', error)
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
