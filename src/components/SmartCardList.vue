<template>
  <div class="q-pt-md">
    <!--
      TOP SECTION: Link-mode banner.
      Only visible when the user is binding a newly-detected ICCID (from a physical
      card in a reader) to one of the existing saved cards. Shown above the list so
      it's clear the next click in the list below assigns the ICCID.
      In link mode the list below is filtered to UNLINKED cards only (see cardsList).
    -->
    <q-banner v-if="isLinkMode" dense inline-actions rounded class="text-white bg-blue-8 q-mb-xs">
      Click a card in the list below to assign or click
      <q-btn
        label="Add card"
        dense
        icon="mdi-card-plus"
        color="white"
        flat
        class="q-ml-sm"
        @click.stop="openAddDialog()"
      />
      <template v-slot:action>
        <q-btn flat round color="white" icon="mdi-close" @click="cancelLink" />
      </template>
    </q-banner>

    <!--
      BOTTOM SECTION: Smart cards list (collapsible card).
      Always rendered. In normal mode shows every saved card with full metadata
      (name, number, generation/type, expire, company name/address, ICCID).
      In link mode the list below is filtered to cards that have no ICCID yet.
    -->
    <q-card flat bordered>
      <q-expansion-item v-model="isExpanded">
        <!-- Header of the collapsible card: icon + title with count + Add button -->
        <template v-slot:header>
          <q-item-section avatar>
            <q-icon name="mdi-cards" />
          </q-item-section>

          <q-item-section>Smart cards ({{ Object.keys(cardsList).length }})</q-item-section>

          <q-item-section>
            <div>
              <q-btn
                label="Add Card"
                dense
                icon="mdi-card-plus"
                color="green"
                @click.stop="openAddDialog()"
              />
            </div>
          </q-item-section>
        </template>
        <q-separator />

        <!--
          Card rows. Click behavior depends on isLinkMode:
            - link mode → bind the pending ICCID to the clicked card
            - normal mode → open the Edit dialog for the clicked card
        -->
        <q-list separator>
          <q-item
            dense
            v-for="(card, number) in cardsList"
            :key="number"
            @click="cardClick(number)"
            clickable
          >
            <!-- Avatar icon: "link" during link mode, regular smart-card icon otherwise -->
            <q-item-section avatar>
              <q-icon name="mdi-link" color="grey" v-if="isLinkMode" />
              <q-icon name="mdi-smart-card" color="grey" v-else />
            </q-item-section>

            <!--
              Main info block (left → right text column):
                1. User-given card name
                2. Card number + "(generation | card type)" in grey parens, if known
                3. Expiry date (red+bold if already expired)
                4. Company name with building icon
                5. Company address with map-marker icon
              All auto-populated fields below the number come from the APDU sniffer
              parsing EF_Identification / EF_Application_Identification after auth.
            -->
            <q-item-section>
              <q-item-label class="overflow-hidden ellipsis">
                {{ card.name }}
              </q-item-label>
              <q-item-label caption class="overflow-hidden ellipsis">
                <span>{{ number }}</span>
                <span
                  v-if="card.structure_version || card.card_type != null"
                  class="text-grey-7 q-ml-xs"
                >
                  ({{ formatCardMeta(card) }})
                </span>
              </q-item-label>
              <q-item-label v-if="card.expire" caption class="overflow-hidden ellipsis">
                <span :class="isExpired(card.expire) ? 'text-red text-weight-medium' : ''">
                  Expire: {{ formatExpire(card.expire) }}
                </span>
              </q-item-label>
              <q-item-label v-if="card.company_name" caption class="overflow-hidden ellipsis">
                <q-icon name="mdi-domain" size="xs" class="q-mr-xs" />{{ card.company_name }}
              </q-item-label>
              <q-item-label
                v-if="card.company_address"
                caption
                class="overflow-hidden ellipsis text-grey-7"
              >
                <q-icon name="mdi-map-marker" size="xs" class="q-mr-xs" />{{ card.company_address }}
              </q-item-label>
            </q-item-section>

            <!-- ICCID chip pinned to the right. Hidden if card has no ICCID yet. -->
            <q-item-section side>
              <q-chip
                v-if="card.iccid"
                dense
                size="sm"
                color="grey"
                class="text-dark text-bold q-ma-none"
              >
                ICCID: {{ card.iccid }}
              </q-chip>
            </q-item-section>

            <!-- Rightmost column: remove card from config -->
            <q-item-section side>
              <q-btn dense flat icon="delete" color="red" round @click.stop="removeCard(number)" />
            </q-item-section>
          </q-item>
        </q-list>
      </q-expansion-item>
    </q-card>

    <!--
      Add/Edit card dialog.
      Opens in two modes:
        - Add    → all fields empty (ICCID pre-filled if coming from link flow)
        - Edit   → card number and ICCID locked (they identify the record)
    -->
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
            maxlength="16"
            :disable="isEditMode"
            :error="!!cardNumberError"
            :error-message="cardNumberError"
          />
          <q-input
            v-model="dialogCardName"
            label="Card Name"
            outlined
            dense
            type="textarea"
            autogrow
            class="q-mt-xs"
          />
        </q-card-section>

        <q-card-actions align="right">
          <q-btn flat label="Cancel" color="primary" v-close-popup @click="closeCard" />
          <q-btn flat label="Save" color="primary" @click="saveCard" />
        </q-card-actions>
      </q-card>
    </q-dialog>
  </div>
