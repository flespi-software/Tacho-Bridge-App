import { describe, it, expect } from 'vitest'
import {
  buildGroups,
  needsAttention,
  matchesSearch,
  matchesStatus,
  authLine,
  expiryLine,
  formatAgo,
  summarize,
  linkableCards,
  newCardNumberError,
  tileLine,
  viewSlot,
  nextViewSlot,
  VIEW_SWITCH_BAND,
  parseViewPrefs,
  WIDE_WINDOW_AT,
  SORTERS,
  type CardEntry,
} from './cardBoard'
import type { RackState, Reader, SmartCard } from './models'

const NOW = Date.UTC(2026, 9, 28, 12, 0, 0)
const DAY_S = 86_400
const nowS = NOW / 1000

function reader(over: Partial<Reader> = {}): Reader {
  return { name: 'ACS ACR39U', status: 'PRESENT', iccid: '', card_number: '', ...over }
}

function rack(over: Partial<RackState> = {}): RackState {
  return { id: 'r1', connected: true, name: 'Rack', serial: 'LSL-1', cards: [], ...over }
}

describe('buildGroups', () => {
  it('always lists the readers group, then racks, then not-inserted cards', () => {
    const cards: Record<string, SmartCard> = { AAAA000000000001: { name: 'Spare', iccid: '' } }
    const groups = buildGroups([], [rack()], cards)
    expect(groups.map((g) => g.id)).toEqual(['readers', 'rack:r1', 'none'])
    expect(groups[0]?.subtitle).toBe('no readers connected')
  })

  it('omits the not-inserted group when every configured card is inserted', () => {
    const cards: Record<string, SmartCard> = { AAAA000000000001: { name: 'A', iccid: '01' } }
    const groups = buildGroups(
      [reader({ iccid: '01', card_number: 'AAAA000000000001' })],
      [],
      cards,
    )
    expect(groups.map((g) => g.id)).toEqual(['readers'])
  })

  it('maps reader states to row statuses', () => {
    const cards: Record<string, SmartCard> = { AAAA000000000001: { name: 'A', iccid: '01' } }
    const rows = buildGroups(
      [
        reader({ name: 'empty' }),
        reader({ name: 'unknown', iccid: 'FF' }),
        reader({ name: 'offline', iccid: '01', card_number: 'AAAA000000000001' }),
        reader({ name: 'online', iccid: '01', card_number: 'AAAA000000000001', online: true }),
        reader({
          name: 'active',
          iccid: '01',
          card_number: 'AAAA000000000001',
          online: true,
          authentication: true,
        }),
        reader({ name: 'gone', status: 'UNKNOWN', iccid: '01', card_number: 'AAAA000000000001' }),
      ],
      [],
      cards,
    )[0]!.entries
    expect(rows.map((e) => e.status)).toEqual([
      'empty',
      'unlinked',
      'offline',
      'online',
      'active',
      'empty',
    ])
    expect(rows[3]?.name).toBe('A')
    expect(rows[1]?.iccid).toBe('FF')
  })

  it('treats a stale authentication flag without a session as offline', () => {
    const rows = buildGroups(
      [reader({ iccid: '01', card_number: 'N', online: false, authentication: true })],
      [],
      {},
    )[0]!.entries
    expect(rows[0]?.status).toBe('offline')
  })

  it('sorts rack slots and marks the cards of a disconnected rack offline', () => {
    const cards: Record<string, SmartCard> = { N1: { name: 'Config name', iccid: '01' } }
    const g = buildGroups(
      [],
      [
        rack({
          connected: false,
          cards: [
            { slot: 7, iccid: 'FF', card_number: null },
            { slot: 2, iccid: '01', card_number: 'N1', name: 'Rack name', online: true },
          ],
        }),
      ],
      cards,
    )[1]!
    expect(g.down).toBe(true)
    expect(g.subtitle).toBe('SN LSL-1 · disconnected')
    expect(g.entries.map((e) => [e.slotLabel, e.status, e.rackDown])).toEqual([
      ['002', 'offline', true],
      ['007', 'unlinked', true],
    ])
    // The live config name wins over the rack snapshot.
    expect(g.entries[0]?.name).toBe('Config name')
    expect(g.entries[0]?.place).toBe('Rack, slot 002')
  })

  it('makes duplicate rack names distinguishable in the location label', () => {
    const groups = buildGroups(
      [],
      [
        rack({ id: 'a', serial: 'S1' }),
        rack({ id: 'b', serial: 'S2' }),
        rack({ id: 'c', name: 'Other' }),
      ],
      {},
    )
    expect(groups.slice(1).map((g) => g.label)).toEqual(['Rack · S1', 'Rack · S2', 'Other'])
  })

  it('lists a configured card as not inserted unless a reader or rack holds it', () => {
    const cards: Record<string, SmartCard> = {
      INREADER00000001: { name: 'r' },
      INRACK0000000001: { name: 'k' },
      ZZZ0000000000001: { name: 'b' },
      YYY0000000000001: { name: 'a' },
    }
    const groups = buildGroups(
      [reader({ iccid: '01', card_number: 'INREADER00000001' })],
      [rack({ cards: [{ slot: 1, iccid: '02', card_number: 'INRACK0000000001' }] })],
      cards,
    )
    const none = groups.find((g) => g.id === 'none')!
    // Sorted by name for the default (slot) order.
    expect(none.entries.map((e) => e.number)).toEqual(['YYY0000000000001', 'ZZZ0000000000001'])
    expect(none.entries.every((e) => e.status === 'absent')).toBe(true)
  })
})

