<template>
  <div style="width: 600px; max-width: 100%">
    <div class="rounded-borders" style="border: 1px solid #666">
      <!-- <div class="header-flex-container">
        <q-item-label header>Cards are connected:</q-item-label>
      </div> -->
      <div v-if="state.readers.length === 0" class="q-pa-md text-grey text-h6">
        No connected smart card readers
      </div>
      <div v-for="(reader, index) in state.readers" :key="index" class="row reader">
        <q-item class="col-6" style="min-height: 50px" dense>
          <q-item-section avatar>
            <q-icon name="mdi-usb-port" :color="reader.status !== 'UNKNOWN' ? 'green' : 'red'" />
          </q-item-section>
          <q-item-section>
            <q-item-label caption lines="3" class="text-grey text-bold">
              <small>{{ reader.name }}</small>
            </q-item-label>
          </q-item-section>
        </q-item>
        <q-item class="col-6" style="min-height: 50px" dense v-if="reader.status !== 'UNKNOWN'">
          <q-item-section avatar>
            <q-icon v-bind="cardConnectedStatus(reader)" />
          </q-item-section>

          <q-item-section>
            <template v-if="!reader.cardNumber && reader.cardICCID">
              <q-item-label lines="1">UNKNOWN CARD</q-item-label>
              <q-item-label lines="1" caption>
                <span>ICCID: {{ reader.cardICCID }}</span>
              </q-item-label>
            </template>
            <q-item-label lines="1" v-if="reader.cardNumber">
              <span class="text-weight-medium">{{ reader.cardNumber }}</span>
            </q-item-label>

            <q-item-label lines="1" v-if="!reader.cardICCID && !reader.cardNumber">
              <span class="text-weight-medium text-grey-6">EMPTY SLOT</span>
            </q-item-label>
          </q-item-section>
          <q-item-section side v-if="reader.cardICCID && !reader.cardNumber">
            <div class="text-grey-8 q-gutter-xs">
              <q-btn
                size="12px"
                flat
                dense
                round
                icon="mdi-link"
                @click="linkMode(reader.cardICCID)"
              />
            </div>
          </q-item-section>
        </q-item>
      </div>
    </div>
    <SmartCardList
      ref="cardlist"
      :cards="state.cards"
      @add-card="addCard"
      @update-card="updateCard"
      @delete-card="removeCard"
    />
  </div>
</template>

<style scoped>
.reader {
  border-bottom: 1px solid #666;
}
.reader:last-child {
  border-bottom: 0;
}
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
  margin-bottom: 8px;
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
  height: 160px; /* Window height */
  max-height: 90vh; /* Maximum window height */
}
</style>

<script setup lang="ts">
import SmartCardList from './SmartCardList.vue'
import type { SmartCard, Reader } from './models'
import { ref, reactive, defineComponent } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { listen, emit } from '@tauri-apps/api/event'
// import { useQuasar} from 'quasar'

// import { Notify } from 'quasar'

// Blinking status for the card icon during authentication.
const isBlinking = ref(true) // controls the blinking status of the icon

const cardlist = ref<null | { linkMode: (ICCID: string) => null; openAddDialog: () => null }>(null)

// reactive state for the readers and cards
const state = reactive({
  readers: [] as Reader[],
  cards: {} as Record<string, SmartCard>,
})

////////////////////////// Listening for the event from the backend //////////////////////////
// This is an event listener that will listen for the backend to send an event
listen('global-cards-sync', (event) => {
  console.log('event payload: ', event.payload) // log event payload from backend to the console
  // structure has fields from the Rust back-end with the 'snake_case' naming convention
  const payload = event.payload as {
    iccid: string
    readerName: string
    cardState: string
    cardNumber: string
    online?: boolean
    authentication?: boolean
  }

  const name = payload.readerName
  const cardNumber = payload.cardNumber
  // Split the status by the pipe character and get the second element
  const splitted = (payload.cardState?.match(/\((.*)\)/i) ?? [])[1]?.split('|') ?? []
  const status = splitted[1]?.trim() ?? splitted[0] ?? ''

  const cardICCID = payload.iccid
  // Find the index of the reader with the same name
  const index = state.readers.findIndex((reader) => reader.name === name)
  if (index !== -1) {
    // If reader with the same name is found, update the status and card data
    const existingReader = state.readers[index]
    if (!existingReader) return // на всякий случай

    state.readers[index] = {
      name,
      status,
      cardICCID,
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
      cardICCID,
      cardNumber,
      online: payload.online,
      authentication: payload.authentication,
    })
  }
}).catch((error) => {
  console.error('Error listening to global-cards-sync:', error)
})

