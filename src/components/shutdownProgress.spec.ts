import { describe, expect, it } from 'vitest'
import { isShutdownState, shutdownProgress } from './shutdownProgress'

describe('shutdown progress', () => {
  it('reaches 100% at the estimate and remains there while cleanup continues', () => {
    expect(shutdownProgress(4000, 0)).toBe(0)
    expect(shutdownProgress(4000, 2000)).toBe(0.5)
    expect(shutdownProgress(4000, 4000)).toBe(1)
    expect(shutdownProgress(4000, 15000)).toBe(1)
  })

  it('accepts replayed progress and rejects invalid timing data', () => {
    const state = { phase: 'connections', slots: 480, estimated_ms: 4390, elapsed_ms: 5000 }
    expect(isShutdownState(state)).toBe(true)
    for (const raw of [
      null,
      {},
      { ...state, phase: 'done' },
      { ...state, elapsed_ms: NaN },
      { ...state, estimated_ms: 0 },
      { ...state, slots: -1 },
    ]) {
      expect(isShutdownState(raw)).toBe(false)
    }
  })
})
