import { describe, it, expect, vi, beforeEach } from 'vitest'

const create = vi.fn()
vi.mock('quasar', () => ({ Notify: { create: (...a: unknown[]) => create(...a) } }))

const { notifyError, notifyWarn, notifySuccess, TOAST_SHORT, TOAST_MEDIUM, TOAST_LONG } =
  await import('./notify')

describe('notify helpers', () => {
  beforeEach(() => create.mockClear())

  it('sends errors in red at the medium dwell by default', () => {
    notifyError('Failed to save card')
    expect(create).toHaveBeenCalledWith({
      message: 'Failed to save card',
      color: 'red',
      position: 'bottom',
      timeout: TOAST_MEDIUM,
    })
  })

  it('appends an Error message to the summary', () => {
    notifyError('Failed to save card', new Error('disk full'))
    expect(create.mock.calls[0]?.[0]).toMatchObject({
      message: 'Failed to save card: disk full',
    })
  })

  // The backend rejects some invokes with a plain string, not an Error.
  it('appends a non-Error rejection value', () => {
    notifyError('Update check failed', 'no manifest')
    expect(create.mock.calls[0]?.[0]).toMatchObject({
      message: 'Update check failed: no manifest',
    })
  })

  // A thrown object used to render as "[object Object]", hiding the reason.
  it('renders a thrown object as JSON rather than [object Object]', () => {
    notifyError('Save failed', { code: 42 })
    const message = String(create.mock.calls[0]?.[0]?.message)
    expect(message).toContain('42')
    expect(message).not.toContain('[object Object]')
  })

  it('does not append anything when no error is supplied', () => {
    notifyError('Something went wrong')
    expect(create.mock.calls[0]?.[0]?.message).toBe('Something went wrong')
  })

  it('sends warnings in orange and successes in green', () => {
    notifyWarn('Settings saved, but reconnect failed')
    expect(create.mock.calls[0]?.[0]).toMatchObject({ color: 'orange' })

    create.mockClear()
    notifySuccess('Settings have been updated.', TOAST_SHORT)
    expect(create.mock.calls[0]?.[0]).toMatchObject({
      color: 'green',
      timeout: TOAST_SHORT,
    })
  })

  it('honours an explicit dwell time', () => {
    notifyError('Failed to remove card', undefined, TOAST_LONG)
    expect(create.mock.calls[0]?.[0]).toMatchObject({ timeout: TOAST_LONG })
  })
})
