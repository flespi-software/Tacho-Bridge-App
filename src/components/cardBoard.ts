// View model of the unified card list: every card the app knows about, joined
// with where it physically is (a USB reader, a rack slot, or nowhere), grouped
// by location. Pure functions only, so the grouping and filter rules are unit
// tested without mounting a component.

import type { RackState, Reader, SmartCard } from './models'
import { formatExpire } from './cardFormatters'

/** Company smart card number: 16 alphanumeric uppercase characters. */
export const CARD_NUMBER_REGEXP = /^[A-Z0-9]{16}$/

const DAY_MS = 86_400_000

/** Expiry this close (or already past) puts a card in the Attention filter. */
export const EXPIRY_WARN_DAYS = 30

/**
 * State of one row:
 *   active   — an APDU exchange is running right now
 *   online   — its MQTT session is up, idle
 *   offline  — inserted and configured, but no session
 *   unlinked — inserted, but its ICCID is not in the configuration
 *   absent   — configured, but not inserted anywhere
 *   empty    — a reader with no card in it
 */
export type CardStatus = 'active' | 'online' | 'offline' | 'unlinked' | 'absent' | 'empty'

/** Status filter of the chip row; `attention` is a cross-cutting check. */
export type StatusFilter = 'all' | 'active' | 'online' | 'offline' | 'unlinked' | 'attention'

export type SortKey = 'slot' | 'name' | 'exp' | 'auth'

export type GroupKind = 'reader' | 'rack' | 'none'

export const READERS_GROUP = 'readers'
export const NOT_INSERTED_GROUP = 'none'

export interface CardEntry {
  /** Stable row identity across state pushes. */
  key: string
  groupId: string
  /** Position inside the group, the default sort order. */
  slot: number
  slotLabel: string
  /** Human-readable location, e.g. "Rack A, slot 005". */
  place: string
  status: CardStatus
  iccid: string
  /** Configured card number; empty for an unlinked card or an empty reader. */
  number: string
  name: string
  /** Configuration entry, when the card is configured. */
  card?: SmartCard | undefined
  /** The card sits in a rack that is disconnected. */
  rackDown: boolean
}

export interface CardGroup {
  id: string
  kind: GroupKind
  title: string
  /** Title unique among the groups, for the location selector. */
  label: string
  subtitle: string
  /** A disconnected rack. */
  down: boolean
  entries: CardEntry[]
  rack?: RackState | undefined
}

function plural(n: number, one: string, many = `${one}s`): string {
  return `${n} ${n === 1 ? one : many}`
}

function sessionStatus(
  online: boolean | null | undefined,
  authentication: boolean | null | undefined,
): CardStatus {
  // Same precedence as the old status icon: no session means offline, even if
  // a stale authentication flag is still set.
  if (!online) return 'offline'
  return authentication ? 'active' : 'online'
}

function readerEntry(reader: Reader, index: number, cards: Record<string, SmartCard>): CardEntry {
  const base = {
    key: `${READERS_GROUP}:${reader.name}`,
    groupId: READERS_GROUP,
    slot: index + 1,
    slotLabel: `R${index + 1}`,
    place: `${reader.name} reader`,
    iccid: reader.iccid,
    rackDown: false,
  }
  // A reader that PC/SC no longer knows, or one without a readable card, is
  // shown as an empty slot — the same rule the old readers list applied.
  if (reader.status === 'UNKNOWN' || (!reader.iccid && !reader.card_number)) {
    return { ...base, status: 'empty', iccid: '', number: '', name: reader.name }
  }
  if (!reader.card_number) {
    return { ...base, status: 'unlinked', number: '', name: '' }
  }
  const card = cards[reader.card_number]
  return {
    ...base,
    status: sessionStatus(reader.online, reader.authentication),
    number: reader.card_number,
    name: card?.name ?? '',
    card,
  }
}

