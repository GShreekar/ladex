import { invoke } from '@tauri-apps/api/core';

/** What the app's Rust side does for the person outside the window. */
export interface DesktopShell {
  notify(title: string, body: string): void;
  setOtherDevices(count: number): void;
}

export const tauriDesktop: DesktopShell = {
  notify: (title, body) => void invoke('ladex_notify', { title, body }).catch(() => {}),
  setOtherDevices: (count) => void invoke('ladex_set_other_devices', { count }).catch(() => {}),
};
