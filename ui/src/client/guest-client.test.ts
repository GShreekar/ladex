import { afterEach, describe, expect, it, vi } from 'vitest';
import { GuestClient, type GuestMe } from './guest-client';
import type { ClientEvent } from './types';

class FakeEventSource {
  static instances: FakeEventSource[] = [];
  static readonly CLOSED = 2;
  readyState = 0;
  onopen: (() => void) | null = null;
  onerror: (() => void) | null = null;
  private readonly handlers = new Map<string, (event: MessageEvent<string>) => void>();

  constructor(readonly url: string) {
    FakeEventSource.instances.push(this);
  }

  addEventListener(name: string, handler: (event: MessageEvent<string>) => void): void {
    this.handlers.set(name, handler);
  }

  close(): void {
    this.readyState = FakeEventSource.CLOSED;
  }

  deliver(name: string, data: unknown): void {
    this.handlers.get(name)?.({ data: JSON.stringify(data) } as MessageEvent<string>);
  }
}

const ME: GuestMe = {
  id: 'g1',
  label: 'Safari on iPhone',
  permission: 'upload-with-approval',
  can_upload: true,
  needs_approval: true,
  expires_in_secs: 3600,
  node_id: 'node_a',
  node_name: 'Maya’s MacBook',
};

function startGuest(devices: unknown[] = []) {
  FakeEventSource.instances = [];
  vi.stubGlobal('EventSource', FakeEventSource);
  vi.stubGlobal('fetch', vi.fn(async () => new Response(JSON.stringify({ devices }))));
  const client = new GuestClient(ME);
  const events: ClientEvent[] = [];
  client.subscribe((event) => events.push(event));
  return { client, events };
}

async function settle(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('GuestClient', () => {
  it('derives what it may do from the invitation', () => {
    const { client } = startGuest();
    expect(client.capabilities).toMatchObject({ canUpload: true, uploadNeedsApproval: true, canChat: false, canShareFolders: false });
  });

  it('introduces itself with the label the host sees', async () => {
    const { client, events } = startGuest();
    await client.start();
    expect(events).toContainEqual({ type: 'self', self: { id: 'g1', name: 'Safari on iPhone', nodeName: 'Maya’s MacBook' } });
  });

  it('is online once the catalog stream opens', async () => {
    const { client, events } = startGuest();
    await client.start();
    FakeEventSource.instances[0]?.onopen?.();
    expect(events.at(-1)).toEqual({ type: 'connection', state: 'online' });
  });

  it('turns a catalog event into files named by the sharing device', async () => {
    const { client, events } = startGuest([{ id: 'peer_x', name: 'Daniel’s Laptop', node_id: 'node_a', node_name: 'Maya’s MacBook', is_here: true }]);
    await client.start();
    FakeEventSource.instances[0]?.deliver('catalog', {
      files: [
        {
          id: 'file_a',
          name: 'notes.txt',
          size: 10,
          mime_type: 'text/plain',
          uploader_id: 'peer_x',
          uploader_node: 'node_a',
          hosts: ['node_a'],
          uploaded_at: '2026-10-13T12:00:00Z',
          is_folder: false,
          parent: null,
          folder_bytes: 0,
          folder_files: 0,
        },
      ],
    });
    await settle();
    const filesEvent = events.findLast((event) => event.type === 'files');
    expect(filesEvent).toMatchObject({ files: [{ name: 'notes.txt', sharedBy: 'Daniel’s Laptop', holderNames: ['Maya’s MacBook'], isAvailable: true }] });
  });

  it('ends the session when the node says so', async () => {
    const { client, events } = startGuest();
    await client.start();
    FakeEventSource.instances[0]?.deliver('ended', {});
    expect(events.at(-1)).toEqual({ type: 'connection', state: 'ended' });
  });

  it('downloads a folder as a zip without probing it', async () => {
    const { client } = startGuest();
    const ticket = await client.prepareDownload({
      id: 'folder_1', name: 'Photos', mimeType: 'inode/directory', isFolder: true, sizeBytes: 0, folderFileCount: 3,
      sharedAt: 0, sharedBy: '', holderNames: [], isAvailable: true, canUnshare: false,
    });
    expect(ticket).toEqual({ url: '/blobs/folder_1.zip', filename: 'Photos.zip' });
  });
});