function rackEntries(rack: RackState, cards: Record<string, SmartCard>): CardEntry[] {
  const groupId = rackGroupId(rack)
  return [...rack.cards]
    .sort((a, b) => a.slot - b.slot)
    .map((rc) => {
      const slotLabel = String(rc.slot).padStart(3, '0')
      const base = {
        key: `${groupId}:${rc.slot}`,
        groupId,
        slot: rc.slot,
        slotLabel,
        place: `${rack.name}, slot ${slotLabel}`,
        iccid: rc.iccid ?? '',
        rackDown: !rack.connected,
      }
      if (!rc.card_number) {
        return { ...base, status: 'unlinked' as const, number: '', name: '' }
      }
      const card = cards[rc.card_number]
      return {
        ...base,
        status: rack.connected ? sessionStatus(rc.online, rc.authentication) : ('offline' as const),
        number: rc.card_number,
        // The live config wins over the name the rack snapshot carried.
        name: card?.name ?? rc.name ?? '',
        card,
      }
    })
}

export function rackGroupId(rack: RackState): string {
  // Prefixed so a rack id can never collide with the fixed group ids.
  return `rack:${rack.id}`
}

/**
 * Builds the location groups: USB readers first, then every rack, then the
 * configured cards that are not inserted anywhere (only when there are any).
 */
export function buildGroups(
  readers: Reader[],
  racks: RackState[],
  cards: Record<string, SmartCard>,
): CardGroup[] {
  const readerRows = readers.map((r, i) => readerEntry(r, i, cards))
  const withCard = readerRows.filter((e) => e.status !== 'empty').length
  const groups: CardGroup[] = [
    {
      id: READERS_GROUP,
      kind: 'reader',
      title: 'USB readers',
      label: 'USB readers',
      subtitle: readers.length
        ? `${plural(readers.length, 'reader')} · ${withCard} with a card`
        : 'no readers connected',
      down: false,
      entries: readerRows,
    },
  ]

  const nameCount = new Map<string, number>()
  for (const rack of racks) nameCount.set(rack.name, (nameCount.get(rack.name) ?? 0) + 1)
  for (const rack of racks) {
    const subtitle = [
      rack.serial ? `SN ${rack.serial}` : '',
      rack.connected ? 'connected' : 'disconnected',
    ]
      .filter(Boolean)
      .join(' · ')
    groups.push({
      id: rackGroupId(rack),
      kind: 'rack',
      title: rack.name,
      label:
        (nameCount.get(rack.name) ?? 0) > 1
          ? `${rack.name} · ${rack.serial || rack.id}`
          : rack.name,
      subtitle,
      down: !rack.connected,
      entries: rackEntries(rack, cards),
      rack,
    })
  }

  const inserted = new Set<string>()
  for (const g of groups) for (const e of g.entries) if (e.number) inserted.add(e.number)
  const absent = Object.keys(cards)
    .filter((number) => !inserted.has(number))
    .sort((a, b) => (cards[a]?.name || a).localeCompare(cards[b]?.name || b))
    .map((number, i): CardEntry => ({
      key: `${NOT_INSERTED_GROUP}:${number}`,
      groupId: NOT_INSERTED_GROUP,
      slot: i + 1,
      slotLabel: '—',
      place: 'Not inserted',
      status: 'absent',
      iccid: cards[number]?.iccid ?? '',
      number,
      name: cards[number]?.name ?? '',
      card: cards[number],
      rackDown: false,
    }))
  if (absent.length) {
    groups.push({
      id: NOT_INSERTED_GROUP,
      kind: 'none',
      title: 'Not inserted',
      label: 'Not inserted',
      subtitle: 'configured, not in a reader or rack',
      down: false,
      entries: absent,
    })
  }
  return groups
}

/** Whole days until expiry (negative once expired), or null without a date. */
export function daysToExpiry(entry: CardEntry, now: number): number | null {
  const expire = entry.card?.expire
  if (!expire) return null
  return Math.floor((expire * 1000 - now) / DAY_MS)
}

/** Expired or expiring soon, or the last authentication failed. */
export function needsAttention(entry: CardEntry, now: number): boolean {
  if (entry.status === 'unlinked' || entry.status === 'empty') return false
  const days = daysToExpiry(entry, now)
  if (days !== null && days <= EXPIRY_WARN_DAYS) return true
  return entry.card?.last_auth?.[1] === false
}

export function matchesStatus(entry: CardEntry, filter: StatusFilter, now: number): boolean {
  if (filter === 'all') return true
  if (entry.status === 'empty') return false
  if (filter === 'attention') return needsAttention(entry, now)
  return entry.status === filter
}

