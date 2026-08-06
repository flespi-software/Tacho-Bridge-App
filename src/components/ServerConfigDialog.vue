<template>
  <q-dialog
    :model-value="modelValue"
    persistent
    @update:model-value="$emit('update:modelValue', $event)"
  >
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
          :model-value="themeMode"
          dense
          unelevated
          toggle-color="primary"
          :options="[
            { label: 'Auto', value: 'Auto', icon: 'mdi-brightness-auto' },
            { label: 'Light', value: 'Light', icon: 'mdi-white-balance-sunny' },
            { label: 'Dark', value: 'Dark', icon: 'mdi-weather-night' },
          ]"
          @update:model-value="onThemeSelected"
        />
      </q-card-section>

      <q-separator inset />

      <q-card-section class="q-py-sm">
        <div class="text-subtitle2 text-grey-7 q-mb-sm">System</div>
        <!-- Applies immediately (like the theme), no Save needed: the OS is
             the source of truth, the state is re-read every dialog open. -->
        <q-toggle
          :model-value="autostartEnabled"
          label="Launch at system startup"
          dense
          class="q-mb-sm"
          @update:model-value="onAutostartToggled"
        >
          <q-tooltip anchor="bottom middle" self="top middle" max-width="320px">
            Starts the application automatically when you log in, minimized to the tray. Recommended
            for unattended machines with a card rack.
          </q-tooltip>
        </q-toggle>
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
        <q-toggle
          v-model="autoInstallUpdates"
          label="Automatically install new updates"
          dense
          class="q-mb-sm"
        >
          <q-tooltip anchor="bottom middle" self="top middle" max-width="320px">
            Checks for updates hourly and installs them without asking. The application restarts on
            its own, waiting for a pause in card activity so an authentication in progress is never
            interrupted.
          </q-tooltip>
        </q-toggle>
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
        <!-- No v-close-popup: the dialog closes from saveServerConfig only after
             the settings are confirmed persisted; on failure it stays open so
             the user can see the error and retry. -->
        <q-btn
          unelevated
          rounded
          label="Save"
          color="primary"
          :loading="saving"
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
          <!-- Group headers (🛠 Fixes / 🆕 Features) in bold; entries as a
               bulleted list with a hanging indent so multi-line entries stay
               visually separated. overflow-wrap: long unbreakable strings
               (URLs) must wrap instead of stretching the card sideways. -->
          <template v-for="(item, i) in section.lines" :key="i">
            <div v-if="/^[🛠🆕]/u.test(item.text)" class="text-body2 text-weight-bold q-mt-sm">
              {{ item.text }}
            </div>
            <div v-else-if="item.bullet" class="row no-wrap q-ml-sm q-mb-xs">
              <span class="q-mr-sm">•</span>
              <span class="text-body2" style="overflow-wrap: anywhere">{{ item.text }}</span>
            </div>
            <div v-else class="text-body2 q-ml-sm" style="overflow-wrap: anywhere">
              {{ item.text }}
            </div>
          </template>
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

const props = defineProps<{
  modelValue: boolean
}>()