</template>

<script lang="ts" setup>
import { ref, computed, watch } from 'vue'
import { Dialog } from 'quasar'
import type { SmartCard } from './models'
import { formatCardMeta, formatExpire, isExpired } from './cardFormatters'

/** Company smart card regex: 16 alphanumeric uppercase characters */
const TACHO_COMPANY_CARD_REGEXP = /^[A-Z0-9]{16}$/

type SmartCardMap = Record<string, SmartCard>

// Props
const props = defineProps<{
  cards: SmartCardMap
}>()

// Filtered view of cards rendered in the list:
//   - Normal mode:   every saved card (full metadata from config + sniffer).
//   - Link mode:     only cards WITHOUT an ICCID yet, so the user can bind the
//                    pending ICCID (from the top-section banner flow) to one of them.
const cardsList = computed(() => {
  return Object.keys(props.cards)
    .filter((el) => {
      return !isLinkMode.value || !props.cards[el]?.iccid
    })
    .reduce((obj: SmartCardMap, n) => {
      if (props.cards[n]) {
        obj[n] = props.cards[n]
      }
      return obj
    }, {})
})

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
    const cardData: SmartCard = { ...props.cards[number], iccid: linkICCID.value }
    emit('update-card', number, cardData)
    isLinkMode.value = false
    linkICCID.value = ''
  } else {
    openEditDialog(number)
  }
}

function openEditDialog(number: string): void {
  isEditMode.value = true
  dialogCardNumber.value = number
  dialogCardName.value = props.cards[number]?.name ?? ''
  dialogCardICCID.value = props.cards[number]?.iccid ?? ''
  cardNumberError.value = ''
  isDialogOpen.value = true
}

function validateCardNumber(): boolean {
  const number = dialogCardNumber.value.trim().toUpperCase()

  if (!TACHO_COMPANY_CARD_REGEXP.test(number)) {
    cardNumberError.value = 'Card number must be 16 characters (A-Z, 0-9 only)'
    return false
  }

  // Use hasOwnProperty instead of `in` so prototype-chain names like
  // "constructor" or "__proto__" can't falsely report "already exists".
  if (!isEditMode.value && Object.prototype.hasOwnProperty.call(props.cards, number)) {
    cardNumberError.value = 'Card number already exists'
    return false
  }

  cardNumberError.value = ''
  return true
}

// Save logic
function saveCard(): void {
  const number = dialogCardNumber.value.trim().toUpperCase()
  const name = dialogCardName.value.trim()

  if (!validateCardNumber()) return

  const cardData: SmartCard = { ...props.cards[number], name, iccid: dialogCardICCID.value || '' }

  if (isEditMode.value) {
    emit('update-card', number, cardData)
  } else {
    emit('add-card', number, cardData)
  }
  isDialogOpen.value = false
  cancelLink()
  isExpanded.value = true
}

function cancelLink(): void {
  isLinkMode.value = false
  linkICCID.value = ''
}
function closeCard(): void {
  isDialogOpen.value = false
  cancelLink()
}
// Delete
function removeCard(number: string): void {
  // Confirm before emitting — a misclick used to silently wipe a saved card
  // from the config (autosaved, no undo).
  Dialog.create({
    title: 'Remove card',
    message: `Remove card "${number}" from the configuration? This cannot be undone.`,
    ok: { label: 'Remove', color: 'red', flat: false },
    cancel: { label: 'Cancel', flat: true },
    persistent: true,
  }).onOk(() => {
    emit('delete-card', number)
  })
}


defineExpose({
  linkMode,
  openAddDialog,
})
</script>
