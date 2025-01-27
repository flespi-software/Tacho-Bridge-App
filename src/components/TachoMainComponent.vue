<template>
  <div>
    <q-list bordered class="rounded-borders" style="width: 600px; padding-bottom: 10px">
      <div class="header-flex-container">
        <q-item-label header>Cards are connected:</q-item-label>
      </div>
      <q-toolbar inset v-for="(reader, index) in state.readers" :key="index">
        <q-item-section avatar top>
          <q-icon v-bind="cardConnectedStatus(reader)" />
        </q-item-section>

        <q-item-section top>
          <q-item-label caption lines="1">
            <span>{{ reader.name }}</span>
          </q-item-label>
          <q-item-label lines="1" v-if="!reader.cardNumber">
            <span>ATR: {{ reader.cardATR }}</span>
          </q-item-label>
          <q-item-label lines="1" v-if="reader.cardNumber">
            <span class="text-weight-medium">CN: {{ reader.cardNumber }}</span>
          </q-item-label>
        </q-item-section>
        <!-- Button to update current connected Company Card -->
        <q-item-section top side>
          <div class="text-grey-8 q-gutter-xs">
            <q-btn
              :class="['q-mr-lg']"
              size="12px"
              flat
              dense
              round
              icon="add"
              @click="editCompanyCardNumberDialog(reader.cardATR)"
            />
            <!-- Dialog window for the entering the Card Number value -->
            <q-dialog v-model="EnterCardNumberDialog" class="card-number-dialog">
              <q-card>
                <q-card-section>
                  <q-input v-model="cardNumberInput" label="Enter the company card number" />
                </q-card-section>

                <q-card-actions align="right">
                  <q-btn flat label="Cancel" color="primary" v-close-popup />
                  <q-btn
                    flat
                    label="Save"
                    color="primary"
                    @click="saveCardNumber(currentcardATR)"
                  />
                </q-card-actions>
              </q-card>
            </q-dialog>
          </div>
        </q-item-section>
      </q-toolbar>
    </q-list>
  </div>
</template>

<style scoped>
.blinking-icon {
  animation: blink 1300ms infinite;
}

@keyframes blink {
  0% {
    opacity: 1;
  }
  50% {
    opacity: 0.37;
  }
  100% {
    opacity: 1;
  }
}
.toolbar-block {
  margin-bottom: 8px; /* Пример отступа */
}
.custom-font-size-reader {
  font-size: 10px;
}
.header-flex-container {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding-right: 16px;
}
.card-number-dialog .q-card {
  width: 300px; /* Window width */
  max-width: 90vw; /* Maximum window width */
  height: 150px; /* Window height */
  max-height: 90vh; /* Maximum window height */
}
</style>

<script setup lang="ts">
import { ref, reactive, defineComponent } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

// Blinking status for the card icon during authentication.
const isBlinking = ref(true) // controls the blinking status of the icon

// structure of the reader object
interface Reader {
  name: string
  status: string
  cardATR: string
  cardNumber: string
  online?: boolean | undefined
  authentication?: boolean | undefined
}

// reactive state for the readers
const state = reactive({
  readers: [] as Reader[],
})

////////////////////////// Listening for the event from the backend //////////////////////////
// This is an event listener that will listen for the backend to send an event
listen('global-cards-sync', (event) => {
  console.log('event payload: ', event.payload) // log event payload from backend to the console
  // structure has fields from the Rust back-end with the 'snake_case' naming convention
  const payload = event.payload as {
    atr: string
    reader_name: string
    card_state: string
    card_number: string
    online?: boolean
    authentication?: boolean
  }

  const name = payload.reader_name
  const cardNumber = payload.card_number
  // Split the status by the pipe character and get the second element

  const splitted = payload.card_state?.split('|') ?? ['']
  const status = splitted[1]?.trim() ?? splitted[0] ?? ''
  // const status = payload.card_state.includes('|')
  //   ? payload.card_state.split('|')[1].trim()
  //   : payload.card_state

  const cardATR = payload.atr
  // Find the index of the reader with the same name
  const index = state.readers.findIndex((reader) => reader.name === name)
  if (index !== -1) {
    // If reader with the same name is found, update the status and card data
    const existingReader = state.readers[index]
    if (!existingReader) return // на всякий случай

    state.readers[index] = {
      name,
      status,
      cardATR,
      cardNumber,
      online: payload.online !== null ? payload.online : existingReader.online,
      authentication:
        payload.authentication !== null ? payload.authentication : existingReader.authentication,
    }
  } else {
    // If reader with the same name is not found, add the reader to the list
    state.readers.push({
      name,
      status,
      cardATR,
      cardNumber,
      online: payload.online,
      authentication: payload.authentication,
    })
  }
}).catch((error) => {
  console.error('Error listening to global-cards-sync:', error)
})

///////////////////////////// Dialog window for entering the Card Number value /////////////////////////////
const EnterCardNumberDialog = ref(false)
const cardNumberInput = ref('') // Init cardNumber
let currentcardATR = '' // Variable to hold the current card data

// Open the dialog window for entering the Card Number value
const editCompanyCardNumberDialog = (cardATR: string) => {
  currentcardATR = cardATR // Set the current card data
  EnterCardNumberDialog.value = true // Open the dialog window
}

const saveCardNumber = async (cardATR: string) => {
  // Find the index of the reader with the same cardATR
  const readerIndex = state.readers.findIndex((reader) => reader.cardATR === cardATR)

  if (readerIndex === -1) {
    console.error('Reader not found')
    return
  }

  // Save the card number to the currentReader object
  console.log(
    `Card Number: ${cardNumberInput.value}, Card Data: ${cardATR}`,
    `typeof currentcardATR.value: ${typeof cardATR}`,
    `typeof cardNumberInput.value: ${typeof cardNumberInput.value}`,
  )
  EnterCardNumberDialog.value = false // Close the dialog window
  // update the configuration with the new card number in the dynamic cache
  const update_result = await invoke('update_card', {
    atr: cardATR,
    cardnumber: cardNumberInput.value,
  })

  if (state.readers[readerIndex]) {
    state.readers[readerIndex].cardNumber = cardNumberInput.value
  } else {
    console.error(`Reader at index ${readerIndex} does not exist`)
  }

  console.log('update_result:', update_result)

  // Launch a manual refresh of server connections.
  await invoke('manual_sync_cards', {})
}

// Function to change the color of the icon depending on the card status
const cardConnectedStatus = (reader: Reader) => {
  if (reader.cardATR && reader.online) {
    // If the card is connected and online

    if (reader.authentication) {
      // If the card is in the authentication process
      return {
        name: 'credit_card',
        color: 'green',
        size: '25px',
        class: isBlinking.value ? 'blinking-icon' : '', // blinking status
      }
    } else {
      // If the card is not in the authentication process
      return {
        name: 'credit_card',
        color: 'green',
        size: '25px',
      }
    }
  } else if (reader.cardATR) {
    // If the card is connected to the app but not online
    return {
      name: 'credit_card',
      color: 'grey',
      size: '25px',
    }
  } else {
    // If the card is disconnected
    return {
      name: 'credit_card_off',
      color: 'grey',
      size: '25px',
    }
  }
}

defineComponent({
  setup() {
    return {
      EnterCardNumberDialog,
      editCompanyCardNumberDialog,
      saveCardNumber,
    }
  },
})
</script>
