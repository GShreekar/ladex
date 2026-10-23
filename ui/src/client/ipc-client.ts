import { Channel, invoke } from '@tauri-apps/api/core';
import { IpcTransport, type IpcBackend } from './ipc-transport';
import { LocalNodeClient } from './local-node-client';
import { TransferError, type Capabilities, type DownloadTicket, type SavedDownload } from './types';

export const tauriBackend: IpcBackend = {
  invoke: (command, args) => invoke(command, args),
  invokeWithBytes: (command, bytes, headers) => invoke(command, bytes, { headers }),
  openChannel: (onMessage) => new Channel(onMessage),
};

interface WireSavedDownload {
  shownAs: string;
  savedId: number | null;
}

/** The app's window talking to the node it embeds, over the app's commands instead of HTTP. */
export class IpcClient extends LocalNodeClient {
  constructor(private readonly backend: IpcBackend = tauriBackend) {
    super(new IpcTransport(backend));
  }

  override get capabilities(): Capabilities {
    // The window belongs to the node's owner, so there is nothing to sign out of.
    return { ...super.capabilities, canLeave: false };
  }

  protected override ticket(path: string, filename: string): DownloadTicket {
    return {
      url: path,
      filename,
      saveToDisk: () => this.saveToDisk(path, filename),
    };
  }

  // The app reports a failure as plain text, which is already fit for the person to read.
  private async saveToDisk(path: string, fileName: string): Promise<SavedDownload> {
    let saved: WireSavedDownload;
    try {
      saved = await this.backend.invoke<WireSavedDownload>('ladex_save_download', { path, fileName });
    } catch (error) {
      throw new TransferError(String(error));
    }
    const savedId = saved.savedId;
    if (savedId === null) return { shownAs: saved.shownAs };
    return {
      shownAs: saved.shownAs,
      open: () => this.handOver('ladex_open_saved', savedId),
      shareOnward: () => this.handOver('ladex_share_saved', savedId),
    };
  }

  private async handOver(command: string, savedId: number): Promise<void> {
    try {
      await this.backend.invoke(command, { savedId });
    } catch (error) {
      throw new TransferError(String(error));
    }
  }
}
