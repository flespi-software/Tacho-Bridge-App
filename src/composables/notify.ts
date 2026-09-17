import { Notify } from 'quasar'

/**
 * Toast helpers wrapping `Notify.create`.
 *
 * Every toast in the app is bottom-positioned and differs only in colour and
 * dwell time, so those are the only knobs here. Going through these helpers
 * keeps a failure from being reported in a style the user has not seen before.
 */

/**
 * Renders a caught value for display. `Error` is by far the common case and
 * stringifies usefully; anything else goes through JSON so a thrown object
 * shows its contents instead of "[object Object]".
 */
function describe(error: unknown): string {
  if (error instanceof Error) return error.message
  if (typeof error === 'object' && error !== null) {
    try {
      return JSON.stringify(error)
    } catch {
      return Object.prototype.toString.call(error)
    }
  }
  return String(error)
}

/** Dwell times, longest for the messages the user must not miss. */
export const TOAST_SHORT = 3000
export const TOAST_MEDIUM = 5000
export const TOAST_LONG = 8000

/** Reports a failure. `error`, when given, is appended as its string form. */
export function notifyError(message: string, error?: unknown, timeout = TOAST_MEDIUM): void {
  Notify.create({
    message: error === undefined ? message : `${message}: ${describe(error)}`,
    color: 'red',
    position: 'bottom',
    timeout,
  })
}

/** Reports a partial success — the main action worked, a follow-up did not. */
export function notifyWarn(message: string, error?: unknown, timeout = TOAST_MEDIUM): void {
  Notify.create({
    message: error === undefined ? message : `${message}: ${describe(error)}`,
    color: 'orange',
    position: 'bottom',
    timeout,
  })
}

/** Confirms an action completed. */
export function notifySuccess(message: string, timeout = TOAST_MEDIUM): void {
  Notify.create({ message, color: 'green', position: 'bottom', timeout })
}
