import { describe, it, expect } from 'vitest'
import type { Reader } from './models'
import { applyReaderEvent, isCardsSyncPayload, readerStatus, unlinkCard } from './readerEvents'
import type { CardsSyncPayload } from './readerEvents'

function event(over: Partial<CardsSyncPayload> = {}): CardsSyncPayload {
  return {
    iccid: '0011223344556677',
    reader_name: 'ACS ACR39U 00',
    card_state: 'State(CHANGED | PRESENT)',
    card_number: 'AAAA000000000001',
    ...over,
  }
}

describe('readerStatus', () => {
  it('takes the first meaningful flag of either form', () => {
    expect(readerStatus('State(CHANGED | PRESENT | INUSE)')).toBe('PRESENT')
    expect(readerStatus('State(EMPTY)')).toBe('EMPTY')
    expect(readerStatus('PRESENT')).toBe('PRESENT')
    expect(readerStatus('State(CHANGED)')).toBe('CHANGED')
    expect(readerStatus('')).toBe('')
  })
})

describe('isCardsSyncPayload', () => {
  it('requires the four string fields', () => {
    expect(isCardsSyncPayload(event())).toBe(true)
    expect(isCardsSyncPayload({ ...event(), iccid: null })).toBe(false)
    expect(isCardsSyncPayload(null)).toBe(false)
  })
})

describe('applyReaderEvent', () => {
  it('adds a reader, then replaces it only when something changed', () => {
    const readers: Reader[] = []
    expect(applyReaderEvent(readers, event())).toBe(true)
    expect(readers).toHaveLength(1)
    expect(readers[0]?.status).toBe('PRESENT')
    const first = readers[0]
    // a keep-alive repeating the same state leaves the row object alone
    expect(applyReaderEvent(readers, event())).toBe(false)
    expect(readers[0]).toBe(first)
    expect(applyReaderEvent(readers, event({ online: true }))).toBe(true)
    expect(readers[0]?.online).toBe(true)
  })

  it('keeps the session flags of the same card on a monitor event without them', () => {
    const readers: Reader[] = []
    applyReaderEvent(readers, event({ online: true, authentication: true }))
    applyReaderEvent(readers, event({ online: null, authentication: null }))
    expect(readers[0]?.online).toBe(true)
    expect(readers[0]?.authentication).toBe(true)
    // another card in the same reader starts without them
    applyReaderEvent(readers, event({ iccid: 'FFEEDDCCBBAA9988', online: null }))
    expect(readers[0]?.online).toBeUndefined()
    expect(readers[0]?.authentication).toBeUndefined()
  })

  it('never keeps an authentication flag for an offline session', () => {
    const readers: Reader[] = []
    applyReaderEvent(readers, event({ online: true, authentication: true }))
    applyReaderEvent(readers, event({ online: false }))
    expect(readers[0]?.authentication).toBe(false)
  })

  it('removes a reader the system no longer knows', () => {
    const readers: Reader[] = []
    applyReaderEvent(readers, event())
    expect(applyReaderEvent(readers, event({ card_state: 'State(CHANGED | UNKNOWN)' }))).toBe(true)
    expect(readers).toHaveLength(0)
    expect(applyReaderEvent(readers, event({ card_state: 'State(IGNORE)' }))).toBe(false)
  })

  it('takes the problem of every event as is', () => {
    const readers: Reader[] = []
    applyReaderEvent(readers, event({ online: true, problem: 'not_registered' }))
    expect(readers[0]?.problem).toBe('not_registered')
    applyReaderEvent(readers, event({ problem: null }))
    expect(readers[0]?.problem).toBeUndefined()
  })
})

describe('unlinkCard', () => {
  it('drops the number and session of the removed card only', () => {
    const readers: Reader[] = []
    applyReaderEvent(readers, event({ online: true, authentication: false }))
    applyReaderEvent(
      readers,
      event({ reader_name: 'other', card_number: 'BBBB000000000002', online: true }),
    )
    unlinkCard(readers, 'AAAA000000000001')
    expect(readers[0]).toMatchObject({ card_number: '', iccid: '0011223344556677' })
    expect(readers[0]?.online).toBeUndefined()
    expect(readers[1]?.card_number).toBe('BBBB000000000002')
  })
})