///////////////////////////// Dialog window for entering the Card Number value /////////////////////////////

const saveCardNumber = async (cardNumber: string, content: SmartCard) => {
  // Find the index of the reader with the same cardICCID
  const readerIndex = state.readers.findIndex((reader) => reader.cardICCID === content.ICCID)
  if (readerIndex === -1) {
    console.error('Reader not found')
    return
  }

  // Save the card number to the currentReader object
  console.log(`Card Number: ${cardNumber}, Card ICCID: ${content.ICCID}`)

  // update the configuration with the new card number in the dynamic cache
  const update_result = await invoke('update_card', {
    content,
    cardnumber: cardNumber,
  })

  // Update the card number in the state if configuration update was successful
  if (update_result) {
    const reader = state.readers[readerIndex]
    if (reader) {
      reader.cardNumber = cardNumber || ''

      // Запускаем обновление только если reader точно существует
      await invoke('manual_sync_cards', {
        readername: reader.name,
        restart: false,
      })

      console.log('Card number updated successfully')
    } else {
      console.error(`Reader at index ${readerIndex} does not exist`)
    }
  }
}

// Function to change the color of the icon depending on the card status
const cardConnectedStatus = (reader: Reader) => {
  if (reader.cardICCID && reader.online) {
    // If the card is connected and online

    if (reader.authentication) {
      // If the card is in the authentication process
      return {
        name: 'mdi-smart-card',
        color: 'green',
        size: '25px',
        class: isBlinking.value ? 'blinking-icon' : '', // blinking status
      }
    } else {
      // If the card is not in the authentication process
      return {
        name: 'mdi-smart-card',
        color: 'green',
        size: '25px',
      }
    }
  } else if (reader.cardICCID) {
    // If the card is connected to the app but not online
    if (reader.cardNumber) {
      // Known card
      return {
        name: 'mdi-smart-card-outline',
        color: 'grey',
        size: '25px',
      }
    } else {
      // unknown card
      return {
        name: 'mdi-card-plus-outline',
        color: 'orange',
        size: '25px',
      }
    }
  } else {
    // If the card is disconnected
    return {
      name: 'mdi-smart-card-off-outline',
      color: 'grey',
      size: '25px',
    }
  }
}

// SmartCardList handlers
function linkMode(ICCID: string) {
  cardlist.value?.linkMode(ICCID)
  if (Object.keys(state.cards).length === 0) {
    cardlist.value?.openAddDialog()
  }
}
async function addCard(number: string, data: SmartCard) {
  state.cards[number] = data
  await saveCardNumber(number, data)
}

async function updateCard(number: string, data: SmartCard) {
  state.cards[number] = data
  await saveCardNumber(number, data)
}

// remove card func from the config
const removeCard = async (cardNumber: string) => {
  state.readers = state.readers.filter((reader) => reader.cardNumber !== cardNumber)

  try {
    await invoke('remove_card', { cardnumber: cardNumber })
    console.log('Card removed:', cardNumber)
  } catch (error) {
    console.error('Failed to remove card:', error)
  }
}

listen('global-card-config-updated', (event) => {
  console.log('event payload: ', event.payload)
  const payload = event.payload as {
    content: object
    cardNumber: string
  }
  if (payload.cardNumber) {
    if (payload.content) {
      state.cards[payload.cardNumber] = { ...payload.content }
    } else {
      delete state.cards[payload.cardNumber]
    }
  }
}).catch((error) => {
  console.error('Error listening to global-card-config-updated:', error)
})

// Generate an event to inform the back-end that the front-end is loaded.
// To correctly display states in the application.
emit('frontend-loaded', { message: 'Hello from frontend!' }).catch((error) => {
  console.error('Error emitting frontend-loaded event:', error)
})

defineComponent({
  setup() {
    return {
      saveCardNumber,
    }
  },
})
</script>
