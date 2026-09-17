import { onUnmounted } from 'vue'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'

/**
 * Registers Tauri event listeners that are torn down when the component
 * unmounts.
 *
 * Listeners must be owned by the component rather than registered at setup
 * top-level: a leaked listener outlives its component and keeps mutating dead
 * state, piling up across remounts and hot reloads and double-firing on the
 * next mount.
 *
 * Register every listener BEFORE notifying the backend that the frontend is
 * loaded — otherwise the initial sync burst can race channel registration and
 * arrive into the void, leaving the UI on stale state.
 */
export function useTauriListeners() {
  const unlistenFns: UnlistenFn[] = []

  /**
   * Attaches one handler to `event`. A failure to register is logged and
   * swallowed so one bad channel cannot abort the rest of the wiring.
   */
  const on = async (event: string, handler: (payload: unknown) => void): Promise<void> => {
    try {
      unlistenFns.push(await listen(event, (e) => handler(e.payload)))
    } catch (error) {
      console.error(`Error listening to ${event}:`, error)
    }
  }

  onUnmounted(() => {
    while (unlistenFns.length > 0) {
      const fn = unlistenFns.pop()
      try {
        fn?.()
      } catch (e) {
        console.error('Error detaching Tauri listener:', e)
      }
    }
  })

  return { on }
}
