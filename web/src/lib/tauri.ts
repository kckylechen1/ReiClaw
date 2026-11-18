// Tauri detection utilities for ReiClaw Desktop.

declare global {
  interface Window {
    __TAURI__?: unknown;
    __REICLAW_GATEWAY__?: string;
  }
}

/** Returns true when running inside a Tauri WebView. */
export const isTauri = (): boolean => '__TAURI__' in window;

/** Gateway base URL when running inside Tauri (defaults to localhost). */
export const tauriGatewayUrl = (): string =>
  window.__REICLAW_GATEWAY__ ?? 'http://127.0.0.1:42617';
