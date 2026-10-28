import { describe, expect, it } from 'vitest';
import { TransferError, type Capabilities, type ClientEvent, type ClientListener, type DownloadTicket, type LadexClient, type SavedDownload, type SharedFile } from '../client/types';
import { Room } from './room.svelte';
import { Toasts } from './toasts.svelte';

const GUEST: Capabilities = {
  canUpload: true,
  uploadNeedsApproval: false,
  canShareFolders: true,
  canChat: true,
  canRename: true,
  canSendToDevice: true,
  canManageGuests: false,
  canLeave: false,
};

// A client that, like the real one, only knows it is the host once the node has answered.
class FakeClient {
  capabilities: Capabilities = GUEST;
  private listener: ClientListener | null = null;

  subscribe(listener: ClientListener): () => void {
    this.listener = listener;
    return () => (this.listener = null);
  }

  saved: SavedDownload = { shownAs: '/home/me/Downloads/beach.jpg' };

  async prepareDownload(file: SharedFile): Promise<DownloadTicket> {
    return { url: '', filename: file.name, saveToDisk: async () => this.saved };
  }

  becomeHost(event: ClientEvent): void {
    this.capabilities = { ...GUEST, canManageGuests: true };
    this.listener?.(event);
  }
}

function roomWith(client: FakeClient, toasts = new Toasts()): Room {
  return new Room(client as unknown as LadexClient, toasts);
}

const BEACH = { name: 'beach.jpg' } as SharedFile;

describe('Room', () => {
  it('starts_with_what_the_client_knows_before_the_node_answers', () => {
    expect(roomWith(new FakeClient()).capabilities.canManageGuests).toBe(false);
  });

  it('picks_up_host_rights_once_the_client_reports_itself', () => {
    const client = new FakeClient();
    const room = roomWith(client);
    client.becomeHost({ type: 'self', self: { id: 'a', name: 'Phone', nodeName: 'phone' } });
    expect(room.capabilities.canManageGuests).toBe(true);
  });

  it('picks_up_host_rights_when_the_connection_changes', () => {
    const client = new FakeClient();
    const room = roomWith(client);
    client.becomeHost({ type: 'connection', state: 'online' });
    expect(room.capabilities.canManageGuests).toBe(true);
  });

  it('keeps_a_file_saved_on_a_phone_and_offers_to_open_or_share_it', async () => {
    const client = new FakeClient();
    const toasts = new Toasts();
    const room = roomWith(client, toasts);
    const handedOver: string[] = [];
    client.saved = {
      shownAs: 'Download/LADEX/beach.jpg',
      open: async () => void handedOver.push('open'),
      shareOnward: async () => void handedOver.push('share'),
    };

    await room.download(BEACH);
    toasts.items.at(-1)?.actions.forEach((action) => action.run());
    await Promise.resolve();

    expect(room.saved.map((file) => file.name)).toEqual(['beach.jpg']);
    expect(toasts.items.at(-1)?.text).toBe('Saved to Download/LADEX/beach.jpg');
    expect(handedOver).toEqual(['open', 'share']);
  });

  it('keeps_no_list_of_files_saved_where_nothing_can_hand_them_on', async () => {
    const client = new FakeClient();
    const toasts = new Toasts();
    const room = roomWith(client, toasts);

    await room.download(BEACH);

    expect(room.saved).toEqual([]);
    expect(toasts.items.at(-1)?.actions).toEqual([]);
  });

  it('says_why_a_saved_file_could_not_be_opened', async () => {
    const client = new FakeClient();
    const toasts = new Toasts();
    const room = roomWith(client, toasts);
    client.saved = {
      shownAs: 'Download/LADEX/beach.jpg',
      open: async () => {
        throw new TransferError('No app on this phone opens this kind of file');
      },
      shareOnward: async () => {},
    };
    await room.download(BEACH);

    await room.openSaved(room.saved[0]!);

    expect(toasts.items.at(-1)?.text).toBe('No app on this phone opens this kind of file');
  });
});
