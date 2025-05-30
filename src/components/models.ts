// Interfaces
export interface SmartCard {
  name?: string
  ICCID?: string
}

export interface Reader {
  name: string
  status: string
  cardICCID: string
  cardNumber: string
  online?: boolean | undefined
  authentication?: boolean | undefined
}
