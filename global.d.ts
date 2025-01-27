// Global types for TypeScript

// Accessing the Tauri API. So that the front understands whether TAURI is available.
interface Window {
  __TAURI__?: any
}
