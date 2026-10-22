import type {
  Capabilities,
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
import type { AutoAccept } from './auto-accept';
import type { Notifier } from './notifier';
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
  // Copied from the client, which learns them from the node only after the page has drawn its navigation.
  capabilities = $state() as Capabilities;

  private stopListening: () => void;

  constructor(
    readonly client: LadexClient,
    private readonly toasts: Toasts,
    private readonly notifier?: Pick<Notifier, 'downloadSaved'>,
    private readonly autoAccept?: Pick<AutoAccept, 'shouldAccept'>,
  ) {
    this.capabilities = client.capabilities;
    this.stopListening = client.subscribe((event) => this.apply(event));
  }

  get otherDevices(): Device[] {
    return this.devices.filter((device) => !device.isSelf);
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
      if (ticket) await this.save(ticket, `Receiving ${ticket.filename}: your browser is saving it`);
    } catch (error) {
      this.toasts.show('error', error instanceof TransferError ? error.message : 'Could not receive that file.');
    }
  }

  async download(file: SharedFile): Promise<void> {
    try {
      const ticket = await this.client.prepareDownload(file);
      await this.save(ticket, `Downloading ${ticket.filename}: your browser is saving it`);
    } catch (error) {
      if (this.connection === 'signed-out' || this.connection === 'ended') return;
      this.toasts.show('error', error instanceof TransferError ? error.message : 'Could not download that file.');
    }
  }

  private async save(ticket: DownloadTicket, browserMessage: string): Promise<void> {
    if (!ticket.saveToDisk) {
      saveViaBrowser(ticket.url, ticket.filename);
      this.toasts.show('info', browserMessage);
      return;
    }
    this.toasts.show('info', `Saving ${ticket.filename}…`);
    const savedAt = await ticket.saveToDisk();
    this.toasts.show('success', `Saved to ${savedAt}`);
    this.notifier?.downloadSaved(ticket.filename);
  }

  // A file the person's rules accept is saved at once; any other is put to them.
  private async receive(offer: IncomingOffer): Promise<void> {
    const isAccepted = await this.autoAccept?.shouldAccept(offer, this.devices).catch(() => false);
    if (isAccepted) await this.answerOffer(offer, true);
    else this.offers.push(offer);
  }

  // An offer of a catalog file that has since been unshared could only fail; an offer can arrive before its file does.
  private dropOffersOfUnsharedFiles(files: SharedFile[]): void {
    const listed = new Set(files.map((file) => file.id));
    const wasListed = new Set(this.files.map((file) => file.id));
    const isGone = (offer: IncomingOffer) => !offer.isDirect && wasListed.has(offer.fileId) && !listed.has(offer.fileId);
    if (this.offers.some(isGone)) this.offers = this.offers.filter((offer) => !isGone(offer));
  }

  private apply(event: ClientEvent): void {
    switch (event.type) {
      case 'connection':
        this.connection = event.state;
        this.capabilities = this.client.capabilities;
        break;
      case 'self':
        this.self = event.self;
        this.capabilities = this.client.capabilities;
        break;
      case 'devices':
        this.devices = event.devices;
        break;
      case 'files':
        this.dropOffersOfUnsharedFiles(event.files);
        this.files = event.files;
        break;
      case 'messages':
        this.messages = event.messages;
        break;
      case 'offer':
        void this.receive(event.offer);
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