function entry(over: Partial<CardEntry> = {}): CardEntry {
  return {
    key: 'k',
    groupId: 'readers',
    slot: 1,
    slotLabel: 'R1',
    place: 'X reader',
    status: 'online',
    iccid: 'AB12',
    number: 'LV10000000000001',
    name: 'Baltic #1',
    card: { name: 'Baltic #1', iccid: 'AB12', company_name: 'Baltic Freight SIA' },
    rackDown: false,
    ...over,
  }
}

describe('needsAttention', () => {
  it('flags expired, soon-expiring and failed-auth cards', () => {
    expect(needsAttention(entry({ card: { expire: nowS - DAY_S } }), NOW)).toBe(true)
    expect(needsAttention(entry({ card: { expire: nowS + 10 * DAY_S } }), NOW)).toBe(true)
    expect(needsAttention(entry({ card: { last_auth: [nowS, false] } }), NOW)).toBe(true)
    expect(
      needsAttention(entry({ card: { expire: nowS + 90 * DAY_S, last_auth: [nowS, true] } }), NOW),
    ).toBe(false)
  })

  it('never flags unknown cards or empty readers', () => {
    expect(needsAttention(entry({ status: 'unlinked', card: { expire: 1 } }), NOW)).toBe(false)
    expect(needsAttention(entry({ status: 'empty', card: { expire: 1 } }), NOW)).toBe(false)
  })
})

describe('filters', () => {
  it('matches search over name, number, ICCID, company and place', () => {
    const e = entry()
    for (const q of ['baltic #1', 'lv1000', 'ab12', 'freight sia', 'x reader']) {
      expect(matchesSearch(e, q)).toBe(true)
    }
    expect(matchesSearch(e, 'nord')).toBe(false)
    expect(matchesSearch(entry({ status: 'empty' }), 'x')).toBe(false)
    expect(matchesSearch(entry({ status: 'empty' }), '')).toBe(true)
  })

  it('keeps empty readers only in the unfiltered view', () => {
    expect(matchesStatus(entry({ status: 'empty' }), 'all', NOW)).toBe(true)
    expect(matchesStatus(entry({ status: 'empty' }), 'offline', NOW)).toBe(false)
    expect(matchesStatus(entry({ status: 'absent' }), 'offline', NOW)).toBe(false)
    expect(matchesStatus(entry({ status: 'unlinked' }), 'unlinked', NOW)).toBe(true)
  })
})

describe('SORTERS', () => {
  it('puts rows without the sort value last, in slot order', () => {
    const a = entry({ slot: 1, card: { expire: null } })
    const b = entry({ slot: 2, card: { expire: nowS } })
    const c = entry({ slot: 3, card: { expire: null } })
    expect([c, a, b].sort(SORTERS.exp).map((e) => e.slot)).toEqual([2, 1, 3])
  })

  it('sorts the most recent authentication first', () => {
    const a = entry({ slot: 1, card: { last_auth: [100, true] } })
    const b = entry({ slot: 2, card: { last_auth: [200, true] } })
    const c = entry({ slot: 3, card: {} })
    expect([a, c, b].sort(SORTERS.auth).map((e) => e.slot)).toEqual([2, 1, 3])
  })

  it('sorts unknown cards after named ones', () => {
    const a = entry({ slot: 1, name: '', number: '' })
    const b = entry({ slot: 2, name: 'b' })
    const c = entry({ slot: 3, name: '', number: 'A000000000000001' })
    expect([a, b, c].sort(SORTERS.name).map((e) => e.slot)).toEqual([3, 2, 1])
  })
})

