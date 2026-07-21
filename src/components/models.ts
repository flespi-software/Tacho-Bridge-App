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
}

// State of the connected card rack, pushed from the backend via `rack-state`.
// Unlike a plain reader, one rack holds many cards.
export interface RackState {
  connected: boolean
  name: string
  serial?: string | null
  manufacturer?: string | null
  product?: string | null
  vid?: number | null
  pid?: number | null
  cards: RackCard[]
}
