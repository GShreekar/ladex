import type { IncomingFile, PhoneShell, ShareProgress } from '../desktop/phone';
import type { Toasts } from '../state/toasts.svelte';

function capitalized(text: string): string {
  return text.charAt(0).toUpperCase() + text.slice(1);
}

/** Files another app shared into LADEX, offered to the person until they share them or set them aside. */
export class IncomingShare {
  files = $state<IncomingFile[]>([]);
  // Set while the files are being shared.
  progress = $state<ShareProgress | null>(null);
  private stopListening: (() => void) | null = null;
  private isStopped = false;

  constructor(
    private readonly phone: PhoneShell,
    private readonly toasts: Pick<Toasts, 'show'>,
  ) {}

  get isSharing(): boolean {
    return this.progress !== null;
  }

  // What is known of the size; some apps only tell once the file is opened.
  get knownBytes(): number {
    return this.files.reduce((sum, file) => sum + (file.size ?? 0), 0);
  }

  get hasUnknownSizes(): boolean {
    return this.files.some((file) => file.size === null);
  }

  // Listening starts first, so files shared while the list is fetched are not missed.
  async start(): Promise<void> {
    const unlisten = await this.phone.onIncomingFiles((files) => {
      if (!this.isSharing) this.files = files;
    });
    if (this.isStopped) return unlisten();
    this.stopListening = unlisten;
    this.files = await this.phone.incomingFiles();
  }

  stop(): void {
    this.isStopped = true;
    this.stopListening?.();
  }

  async share(): Promise<void> {
    const named = this.files.length === 1 ? (this.files[0]?.name ?? 'the file') : `${this.files.length} files`;
    this.progress = { done: 0, total: this.knownBytes };
    try {
      const count = await this.phone.shareIncoming((progress) => (this.progress = progress));
      this.toasts.show('success', count === 1 ? `Shared ${named} with everyone` : `Shared ${count} files with everyone`);
    } catch (error) {
      // The app says which file failed and why.
      this.toasts.show('error', capitalized(String(error)));
    } finally {
      this.progress = null;
      this.files = [];
    }
  }

  async setAside(): Promise<void> {
    if (this.isSharing) return;
    this.files = [];
    await this.phone.setIncomingAside();
  }
}