describe('row lines', () => {
  it('formats relative time', () => {
    expect(formatAgo(nowS - 10, NOW)).toBe('just now')
    expect(formatAgo(nowS - 5 * 60, NOW)).toBe('5 min ago')
    expect(formatAgo(nowS - 3 * 3600, NOW)).toBe('3 h ago')
    expect(formatAgo(nowS - 2 * DAY_S, NOW)).toBe('2 d ago')
  })

  it('describes the session and the last authentication', () => {
    const ok: [number, boolean] = [nowS - 300, true]
    const bad: [number, boolean] = [nowS - 300, false]
    expect(authLine(entry({ status: 'active' }), NOW)).toEqual({
      text: 'authenticating…',
      tone: 'active',
    })
    expect(authLine(entry({ rackDown: true, status: 'offline' }), NOW).text).toBe(
      'rack disconnected',
    )
    expect(authLine(entry({ status: 'unlinked' }), NOW).text).toBe('not in configuration')
    expect(authLine(entry({ card: {} }), NOW).text).toBe('no auth yet')
    expect(authLine(entry({ status: 'offline', card: { last_auth: ok } }), NOW).text).toBe(
      'no session',
    )
    expect(authLine(entry({ card: { last_auth: ok } }), NOW)).toEqual({
      text: 'auth 5 min ago',
      tone: 'muted',
    })
    expect(authLine(entry({ card: { last_auth: bad } }), NOW)).toEqual({
      text: 'auth failed · 5 min ago',
      tone: 'danger',
    })
    expect(authLine(entry({ status: 'absent', card: { last_auth: ok } }), NOW).text).toBe(
      'last auth 5 min ago',
    )
  })

  it('describes the expiry', () => {
    expect(expiryLine(entry({ card: { expire: nowS - DAY_S } }), NOW)).toEqual({
      text: 'expired 2026-10-27',
      tone: 'danger',
    })
    expect(expiryLine(entry({ card: { expire: nowS + 10 * DAY_S } }), NOW)).toEqual({
      text: 'expires in 10 d',
      tone: 'warn',
    })
    expect(expiryLine(entry({ card: { expire: nowS + 100 * DAY_S } }), NOW).tone).toBe('muted')
    expect(expiryLine(entry({ card: {} }), NOW).text).toBe('')
  })
})

describe('summarize', () => {
  it('counts statuses and attention, skipping empty readers', () => {
    const s = summarize(
      {
        id: 'readers',
        kind: 'reader',
        title: '',
        label: '',
        subtitle: '',
        down: false,
        entries: [
          entry({ status: 'online' }),
          entry({ status: 'unlinked' }),
          entry({ status: 'empty' }),
          entry({ status: 'offline', card: { last_auth: [nowS, false] } }),
        ],
      },
      NOW,
    )
    expect(s.total).toBe(3)
    expect(s.counts).toEqual({ active: 0, online: 1, offline: 1, unlinked: 1, absent: 0 })
    expect(s.attention).toBe(1)
  })
})

describe('card config helpers', () => {
  it('offers only cards without an ICCID for linking', () => {
    const list = linkableCards({
      B000000000000001: { name: 'b', iccid: '' },
      A000000000000001: { name: 'a' },
      C000000000000001: { name: 'c', iccid: '01' },
    })
    expect(list.map((x) => x.number)).toEqual(['A000000000000001', 'B000000000000001'])
  })

  it('validates a new card number', () => {
    const cards = { A000000000000001: {} }
    expect(newCardNumberError('A000000000000002', cards)).toBe('')
    expect(newCardNumberError('A000000000000001', cards)).toBe('Card number already exists')
    expect(newCardNumberError('a000000000000002', cards)).not.toBe('')
    expect(newCardNumberError('A00000000000000', cards)).not.toBe('')
    expect(newCardNumberError('CONSTRUCTOR00000', cards)).toBe('')
  })
})

