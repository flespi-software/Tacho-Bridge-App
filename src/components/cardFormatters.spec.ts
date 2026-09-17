import { describe, it, expect, vi, afterEach } from 'vitest'
import {
  formatCardMeta,
  formatStructureVersion,
  formatExpire,
  isExpired,
  formatAuthDate,
  cardStatusIcon,
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

describe('isExpired', () => {
  afterEach(() => {
    vi.useRealTimers()
  })

  it('compares against the current time', () => {
    vi.useFakeTimers()
    vi.setSystemTime(new Date('2026-06-01T00:00:00Z'))
    expect(isExpired(Math.floor(Date.parse('2026-05-31T00:00:00Z') / 1000))).toBe(true)
    expect(isExpired(Math.floor(Date.parse('2026-06-02T00:00:00Z') / 1000))).toBe(false)
  })

  // An unknown expiry must not render the card as expired — that would flag
  // every card the sniffer has not read yet.
  it('treats an unknown expiry as not expired', () => {
    expect(isExpired(null)).toBe(false)
    expect(isExpired(undefined)).toBe(false)
    expect(isExpired(0)).toBe(false)
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

describe('cardStatusIcon', () => {
  it('shows an empty slot when no card is present', () => {
    const icon = cardStatusIcon({ present: false, linked: true, online: true })
    expect(icon.name).toBe('mdi-smart-card-off-outline')
    expect(icon.color).toBe('grey')
  })

  it('prompts to link a card that is present but unconfigured', () => {
    const icon = cardStatusIcon({ present: true, linked: false })
    expect(icon.name).toBe('mdi-card-plus-outline')
    expect(icon.color).toBe('orange')
  })

  it('greys out a linked card with no session', () => {
    const icon = cardStatusIcon({ present: true, linked: true, online: false })
    expect(icon.name).toBe('mdi-smart-card-outline')
    expect(icon.color).toBe('grey')
  })

  it('goes solid green when the session is up and idle', () => {
    const icon = cardStatusIcon({ present: true, linked: true, online: true })
    expect(icon).toMatchObject({ name: 'mdi-smart-card', color: 'green' })
    expect(icon.class).toBeUndefined()
  })

  it('blinks while an APDU exchange is running', () => {
    const icon = cardStatusIcon({
      present: true,
      linked: true,
      online: true,
      authentication: true,
    })
    expect(icon.class).toBe('blinking-icon')
  })

  // online/authentication are tri-state on the wire; "not known yet" must read
  // the same as false rather than throwing or blinking.
  it('treats an absent online flag as offline', () => {
    expect(cardStatusIcon({ present: true, linked: true }).name).toBe('mdi-smart-card-outline')
    expect(cardStatusIcon({ present: true, linked: true, online: null }).color).toBe('grey')
  })

  it('honours a caller-supplied size and defaults otherwise', () => {
    expect(cardStatusIcon({ present: false, linked: false }).size).toBe('25px')
    expect(cardStatusIcon({ present: false, linked: false }, '12px').size).toBe('12px')
  })
})
