// Interfaces
export interface SmartCard {
  name?: string
  iccid?: string
  expire?: number | null
  t_protocol?: string | null
  card_type?: number | null
  structure_version?: [number, number] | null
  company_name?: string | null
  company_address?: string | null
  last_auth?: [number, boolean] | null
}

export interface Reader {
  name: string
  status: string
  iccid: string
  card_number: string
  online?: boolean | undefined
  authentication?: boolean | undefined
}

// One card held in a rack slot, as reported by the server's rack discovery.
// card_number/name are null when the card's ICCID is not in the local config —
// the card is visible in the rack section but not served yet.
export interface RackCard {
  slot: number
  iccid?: string | null
  card_number?: string | null
  name?: string | null
  // Session state of this rack-backed card, mirroring Reader.online /
  // Reader.authentication: `online` is true once its MQTT session is up,
  // `authentication` is true while an APDU exchange is actually running.
  // Both absent for a card the rack reports but TBA does not serve.
  online?: boolean | null
  authentication?: boolean | null
}

// State of one connected card rack. The backend pushes the full rack list as
// an array via `rack-state` — several racks can be connected at once, each on
// its own USB port. Unlike a plain reader, one rack holds many cards.
export interface RackState {
  // Stable identity of the rack (its MQTT client_id, derived from the device
  // serial); keys the rack rows in the UI.
  client_id: string
  connected: boolean
  name: string
  serial?: string | null
  manufacturer?: string | null
  product?: string | null
  vid?: number | null
  pid?: number | null
  cards: RackCard[]
  // True once the server has finished enumerating the rack. Absent on older
  // backends, where the UI falls back to its silence timeout.
  scan_complete?: boolean
}

/// The app identifier the backend generates and the server registers a device
/// under: literal "TBA" followed by exactly 13 digits. Shared so the header's
/// reconnect gate and the settings dialog's validation cannot drift apart on
/// what counts as a valid identity.
export const TBA_IDENT_REGEXP = /^TBA\d{13}$/
