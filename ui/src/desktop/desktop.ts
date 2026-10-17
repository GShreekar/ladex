import { invoke } from '@tauri-apps/api/core';

/** What the app's Rust side does for the person outside the window. */
export interface DesktopShell {
  notify(title: string, body: string): void;
  setOtherDevices(count: number): void;
}

/** The app's own settings, which only exist where there is an app. */
export interface DesktopSettings {
  downloadsFolder(): Promise<string | null>;
  pickFolder(title: string): Promise<string | null>;
  launchesAtLogin(): Promise<boolean>;
  setLaunchesAtLogin(shouldLaunch: boolean): Promise<boolean>;
  // Stops the node and starts it again from what is saved, then reloads the window.
  restartNode(): Promise<void>;
}

export interface SetupState {
  isNeeded: boolean;
  suggestedPassphrase: string;
  defaultDownloads: string | null;
  defaultStorageGib: number;
  freeBytes: number | null;
  dataDir: string;
}

export interface SetupChoices {
  passphrase: string;
  downloadsDir: string | null;
  storageLimitGib: number;
  launchAtLogin: boolean;
}

/** The first run, before any node exists to ask. */
export interface DesktopSetup {
  state(): Promise<SetupState>;
  suggestPassphrase(): Promise<string>;
  // Saves the choices, starts the node and reloads the window onto it.
  finish(choices: SetupChoices): Promise<void>;
}

interface WireSetupState {
  is_needed: boolean;
  suggested_passphrase: string;
  default_downloads: string | null;
  default_storage_gib: number;
  free_bytes: number | null;
  data_dir: string;
}

export const tauriDesktop: DesktopShell = {
  notify: (title, body) => void invoke('ladex_notify', { title, body }).catch(() => {}),
  setOtherDevices: (count) => void invoke('ladex_set_other_devices', { count }).catch(() => {}),
};

export const tauriSettings: DesktopSettings = {
  downloadsFolder: () => invoke<string | null>('ladex_downloads_folder'),
  pickFolder: (title) => invoke<string | null>('ladex_pick_folder', { title }),
  launchesAtLogin: () => invoke<boolean>('ladex_launch_at_login'),
  setLaunchesAtLogin: (shouldLaunch) => invoke<boolean>('ladex_set_launch_at_login', { shouldLaunch }),
  restartNode: () => invoke<void>('ladex_restart_node'),
};

export const tauriSetup: DesktopSetup = {
  async state() {
    const state = await invoke<WireSetupState>('ladex_setup_state');
    return {
      isNeeded: state.is_needed,
      suggestedPassphrase: state.suggested_passphrase,
      defaultDownloads: state.default_downloads,
      defaultStorageGib: state.default_storage_gib,
      freeBytes: state.free_bytes,
      dataDir: state.data_dir,
    };
  },
  suggestPassphrase: () => invoke<string>('ladex_suggest_passphrase'),
  async finish(choices) {
    await invoke('ladex_setup_finish', {
      choices: {
        passphrase: choices.passphrase,
        downloads_dir: choices.downloadsDir,
        storage_limit_gib: choices.storageLimitGib,
        launch_at_login: choices.launchAtLogin,
      },
    });
  },
};
