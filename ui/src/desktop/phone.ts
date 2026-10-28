import { Channel, invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

/** A file another app shared into LADEX, waiting to be shared with everyone. */
export interface IncomingFile {
  id: string;
  name: string;
  // Some apps do not say until the file is opened.
  size: number | null;
  mimeType: string;
}

export interface ShareProgress {
  done: number;
  total: number;
}

/** What only the Android app offers: files shared in from other apps, and the folder received files are saved in. */
export interface PhoneShell {
  incomingFiles(): Promise<IncomingFile[]>;
  // Resolves to a function that stops listening.
  onIncomingFiles(listener: (files: IncomingFile[]) => void): Promise<() => void>;
  // Resolves to how many files were shared; they are set aside either way.
  shareIncoming(onProgress: (progress: ShareProgress) => void): Promise<number>;
  setIncomingAside(): Promise<void>;
  saveFolder(): Promise<string | null>;
  // Shows Android's folder picker; resolves to the folder chosen now, the old one if none was picked.
  pickSaveFolder(): Promise<string | null>;
  forgetSaveFolder(): Promise<void>;
}

const INCOMING_EVENT = 'ladex-incoming-files';

export const tauriPhone: PhoneShell = {
  incomingFiles: () => invoke<IncomingFile[]>('ladex_incoming_files'),
  onIncomingFiles: (listener) => listen<IncomingFile[]>(INCOMING_EVENT, (event) => listener(event.payload)),
  shareIncoming: (onProgress) => invoke<number>('ladex_share_incoming', { onProgress: new Channel<ShareProgress>(onProgress) }),
  setIncomingAside: () => invoke<void>('ladex_set_incoming_aside'),
  saveFolder: () => invoke<string | null>('ladex_save_folder'),
  pickSaveFolder: () => invoke<string | null>('ladex_pick_save_folder'),
  forgetSaveFolder: () => invoke<void>('ladex_forget_save_folder'),
};