/** Case-insensitive substring search; `query` is expected lowercased. */
export function matchesSearch(entry: CardEntry, query: string): boolean {
  if (!query) return true
  if (entry.status === 'empty') return false
  const hay = [
    entry.name,
    entry.number,
    entry.iccid,
    entry.card?.company_name ?? '',
    entry.place,
    entry.slotLabel,
  ]
    .join(' ')
    .toLowerCase()
  return hay.includes(query)
}

function title(entry: CardEntry): string {
  return entry.name || entry.number
}

/** Comparators for the sort selector; rows without the value sort last. */
export const SORTERS: Record<SortKey, (a: CardEntry, b: CardEntry) => number> = {
  slot: (a, b) => a.slot - b.slot,
  name: (a, b) => {
    const ta = title(a)
    const tb = title(b)
    if (!ta || !tb) return ta ? -1 : tb ? 1 : a.slot - b.slot
    return ta.localeCompare(tb)
  },
  exp: (a, b) => (a.card?.expire || Infinity) - (b.card?.expire || Infinity) || a.slot - b.slot,
  auth: (a, b) =>
    (b.card?.last_auth?.[0] ?? -Infinity) - (a.card?.last_auth?.[0] ?? -Infinity) ||
    a.slot - b.slot,
}

/** "5 min ago", "3 h ago", "2 d ago". */
export function formatAgo(unixTs: number, now: number): string {
  const mins = Math.max(0, Math.floor((now - unixTs * 1000) / 60_000))
  if (mins < 1) return 'just now'
  if (mins < 60) return `${mins} min ago`
  if (mins < 1440) return `${Math.floor(mins / 60)} h ago`
  return `${Math.floor(mins / 1440)} d ago`
}

export type Tone = 'muted' | 'active' | 'warn' | 'danger'

export interface ToneText {
  text: string
  tone: Tone
}

/** The right-hand top line of a collapsed row: session / last authentication. */
export function authLine(entry: CardEntry, now: number): ToneText {
  if (entry.status === 'empty') return { text: '', tone: 'muted' }
  if (entry.rackDown) return { text: 'rack disconnected', tone: 'danger' }
  if (entry.status === 'active') return { text: 'authenticating…', tone: 'active' }
  if (entry.status === 'unlinked') return { text: 'not in configuration', tone: 'muted' }
  const last = entry.card?.last_auth
  if (!last) return { text: 'no auth yet', tone: 'muted' }
  if (entry.status === 'offline') return { text: 'no session', tone: 'muted' }
  const ago = formatAgo(last[0], now)
  if (entry.status === 'absent')
    return { text: `last auth ${ago}`, tone: last[1] ? 'muted' : 'danger' }
  return last[1]
    ? { text: `auth ${ago}`, tone: 'muted' }
    : { text: `auth failed · ${ago}`, tone: 'danger' }
}

/** The right-hand bottom line of a collapsed row: card expiry. */
export function expiryLine(entry: CardEntry, now: number): ToneText {
  const days = daysToExpiry(entry, now)
  if (days === null || entry.status === 'unlinked') return { text: '', tone: 'muted' }
  const date = formatExpire(entry.card?.expire)
  if (days < 0) return { text: `expired ${date}`, tone: 'danger' }
  if (days <= EXPIRY_WARN_DAYS) return { text: `expires in ${days} d`, tone: 'warn' }
  return { text: `exp ${date}`, tone: 'muted' }
}

/**
 * The single status line of a tile: an urgent expiry wins over the session
 * line, since a tile has no room for both, unless the session line reports
 * something worse than an upcoming expiry (a failed auth, a disconnected rack).
 */
export function tileLine(entry: CardEntry, now: number): ToneText {
  if (entry.status === 'empty') return { text: 'empty reader', tone: 'muted' }
  if (entry.status === 'unlinked') {
    return { text: entry.iccid ? 'not configured · click to link' : 'ICCID not read', tone: 'warn' }
  }
  const session = authLine(entry, now)
  const days = daysToExpiry(entry, now)
  if (days !== null && days <= EXPIRY_WARN_DAYS) {
    const expiry = expiryLine(entry, now)
    if (expiry.tone === 'danger' || session.tone !== 'danger') return expiry
  }
  return session
}

