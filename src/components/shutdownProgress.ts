export interface ShutdownState {
  phase: 'indicators' | 'connections' | 'installing'
  slots: number
  estimated_ms: number
  elapsed_ms: number
}

export function isShutdownState(raw: unknown): raw is ShutdownState {
  if (!raw || typeof raw !== 'object') return false
  const value = raw as Record<string, unknown>
  return (
    ['indicators', 'connections', 'installing'].includes(String(value.phase)) &&
    typeof value.slots === 'number' &&
    Number.isInteger(value.slots) &&
    value.slots >= 0 &&
    typeof value.estimated_ms === 'number' &&
    Number.isFinite(value.estimated_ms) &&
    value.estimated_ms > 0 &&
    typeof value.elapsed_ms === 'number' &&
    Number.isFinite(value.elapsed_ms) &&
    value.elapsed_ms >= 0
  )
}

export function shutdownProgress(estimatedMs: number, elapsedMs: number): number {
  return Math.min(1, Math.max(0, elapsedMs / Math.max(1, estimatedMs)))
}
