// Interfaces
export interface SmartCard {
  name?: string
  iccid?: string
  expire?: number | null
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
