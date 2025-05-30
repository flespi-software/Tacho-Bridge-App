<template>
  <div class="q-pt-md">
    <q-card flat bordered>
      <q-expansion-item v-model="isExpanded">
        <template v-slot:header>
          <q-item-section avatar>
            <q-icon name="mdi-cards" />
          </q-item-section>

          <q-item-section>Smart cards ({{ Object.keys(props.cards).length }})</q-item-section>

          <q-item-section>
            <div>
              <q-btn
                label="Add Card"
                dense
                icon="mdi-card-plus"
                flat
                @click.stop="openAddDialog()"
              />
            </div>
          </q-item-section>
        </template>
        <q-separator />
        <q-list separator>
          <q-item
            dense
            v-for="(card, number) in props.cards"
            :key="number"
            @click="cardClick(number)"
            clickable
          >
            <q-item-section avatar>
              <q-icon name="mdi-link" color="grey" v-if="isLinkMode" />
              <q-icon name="mdi-smart-card" color="grey" v-else />
            </q-item-section>
            <q-item-section>
              <q-item-label class="overflow-hidden ellipsis">
                {{ card.name }}
              </q-item-label>
              <q-item-label caption class="overflow-hidden ellipsis">
                {{ number }}
              </q-item-label>
            </q-item-section>

            <q-item-section>
              <q-item-label caption class="overflow-hidden ellipsis">
                <small>{{ card.ICCID }}</small>
              </q-item-label>
            </q-item-section>

            <!-- <q-item-section side>
              <q-btn
                dense
                flat
                icon="edit"
                color="primary"
                round
                @click.stop="openEditDialog(number)"
              />
            </q-item-section> -->
            <q-item-section side>
              <q-btn dense flat icon="delete" color="red" round @click.stop="removeCard(number)" />
            </q-item-section>
          </q-item>
        </q-list>
      </q-expansion-item>
    </q-card>

    <!-- Add/Edit Dialog -->
    <q-dialog v-model="isDialogOpen">
      <q-card style="min-width: 400px">
        <q-card-section>
          <div class="text-h6">{{ isEditMode ? 'Edit Card' : 'Add Card' }}</div>
        </q-card-section>

        <q-card-section class="q-py-none">
          <q-input v-model="dialogCardICCID" label="ICCID" outlined dense disable />
          <q-input
            v-model="dialogCardNumber"
            label="Card Number"
            outlined
            dense
            :disable="isEditMode"
            :error="!!cardNumberError"
            :error-message="cardNumberError"
          />
          <q-input v-model="dialogCardName" label="Card Name" outlined dense class="q-mt-xs" />
        </q-card-section>

        <q-card-actions align="right">
          <q-btn flat label="Cancel" color="primary" v-close-popup />
          <q-btn flat label="Save" color="primary" @click="saveCard" />
        </q-card-actions>
      </q-card>
    </q-dialog>
  </div>
</template>

<script lang="ts" setup>
import { ref, watch, defineComponent, defineProps, defineExpose } from 'vue'
import type { SmartCard } from './models'

/** Company smart card regex: 16 alphanumeric uppercase characters */
const TACHO_COMPANY_CARD_REGEXP = /^[A-Z0-9]{16}$/

type SmartCardMap = Record<string, SmartCard>

// Props
const props = defineProps<{
  cards: SmartCardMap
}>()

// Emits
const emit = defineEmits<{
  (e: 'add-card', number: string, data: SmartCard): void
  (e: 'update-card', number: string, data: SmartCard): void
  (e: 'delete-card', number: string): void
}>()

const isDialogOpen = ref<boolean>(false)
const isEditMode = ref<boolean>(false)

const isExpanded = ref<boolean>(false)
const isLinkMode = ref<boolean>(false)
const linkICCID = ref<string>('')

const dialogCardNumber = ref<string>('')
const dialogCardName = ref<string>('')
const dialogCardICCID = ref<string>('')
const cardNumberError = ref<string>('')

// Watcher for Validation
watch(dialogCardNumber, () => {
  validateCardNumber()
})

// Methods
function openAddDialog(): void {
  isEditMode.value = false
  dialogCardNumber.value = ''
  dialogCardName.value = ''
  dialogCardICCID.value = linkICCID.value || ''
  cardNumberError.value = ''
  isDialogOpen.value = true
}

function linkMode(iccid: string) {
  isExpanded.value = true
  isLinkMode.value = true
  linkICCID.value = iccid || ''
}

function cardClick(number: string) {
  if (isLinkMode.value) {
    const cardData: SmartCard = { ...props.cards[number], ICCID: linkICCID.value }
    emit('update-card', number, cardData)
    isLinkMode.value = false
  } else {
    openEditDialog(number)
  }
}

function openEditDialog(number: string): void {
  isEditMode.value = true
  dialogCardNumber.value = number
  dialogCardName.value = props.cards[number]?.name ?? ''
  dialogCardICCID.value = props.cards[number]?.ICCID ?? ''
  cardNumberError.value = ''
  isDialogOpen.value = true
}

function validateCardNumber(): boolean {
  const number = dialogCardNumber.value.trim().toUpperCase()

  if (!TACHO_COMPANY_CARD_REGEXP.test(number)) {
    cardNumberError.value = 'Card number must be 16 characters (A-Z, 0-9 only)'
    return false
  }

  if (!isEditMode.value && number in props.cards) {
    cardNumberError.value = 'Card number already exists'
    return false
  }

  cardNumberError.value = ''
  return true
}

// function saveCard(): void {
//   const number = dialogCardNumber.value.trim().toUpperCase()
//   const name = dialogCardName.value.trim()

//   if (!validateCardNumber()) {
//     return
//   }

//   cards[number] = { ...cards[number], name }
//   isDialogOpen.value = false
// }

// function removeCard(number: string): void {
//   delete cards[number]
// }

// Save logic
function saveCard(): void {
  const number = dialogCardNumber.value.trim().toUpperCase()
  const name = dialogCardName.value.trim()

  if (!validateCardNumber()) return

  const cardData: SmartCard = { ...props.cards[number], name, ICCID: dialogCardICCID.value || '' }

  if (isEditMode.value) {
    emit('update-card', number, cardData)
  } else {
    emit('add-card', number, cardData)
  }
  isDialogOpen.value = false
  isLinkMode.value = false
  linkICCID.value = ''
}

// Delete
function removeCard(number: string): void {
  emit('delete-card', number)
}

defineComponent({
  setup() {
    return {
      openAddDialog,
      openEditDialog,
      validateCardNumber,
      saveCard,
      linkMode,
    }
  },
})
defineExpose({
  linkMode,
  openAddDialog,
})
</script>
