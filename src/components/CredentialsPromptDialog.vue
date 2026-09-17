<template>
  <q-dialog
    :model-value="modelValue"
    persistent
    @update:model-value="$emit('update:modelValue', $event)"
  >
    <q-card style="width: 440px; max-width: 95vw">
      <q-card-section class="row items-center q-pb-sm">
        <q-icon name="mdi-account-key" size="28px" color="primary" class="q-mr-sm" />
        <div class="text-h6">Server sign-in</div>
        <q-space />
        <q-btn flat round dense icon="mdi-close" v-close-popup />
      </q-card-section>

      <q-separator />

      <q-card-section class="q-pt-md q-pb-sm">
        <div class="text-body2 text-grey-7 q-mb-md">
          Server authentication is enabled, but no credentials are stored. Enter them to connect.
        </div>
        <q-input
          label="Username"
          outlined
          dense
          v-model="username"
          autofocus
          autocomplete="off"
          class="q-mb-sm"
          @keyup.enter="connect"
        >
          <template v-slot:prepend>
            <q-icon name="mdi-account-key" size="xs" />
          </template>
        </q-input>
        <q-input
          label="Password"
          outlined
          dense
          v-model="password"
          :type="passwordVisible ? 'text' : 'password'"
          autocomplete="new-password"
          class="q-mb-sm"
          @keyup.enter="connect"
        >
          <template v-slot:prepend>
            <q-icon name="mdi-form-textbox-password" size="xs" />
          </template>
          <template v-slot:append>
            <q-icon
              :name="passwordVisible ? 'mdi-eye-off' : 'mdi-eye'"
              class="cursor-pointer"
              @click="passwordVisible = !passwordVisible"
            />
          </template>
        </q-input>
        <q-checkbox v-model="save" label="Save to config and use for sign-in" dense>
          <q-tooltip anchor="bottom middle" self="top middle" max-width="320px">
            Unchecked: the credentials are kept in memory for this run only and never written to
            config.yaml. The application asks for them again at the next launch.
          </q-tooltip>
        </q-checkbox>
      </q-card-section>

      <q-card-actions align="right" class="q-px-md q-pb-md">
        <q-btn flat label="Skip" color="grey-7" v-close-popup>
          <q-tooltip
            >Stay disconnected; the credentials can be entered in Settings later.</q-tooltip
          >
        </q-btn>
        <q-btn
          unelevated
          rounded
          label="Connect"
          color="primary"
          :loading="connecting"
          :disable="username.length === 0"
          @click="connect"
        />
      </q-card-actions>
    </q-card>
  </q-dialog>
</template>

<script setup lang="ts">
import { ref } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { notifyError, notifySuccess, TOAST_SHORT } from 'src/composables/notify'

defineProps<{
  modelValue: boolean
}>()

const emit = defineEmits<{
  'update:modelValue': [value: boolean]
}>()

const username = ref('')
const password = ref('')
const passwordVisible = ref(false)
// Checked (default): the pair goes to config.yaml. Unchecked: memory only.
const save = ref(true)
const connecting = ref(false)

// Applies the pair and rebuilds every server connection with it. On failure
// the dialog stays open with the error so the user can correct and retry.
async function connect() {
  if (username.value.length === 0) return
  connecting.value = true
  try {
    await invoke('apply_credentials', {
      enabled: true,
      username: username.value,
      password: password.value,
      save: save.value,
      reconnect: true,
    })
    notifySuccess('Connecting with the entered credentials.', TOAST_SHORT)
    password.value = ''
    emit('update:modelValue', false)
  } catch (error) {
    console.error('Failed to apply credentials:', error)
    notifyError('Failed to apply the credentials', error)
  } finally {
    connecting.value = false
  }
}
</script>
