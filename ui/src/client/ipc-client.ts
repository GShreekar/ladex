import { Channel, invoke } from '@tauri-apps/api/core';
import { IpcTransport, type IpcBackend } from './ipc-transport';
import { LocalNodeClient } from './local-node-client';
import { TransferError, type Capabilities, type DownloadTicket } from './types';

export const tauriBackend: IpcBackend = {
  invoke: (command, args) => invoke(command, args),
  invokeWithBytes: (command, bytes, headers) => invoke(command, bytes, { headers }),
  openChannel: (onMessage) => new Channel(onMessage),
};

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
  private async saveToDisk(path: string, fileName: string): Promise<string> {
    try {
      return await this.backend.invoke<string>('ladex_save_download', { path, fileName });
    } catch (error) {
      throw new TransferError(String(error));
    }
  }
}