describe('tileLine', () => {
  it('shows an urgent expiry instead of the session line', () => {
    const e = entry({ card: { expire: nowS + 5 * DAY_S, last_auth: [nowS - 300, true] } })
    expect(tileLine(e, NOW)).toEqual({ text: 'expires in 5 d', tone: 'warn' })
  })

  it('does not hide a failed auth or a disconnected rack behind an upcoming expiry', () => {
    const soon = nowS + 5 * DAY_S
    expect(
      tileLine(entry({ card: { expire: soon, last_auth: [nowS - 300, false] } }), NOW),
    ).toEqual({ text: 'auth failed · 5 min ago', tone: 'danger' })
    expect(
      tileLine(entry({ status: 'offline', rackDown: true, card: { expire: soon } }), NOW).text,
    ).toBe('rack disconnected')
    // An expired card explains the failed auth, so the expiry is shown.
    expect(
      tileLine(entry({ card: { expire: nowS - DAY_S, last_auth: [nowS - 300, false] } }), NOW).tone,
    ).toBe('danger')
    expect(
      tileLine(entry({ card: { expire: nowS - DAY_S, last_auth: [nowS - 300, false] } }), NOW).text,
    ).toMatch(/^expired /)
  })

  it('falls back to the session line', () => {
    expect(tileLine(entry({ card: { last_auth: [nowS - 300, true] } }), NOW).text).toBe(
      'auth 5 min ago',
    )
    expect(tileLine(entry({ status: 'active' }), NOW).text).toBe('authenticating…')
  })

  it('invites linking an unknown card', () => {
    expect(tileLine(entry({ status: 'unlinked' }), NOW)).toEqual({
      text: 'not configured · click to link',
      tone: 'warn',
    })
    expect(tileLine(entry({ status: 'unlinked', iccid: '' }), NOW).text).toBe('ICCID not read')
    expect(tileLine(entry({ status: 'empty' }), NOW).text).toBe('empty reader')
  })
})

describe('view prefs', () => {
  it('switches to the stretched-window slot above 15% over the default width', () => {
    expect(WIDE_WINDOW_AT).toBe(713)
    expect(viewSlot(620)).toBe('narrow')
    expect(viewSlot(713)).toBe('narrow')
    expect(viewSlot(714)).toBe('wide')
  })

  it('defaults to the list normally and medium tiles when stretched', () => {
    expect(parseViewPrefs(null)).toEqual({ narrow: 'list', wide: 'tiles' })
  })

  it('keeps stored choices and repairs broken ones field by field', () => {
    expect(parseViewPrefs('{"narrow":"mini","wide":"list"}')).toEqual({
      narrow: 'mini',
      wide: 'list',
    })
    expect(parseViewPrefs('{"narrow":"grid","wide":"mini"}')).toEqual({
      narrow: 'list',
      wide: 'mini',
    })
    expect(parseViewPrefs('not json')).toEqual({ narrow: 'list', wide: 'tiles' })
    expect(parseViewPrefs('[1]')).toEqual({ narrow: 'list', wide: 'tiles' })
  })

  it('keeps the stretched view until the window is clearly narrower again', () => {
    // Up: switches once above the threshold.
    expect(nextViewSlot('narrow', 713)).toBe('narrow')
    expect(nextViewSlot('narrow', 714)).toBe('wide')
    // Down: a window resting on the threshold does not flip back...
    expect(nextViewSlot('wide', 713)).toBe('wide')
    expect(nextViewSlot('wide', 713 - VIEW_SWITCH_BAND)).toBe('wide')
    // ...only one clearly below it does.
    expect(nextViewSlot('wide', 713 - VIEW_SWITCH_BAND - 1)).toBe('narrow')
  })

  it('does not flap when the width jitters around the threshold', () => {
    let slot: 'narrow' | 'wide' = 'narrow'
    const seen: string[] = []
    for (const w of [700, 714, 712, 715, 711, 713, 710, 716]) {
      slot = nextViewSlot(slot, w)
      seen.push(slot)
    }
    expect(seen).toEqual(['narrow', 'wide', 'wide', 'wide', 'wide', 'wide', 'wide', 'wide'])
  })
})
