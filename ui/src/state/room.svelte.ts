import type {
  ChatMessage,
  ClientEvent,
  ConnectionState,
  Device,
  DownloadTicket,
  IncomingOffer,
  LadexClient,
  SelfInfo,
  SharedFile,
} from '../client/types';
import { TransferError } from '../client/types';
import { saveViaBrowser } from './save';
import type { Toasts } from './toasts.svelte';

/** What this page knows about the room, kept current from the client's events. */
export class Room {
  connection = $state<ConnectionState>('connecting');
  self = $state<SelfInfo>({ id: '', name: 'This device', nodeName: '' });
  devices = $state<Device[]>([]);
  files = $state<SharedFile[]>([]);
  messages = $state<ChatMessage[]>([]);
  offers = $state<IncomingOffer[]>([]);
  hasNoPeersWarning = $state(false);

  private stopListening: () => void;

  constructor(
    readonly client: LadexClient,
    private readonly toasts: Toasts,
  ) {
    this.stopListening = client.subscribe((event) => this.apply(event));
  }

  get otherDevices(): Device[] {
    return this.devices.filter((device) => !device.isSelf);
  }

  get capabilities(): LadexClient['capabilities'] {
    return this.client.capabilities;
  }

  async start(): Promise<void> {
    await this.client.start();
  }

  dispose(): void {
    this.stopListening();
    this.client.stop();
  }

  dismissNoPeersWarning(): void {
    this.hasNoPeersWarning = false;
  }

  async answerOffer(offer: IncomingOffer, accepted: boolean): Promise<void> {
    this.offers = this.offers.filter((pending) => pending !== offer);
    try {
      const ticket = await this.client.answerOffer(offer, accepted);
      if (ticket) this.save(ticket, `Receiving ${ticket.filename}: your browser is saving it`);
    } catch (error) {
      this.toasts.show('error', error instanceof TransferError ? error.message : 'Could not receive that file.');
    }
  }

  async download(file: SharedFile): Promise<void> {
    try {
      const ticket = await this.client.prepareDownload(file);
      this.save(ticket, `Downloading ${ticket.filename}: your browser is saving it`);
    } catch (error) {
      if (this.connection === 'signed-out' || this.connection === 'ended') return;
      this.toasts.show('error', error instanceof TransferError ? error.message : 'Could not download that file.');
    }
  }

  private save(ticket: DownloadTicket, message: string): void {
    saveViaBrowser(ticket.url, ticket.filename);
    this.toasts.show('info', message);
  }

  private apply(event: ClientEvent): void {
    switch (event.type) {
      case 'connection':
        this.connection = event.state;
        break;
      case 'self':
        this.self = event.self;
        break;
      case 'devices':
        this.devices = event.devices;
        break;
      case 'files':
        this.files = event.files;
        break;
      case 'messages':
        this.messages = event.messages;
        break;
      case 'offer':
        this.offers.push(event.offer);
        break;
      case 'no-peers':
        this.hasNoPeersWarning = true;
        break;
      case 'notice':
        this.toasts.show(event.notice.level, event.notice.text);
        break;
    }
  }
}