const emit = defineEmits<{
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
// Unattended updates: the backend checks hourly and installs on its own,
// restarting the app during a pause in card activity.
const autoInstallUpdates = ref(false)

// Launch at login. The OS registration is the source of truth (no config
// field): the state is read on every dialog open and the toggle applies
// immediately, reverting itself when the OS call fails.
const autostartEnabled = ref(false)
async function refreshAutostartState() {
  try {
    autostartEnabled.value = await invoke<boolean>('autostart_get')
  } catch (error) {
    console.error('Failed to read autostart state:', error)
  }
}
async function onAutostartToggled(value: boolean) {
  autostartEnabled.value = value
  try {
    await invoke('autostart_set', { enabled: value })
  } catch (error) {
    autostartEnabled.value = !value
    console.error('Failed to change autostart:', error)
    Notify.create({
      message: `Failed to ${value ? 'enable' : 'disable'} launch at startup: ${String(error)}`,
      color: 'red',
      position: 'bottom',
      timeout: 5000,
    })
  }
}
watch(
  () => props.modelValue,
  (open) => {
    if (open) void refreshAutostartState()
  },
)

const $q = useQuasar()

// Theme selector. Applies and persists immediately on a user's choice (like
// the old header button did). Persistence hangs off the control's update
// event — not a watcher — so seeding the ref from the config on mount cannot
// trigger a redundant re-persist.
type ThemeMode = 'Auto' | 'Light' | 'Dark'
const themeMode = ref<ThemeMode>('Auto')
function onThemeSelected(mode: ThemeMode) {
  themeMode.value = mode
  if (mode === 'Auto') $q.dark.set('auto')
  else $q.dark.set(mode === 'Dark')
  // update_theme resolves with `false` on a persistence failure instead of
  // rejecting — check the value, or a read-only config dir would fail silently.
  invoke<boolean>('update_theme', { theme: mode })
    .then((ok) => {
      if (!ok) throw new Error('the backend could not persist the theme')
    })
    .catch((error) => {
      console.error('Failed to persist theme:', error)
      Notify.create({
        message: `Theme was applied but not saved: ${String(error)}`,
        color: 'red',
        position: 'bottom',
        timeout: 5000,
      })
    })
}

// Forced update check. An available update raises the standard `update`
// notification from the backend; here we only need to voice "up to date".
const checking = ref(false)
const checkForUpdates = async () => {
  checking.value = true
  try {
    // Pass the on-screen channel toggle so the check honors it before Save.
    const result = await invoke<{ status: string; version: string }>('check_updates_now', {
      betaUpdates: betaUpdates.value,
    })
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
    // A channel without a published manifest yet (e.g. the stable channel
    // before the first stable release ships one) is not a scary failure.
    const raw = String(error)
    const noManifest = raw.includes('Could not fetch a valid release JSON')
    Notify.create({
      message: noManifest
        ? 'No update information is published for this channel yet.'
        : `Update check failed: ${raw}`,
      color: noManifest ? 'orange' : 'red',
      position: 'bottom',
      timeout: 5000,
    })
  } finally {
    checking.value = false
  }
}

// Changelog viewer: the file is bundled into the binary at build time.
const changelogOpen = ref(false)
type ChangelogLine = { text: string; bullet: boolean }
const changelogSections = ref<{ title: string; lines: ChangelogLine[] }[]>([])
const openChangelog = async () => {
  try {
    const text = await invoke<string>('get_changelog')
    const sections: { title: string; lines: ChangelogLine[] }[] = []
    for (const rawLine of text.split('\n')) {
      const line = rawLine.trimEnd()
      if (line.startsWith('### ')) {
        sections.push({ title: line.slice(4), lines: [] })
      } else if (sections.length > 0 && line.trim().length > 0) {
        // `- ` is markdown list syntax; the dialog draws its own bullet dot.
        sections[sections.length - 1]!.lines.push({
          text: line.replace(/^-\s+/, ''),
          bullet: /^-\s/.test(line),
        })
      }
    }
    // The file is oldest-first; the dialog shows the newest release on top.
    changelogSections.value = sections.reverse()
    changelogOpen.value = true
  } catch (error) {
    console.error('Failed to load changelog:', error)
    Notify.create({
      message: `Failed to open the changelog: ${String(error)}`,
      color: 'red',
      position: 'bottom',
      timeout: 5000,
    })
  }
}

const saving = ref(false)
const saveServerConfig = async () => {
  const theme = themeMode.value
  console.log(`server_address: ${hostValue.value}, ident: ${identInput.value}, theme: ${theme}`)

  saving.value = true
  try {
    // update_server resolves with `false` on a persistence failure (read-only
    // config dir, disk error) instead of rejecting — a bare await would show
    // the green toast over an unsaved config.
    const ok = await invoke<boolean>('update_server', {
      host: hostValue.value,
      ident: identInput.value,
      theme,
      betaUpdates: betaUpdates.value,
      autoInstallUpdates: autoInstallUpdates.value,
    })
    if (!ok) {
      throw new Error('the backend could not persist the settings')
    }

    Notify.create({
      message: 'Settings have been updated.',
      color: 'green',
      position: 'bottom',
      timeout: 3000,
    })
    emit('update:modelValue', false)
  } catch (error) {
    console.error('Error updating server configuration:', error)
    Notify.create({
      message: `Failed to update settings: ${String(error)}`,
      color: 'red',
      position: 'bottom',
      timeout: 5000,
    })
    return
  } finally {
    saving.value = false
  }

  // Reconnect steps run after a confirmed save, each on its own: a PCSC sync
  // failure (e.g. the smart-card service is down) must not prevent the app
  // connection from moving to the new broker.
  try {
    await invoke('manual_sync_cards', { readername: '', restart: true })
  } catch (error) {
    console.error('Card sync after settings save failed:', error)
    Notify.create({
      message: `Settings saved, but card sync failed: ${String(error)}`,
      color: 'orange',
      position: 'bottom',
      timeout: 5000,
    })
  }
  try {
    await invoke('app_connection')
  } catch (error) {
    console.error('App reconnect after settings save failed:', error)
    Notify.create({
      message: `Settings saved, but reconnect failed: ${String(error)}`,
      color: 'orange',
      position: 'bottom',
      timeout: 5000,
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
        auto_install_updates?: string
      }
      hostValue.value = payload.host
      // Seed the backing ref directly, NOT through the identInput setter: the
      // setter strips dashes/non-digits, so a non-conforming persisted ident
      // would be silently rewritten here and the next Save would persist the
      // mangled identity of a device already registered on the server. The
      // faithful value renders as-is and isIdentValid flags it instead.
      ident.value = payload.ident.replace(/^TBA/i, '')
      betaUpdates.value = payload.beta_updates === 'true'
      autoInstallUpdates.value = payload.auto_install_updates === 'true'
      if (
        payload.dark_theme === 'Auto' ||
        payload.dark_theme === 'Light' ||
        payload.dark_theme === 'Dark'
      ) {
        // Reflect the persisted mode; no watcher, so nothing re-persists.
        themeMode.value = payload.dark_theme
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
