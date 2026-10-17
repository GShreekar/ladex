import { runLimited } from '../client/concurrency';
import { TransferCancelled, type Device, type LadexClient, type UploadOutcome } from '../client/types';
import type { Toasts } from './toasts.svelte';

export type TransferStatus = 'sending' | 'queued' | 'done' | 'awaiting-approval' | 'offered' | 'failed' | 'cancelled';

export interface Transfer {
  id: number;
  name: string;
  sizeBytes: number;
  isFolder: boolean;
  targetName: string | null;
  status: TransferStatus;
  sentBytes: number;
  bytesPerSecond: number | null;
  startedAt: number | null;
  finishedAt: number | null;
  message: string | null;
}

const UPLOAD_CONCURRENCY = 2;
const SPEED_WINDOW_MS = 500;

/** Uploads started from this page: queued, progressing, finished or failed. */
export class Transfers {
  items = $state<Transfer[]>([]);
  private nextId = 1;
  private readonly controllers = new Map<number, AbortController>();

  constructor(
    private readonly client: LadexClient,
    private readonly toasts: Toasts,
  ) {}

  get active(): Transfer[] {
    return this.items.filter((item) => item.status === 'sending' || item.status === 'queued');
  }

  get finished(): Transfer[] {
    return this.items.filter((item) => item.status !== 'sending' && item.status !== 'queued');
  }

  get hasActive(): boolean {
    return this.active.length > 0;
  }

  async share(files: File[]): Promise<void> {
    const entries = files.map((file) => this.enqueue(file.name, file.size, false, null));
    let sharedCount = 0;
    await runLimited(files, UPLOAD_CONCURRENCY, async (file, index) => {
      const entry = entries[index] as Transfer;
      const onAnswered = (isAccepted: boolean) => this.answered(entry, isAccepted);
      const outcome = await this.run(entry, (signal, onProgress) => this.client.uploadFile(file, { signal, onProgress, onAnswered }));
      if (outcome === 'shared') sharedCount++;
    });
    if (sharedCount > 0) this.toasts.show('success', `${sharedCount} file${sharedCount === 1 ? '' : 's'} shared`);
  }

  async shareFolder(files: File[]): Promise<void> {
    if (files.length === 0) return;
    const name = files[0]?.webkitRelativePath.split('/')[0] || 'folder';
    const totalBytes = files.reduce((sum, file) => sum + file.size, 0);
    const entry = this.enqueue(name, totalBytes, true, null);
    const outcome = await this.run(entry, (signal, onProgress) => this.client.uploadFolder(files, { signal, onProgress }));
    if (outcome === 'shared') this.toasts.show('success', `Folder shared: ${name} (${files.length} file${files.length === 1 ? '' : 's'})`);
  }

  async sendTo(files: File[], device: Device): Promise<void> {
    const entries = files.map((file) => this.enqueue(file.name, file.size, false, device.name));
    await runLimited(files, UPLOAD_CONCURRENCY, async (file, index) => {
      const entry = entries[index] as Transfer;
      const outcome = await this.run(entry, (signal, onProgress) =>
        this.client.uploadFile(file, { signal, onProgress, toDeviceId: device.id }),
      );
      if (outcome === 'offered') this.toasts.show('info', `Waiting for ${device.name} to accept "${file.name}"`);
    });
  }

  cancel(id: number): void {
    this.controllers.get(id)?.abort();
    const entry = this.items.find((item) => item.id === id);
    if (entry?.status === 'queued') this.finish(entry, 'cancelled', null);
  }

  clearFinished(): void {
    this.items = this.items.filter((item) => item.status === 'sending' || item.status === 'queued');
  }

  private enqueue(name: string, sizeBytes: number, isFolder: boolean, targetName: string | null): Transfer {
    const entry: Transfer = {
      id: this.nextId++,
      name,
      sizeBytes,
      isFolder,
      targetName,
      status: 'queued',
      sentBytes: 0,
      bytesPerSecond: null,
      startedAt: null,
      finishedAt: null,
      message: null,
    };
    this.items.unshift(entry);
    // Reading the proxied copy keeps later writes visible to the page.
    return this.items[0] as Transfer;
  }

  private answered(entry: Transfer, isAccepted: boolean): void {
    if (entry.status !== 'awaiting-approval') return;
    if (isAccepted) {
      this.finish(entry, 'done', null);
      this.toasts.show('success', `The host accepted ${entry.name}`);
    } else {
      this.finish(entry, 'failed', 'The host declined it');
      this.toasts.show('warning', `The host declined ${entry.name}`);
    }
  }

  private finish(entry: Transfer, status: TransferStatus, message: string | null): void {
    entry.status = status;
    entry.message = message;
    entry.finishedAt = Date.now();
    this.controllers.delete(entry.id);
  }

  private async run(
    entry: Transfer,
    start: (signal: AbortSignal, onProgress: (bytesSent: number) => void) => Promise<UploadOutcome>,
  ): Promise<UploadOutcome | null> {
    if (entry.status === 'cancelled') return null;
    const controller = new AbortController();
    this.controllers.set(entry.id, controller);
    entry.status = 'sending';
    entry.startedAt = Date.now();
    let lastSampleAt = entry.startedAt;

    const onProgress = (bytesSent: number): void => {
      const now = Date.now();
      entry.sentBytes = bytesSent;
      if (now - lastSampleAt < SPEED_WINDOW_MS && bytesSent < entry.sizeBytes) return;
      lastSampleAt = now;
      entry.bytesPerSecond = bytesSent / Math.max(0.001, (now - (entry.startedAt ?? now)) / 1000);
    };

    try {
      const outcome = await start(controller.signal, onProgress);
      entry.sentBytes = entry.sizeBytes;
      this.finish(entry, outcome === 'shared' ? 'done' : outcome, null);
      return outcome;
    } catch (error) {
      if (error instanceof TransferCancelled) {
        this.finish(entry, 'cancelled', null);
        return null;
      }
      const reason = error instanceof Error ? error.message : 'the transfer failed';
      this.finish(entry, 'failed', reason);
      this.toasts.show('error', `Could not share ${entry.name}: ${reason}`);
      return null;
    }
  }
}
