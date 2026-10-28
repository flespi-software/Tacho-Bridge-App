import { describe, it, expect } from 'vitest'
import {
  formatCardMeta,
  formatStructureVersion,
  formatExpire,
  formatAuthDate,
} from './cardFormatters'

describe('formatStructureVersion', () => {
  it('names the generations the app knows', () => {
    expect(formatStructureVersion([0, 0])).toBe('Gen1')
    expect(formatStructureVersion([1, 0])).toBe('Gen2 v1')
    expect(formatStructureVersion([1, 1])).toBe('Gen2 v2')
  })

  it('falls back to a raw version for anything newer', () => {
    // A card structure the app predates must still render something truthful
    // rather than claiming to be a generation it is not.
    expect(formatStructureVersion([1, 2])).toBe('v1.2')
    expect(formatStructureVersion([2, 0])).toBe('v2.0')
  })

  it('renders nothing when the version is unknown', () => {
    expect(formatStructureVersion(null)).toBe('')
    expect(formatStructureVersion(undefined)).toBe('')
  })
})

describe('formatCardMeta', () => {
  it('joins the parts it has', () => {
    expect(formatCardMeta({ structure_version: [0, 0], card_type: 4 })).toBe('Gen1 | Company Card')
  })

  it('omits missing parts rather than leaving a dangling separator', () => {
    expect(formatCardMeta({ structure_version: [1, 0], card_type: null })).toBe('Gen2 v1')
    expect(formatCardMeta({ structure_version: null, card_type: 1 })).toBe('Driver Card')
    expect(formatCardMeta({ structure_version: null, card_type: null })).toBe('')
  })

  it('labels every card type defined by the tachograph spec', () => {
    const t = (card_type: number) => formatCardMeta({ structure_version: null, card_type })
    expect(t(1)).toBe('Driver Card')
    expect(t(2)).toBe('Workshop Card')
    expect(t(3)).toBe('Control Card')
    expect(t(4)).toBe('Company Card')
  })

  it('surfaces an unrecognised type instead of hiding it', () => {
    expect(formatCardMeta({ structure_version: null, card_type: 9 })).toBe('Unknown (9)')
  })

  // card_type 0 is falsy — a `!card.card_type` guard would drop it silently.
  it('does not treat card type 0 as absent', () => {
    expect(formatCardMeta({ structure_version: null, card_type: 0 })).toBe('Unknown (0)')
  })
})

describe('formatExpire', () => {
  it('renders a UTC date', () => {
    // 2030-01-01T00:00:00Z
    expect(formatExpire(1893456000)).toBe('2030-01-01')
  })

  it('renders nothing for an absent or zero timestamp', () => {
    expect(formatExpire(null)).toBe('')
    expect(formatExpire(undefined)).toBe('')
    expect(formatExpire(0)).toBe('')
  })
})

describe('formatAuthDate', () => {
  it('renders local time zero-padded to a fixed width', () => {
    const ts = Math.floor(Date.parse('2026-03-04T05:06:07Z') / 1000)
    const out = formatAuthDate(ts)
    expect(out).toMatch(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/)

    // Storage is UTC, display is local: assert against the local rendering of
    // the same instant rather than a hard-coded string, so the test holds in
    // any time zone.
    const d = new Date(ts * 1000)
    const pad = (n: number) => String(n).padStart(2, '0')
    expect(out).toBe(
      `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ` +
        `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`,
    )
  })

  it('renders nothing when the card has never authenticated', () => {
    expect(formatAuthDate(null)).toBe('')
    expect(formatAuthDate(0)).toBe('')
  })
})
