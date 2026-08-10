// Display formatters for CardConfig fields populated by the APDU sniffer.
// Used by SmartCardList.vue and TachoMainComponent.vue.

import type { SmartCard } from './models'

/// Builds the meta string shown in parentheses after the card number,
/// e.g. "Gen1 | Company Card". Omits missing parts.
export function formatCardMeta(card: Pick<SmartCard, 'structure_version' | 'card_type'>): string {
  const parts: string[] = []
  if (card.structure_version) parts.push(formatStructureVersion(card.structure_version))
  if (card.card_type != null) parts.push(formatCardType(card.card_type))
  return parts.join(' | ')
}

export function formatCardType(type: number | null | undefined): string {
  switch (type) {
    case 1:
      return 'Driver Card'
    case 2:
      return 'Workshop Card'
    case 3:
      return 'Control Card'
    case 4:
      return 'Company Card'
    default:
      return `Unknown (${type})`
  }
}

export function formatStructureVersion(v: [number, number] | null | undefined): string {
  if (!v) return ''
  const [major, minor] = v
  if (major === 0 && minor === 0) return 'Gen1'
  if (major === 1 && minor === 0) return 'Gen2 v1'
  if (major === 1 && minor === 1) return 'Gen2 v2'
  return `v${major}.${minor}`
}

export function formatExpire(unixTs: number | null | undefined): string {
  if (!unixTs) return ''
  return new Date(unixTs * 1000).toISOString().slice(0, 10)
}

export function isExpired(unixTs: number | null | undefined): boolean {
  if (!unixTs) return false
  return unixTs * 1000 < Date.now()
}

/** Icon spec consumed by `<q-icon v-bind="...">`. */
export interface CardStatusIcon {
  name: string
  color: string
  size: string
  class?: string
}

/** What the UI needs to know about a card to pick its status icon. */
export interface CardStatusState {
  /** A card is physically present (in a reader slot, or in a rack slot). */
  present: boolean
  /** The card is linked to a configured card number. */
  linked: boolean
  // Both are tri-state on the wire: absent means "not known yet" (a card the
  // rack reports but TBA does not serve), which reads the same as false here.
  /** Its MQTT session is up. */
  online?: boolean | null | undefined
  /** An APDU exchange is running right now. */
  authentication?: boolean | null | undefined
}

/**
 * The shared status-icon vocabulary for a card, used by both the readers list
 * and the rack list so the two speak the same visual language:
 *
 *   blinking green — an APDU exchange is in progress
 *   solid green    — session up, idle
 *   grey outline   — present and configured, but no session
 *   orange plus    — present but not linked to a card number
 *   grey off       — nothing in the slot
 */
export function cardStatusIcon(state: CardStatusState, size = '25px'): CardStatusIcon {
  if (!state.present) {
    return { name: 'mdi-smart-card-off-outline', color: 'grey', size }
  }
  if (!state.linked) {
    return { name: 'mdi-card-plus-outline', color: 'orange', size }
  }
  if (!state.online) {
    return { name: 'mdi-smart-card-outline', color: 'grey', size }
  }
  return state.authentication
    ? { name: 'mdi-smart-card', color: 'green', size, class: 'blinking-icon' }
    : { name: 'mdi-smart-card', color: 'green', size }
}

/// Formats last_auth timestamp (stored as UTC unix seconds) in the user's local
/// timezone as "YYYY-MM-DD HH:MM:SS". Storage stays UTC — only display is localised.
export function formatAuthDate(unixTs: number | null | undefined): string {
  if (!unixTs) return ''
  const d = new Date(unixTs * 1000)
  const pad = (n: number) => String(n).padStart(2, '0')
  return (
    `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ` +
    `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
  )
}
