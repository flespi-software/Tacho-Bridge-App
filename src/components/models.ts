// Interfaces
export interface SmartCard {
  name?: string
  iccid?: string
}

export interface Reader {
  name: string
  status: string
  iccid: string
  card_number: string
  online?: boolean | undefined
  authentication?: boolean | undefined
}
