import { describe, it, expect } from 'vitest'
import { isGlobalConfigServerPayload } from './models'

describe('isGlobalConfigServerPayload', () => {
  it('accepts the backend payload, every value a string', () => {
    expect(
      isGlobalConfigServerPayload({
        host: 'mqtt.flespi.io:8883',
        ident: 'TBA0000000000001',
        dark_theme: 'Auto',
        auth_enabled: 'false',
        debug_log_until: '0',
      }),
    ).toBe(true)
    expect(isGlobalConfigServerPayload({})).toBe(true)
  })

  it('ignores unknown fields', () => {
    expect(isGlobalConfigServerPayload({ host: 'h:1', extra: 42 })).toBe(true)
  })

  it('rejects non-objects and known fields of the wrong type', () => {
    expect(isGlobalConfigServerPayload(null)).toBe(false)
    expect(isGlobalConfigServerPayload('host')).toBe(false)
    expect(isGlobalConfigServerPayload(['host'])).toBe(false)
    expect(isGlobalConfigServerPayload({ host: 1 })).toBe(false)
    expect(isGlobalConfigServerPayload({ auth_enabled: true })).toBe(false)
  })
})