/** How cards are drawn inside a group. */
export type CardView = 'list' | 'tiles' | 'mini'

export const CARD_VIEWS: readonly CardView[] = ['list', 'tiles', 'mini']

/** Default window width, as set in tauri.conf.json. */
export const BASE_WINDOW_WIDTH = 620

/**
 * Window width (px) above which the window counts as stretched: more than 15%
 * wider than the default.
 */
export const WIDE_WINDOW_AT = Math.round(BASE_WINDOW_WIDTH * 1.15)

/**
 * The view chosen for a normal and for a stretched window, remembered
 * separately: stretching the window switches to medium tiles until the user
 * picks another view there, narrowing it brings the list back.
 */
export interface ViewPrefs {
  narrow: CardView
  wide: CardView
}

export const DEFAULT_VIEW_PREFS: ViewPrefs = { narrow: 'list', wide: 'tiles' }

export function viewSlot(windowWidth: number): keyof ViewPrefs {
  return windowWidth > WIDE_WINDOW_AT ? 'wide' : 'narrow'
}

/**
 * How far below WIDE_WINDOW_AT a stretched window must shrink before the
 * normal view comes back. Without this band a window edge resting on the
 * threshold flips between tiles and list on every pixel of hand jitter, and
 * the two layouts differ enough that the content visibly jumps each time.
 */
export const VIEW_SWITCH_BAND = 24

/** The next view slot for `windowWidth`, with hysteresis around the threshold. */
export function nextViewSlot(current: keyof ViewPrefs, windowWidth: number): keyof ViewPrefs {
  if (current === 'wide') return windowWidth < WIDE_WINDOW_AT - VIEW_SWITCH_BAND ? 'narrow' : 'wide'
  return viewSlot(windowWidth)
}

/** Reads stored prefs, falling back per field on anything unexpected. */
export function parseViewPrefs(raw: string | null): ViewPrefs {
  let parsed: unknown = null
  try {
    parsed = raw ? JSON.parse(raw) : null
  } catch {
    parsed = null
  }
  const p = parsed && typeof parsed === 'object' ? (parsed as Record<string, unknown>) : {}
  const pick = (v: unknown, fallback: CardView): CardView =>
    CARD_VIEWS.includes(v as CardView) ? (v as CardView) : fallback
  return {
    narrow: pick(p.narrow, DEFAULT_VIEW_PREFS.narrow),
    wide: pick(p.wide, DEFAULT_VIEW_PREFS.wide),
  }
}

export interface GroupSummary {
  counts: Record<Exclude<CardStatus, 'empty'>, number>
  /** Cards in the group, not counting empty readers. */
  total: number
  attention: number
}

export function summarize(group: CardGroup, now: number): GroupSummary {
  const counts = { active: 0, online: 0, offline: 0, unlinked: 0, absent: 0 }
  let total = 0
  let attention = 0
  for (const e of group.entries) {
    if (e.status === 'empty') continue
    total++
    counts[e.status]++
    if (needsAttention(e, now)) attention++
  }
  return { counts, total, attention }
}

/** Configured cards without an ICCID: the ones an unknown card can be linked to. */
export function linkableCards(
  cards: Record<string, SmartCard>,
): { number: string; card: SmartCard }[] {
  return Object.entries(cards)
    .filter(([, card]) => !card.iccid)
    .map(([number, card]) => ({ number, card }))
    .sort((a, b) => (a.card.name || a.number).localeCompare(b.card.name || b.number))
}

/**
 * Validates a number typed for a new card. Returns the error text, or '' when
 * the number can be saved.
 */
export function newCardNumberError(number: string, cards: Record<string, SmartCard>): string {
  if (!CARD_NUMBER_REGEXP.test(number)) return 'Card number must be 16 characters (A-Z, 0-9 only)'
  // hasOwnProperty instead of `in`, so prototype names like "constructor"
  // cannot falsely report "already exists".
  if (Object.prototype.hasOwnProperty.call(cards, number)) return 'Card number already exists'
  return ''
}
