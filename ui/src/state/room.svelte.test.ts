import { describe, expect, it } from 'vitest';
import type { Capabilities, ClientEvent, ClientListener, LadexClient } from '../client/types';
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

  becomeHost(event: ClientEvent): void {
    this.capabilities = { ...GUEST, canManageGuests: true };
    this.listener?.(event);
  }
}

function roomWith(client: FakeClient): Room {
  return new Room(client as unknown as LadexClient, new Toasts());
}

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
});
