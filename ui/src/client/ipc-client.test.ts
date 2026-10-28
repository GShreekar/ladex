import { describe, expect, it } from 'vitest';
import { IpcClient } from './ipc-client';
import type { IpcBackend, SocketEvent } from './ipc-transport';
import type { ClientEvent, SharedFile } from './types';
import type { ServerMessage } from './wire';

interface Call {
  command: string;
  args?: Record<string, unknown>;
  bytes?: Uint8Array;
  headers?: Record<string, string>;
}

class FakeBackend implements IpcBackend {
  calls: Call[] = [];
  replies = new Map<string, unknown | ((args?: Record<string, unknown>) => unknown)>();
  private emit: ((event: SocketEvent) => void) | null = null;

  async invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
    this.calls.push({ command, args });
    const reply = this.replies.get(command);
    if (reply instanceof Error) throw reply;
    return (typeof reply === 'function' ? reply(args) : reply) as T;
  }

  async invokeWithBytes<T>(command: string, bytes: Uint8Array, headers: Record<string, string>): Promise<T> {
    this.calls.push({ command, bytes, headers });
    const reply = this.replies.get(command);
    if (reply instanceof Error) throw reply;
    return reply as T;
  }

  openChannel<T>(onMessage: (message: T) => void): unknown {
    this.emit = onMessage as (event: SocketEvent) => void;
    return 'channel';
  }

  deliver(message: ServerMessage): void {
    this.emit?.({ kind: 'message', text: JSON.stringify(message) });
  }

  close(): void {
    this.emit?.({ kind: 'closed' });
  }

  sent(command: string): Call[] {
    return this.calls.filter((call) => call.command === command);
  }
}

const NODE_STATUS = { status: 200, body: JSON.stringify({ authenticated: true, auth_required: true, node_id: 'node_a', is_host: true }) };

async function startClient(): Promise<{ client: IpcClient; backend: FakeBackend; events: ClientEvent[] }> {
  const backend = new FakeBackend();
  backend.replies.set('ladex_request', NODE_STATUS);
  backend.replies.set('ladex_socket_open', 7);
  const client = new IpcClient(backend);
  const events: ClientEvent[] = [];
  client.subscribe((event) => events.push(event));
  await client.start();
  await Promise.resolve();
  return { client, backend, events };
}

function lastOf<T extends ClientEvent['type']>(events: ClientEvent[], type: T): Extract<ClientEvent, { type: T }> {
  return events.findLast((event) => event.type === type) as Extract<ClientEvent, { type: T }>;
}

const SHARED_FILE: SharedFile = {
  id: 'file_a',
  name: 'slides.pdf',
  mimeType: 'application/pdf',
  isFolder: false,
  sizeBytes: 10,
  folderFileCount: 0,
  sharedAt: 0,
  sharedBy: 'You',
  isOwn: true,
  holderNames: ['here'],
  isAvailable: true,
  canUnshare: true,
};

describe('IpcClient', () => {
  it('asks the node who it is and opens the room over the app, with no login', async () => {
    const { backend, events } = await startClient();
    expect(backend.sent('ladex_request')[0]?.args).toMatchObject({ method: 'GET', path: '/auth-status' });
    expect(lastOf(events, 'connection').state).toBe('online');
  });

  it('joins the room as soon as the socket is open', async () => {
    const { backend } = await startClient();
    const [join] = backend.sent('ladex_socket_send');
    expect(join?.args).toMatchObject({ socketId: 7 });
    expect(JSON.parse(join?.args?.text as string)).toMatchObject({ type: 'join' });
  });

  it('turns messages from the node into room events', async () => {
    const { backend, events } = await startClient();
    backend.deliver({ type: 'peer_joined', peer: { session_id: 'peer_x', nickname: 'Pixel', hosting_node_id: 'node_b' }, total_peers: 2 });
    expect(lastOf(events, 'devices').devices.map((device) => device.name)).toEqual(['This device', 'Pixel']);
  });

  it('lets the owner manage guests but never offers to sign out', async () => {
    const { client } = await startClient();
    expect(client.capabilities).toMatchObject({ canManageGuests: true, canLeave: false });
  });

  it('reports the node as offline when the socket closes', async () => {
    const { client, backend, events } = await startClient();
    backend.close();
    expect(lastOf(events, 'connection').state).toBe('offline');
    client.stop();
  });

  it('saves a download itself, asking the app to write it to disk', async () => {
    const { client, backend } = await startClient();
    backend.replies.set('ladex_request', { status: 200, body: '' });
    backend.replies.set('ladex_save_download', { shownAs: '/home/me/Downloads/slides.pdf', savedId: null });

    const ticket = await client.prepareDownload(SHARED_FILE);
    const saved = await ticket.saveToDisk?.();

    expect(saved).toEqual({ shownAs: '/home/me/Downloads/slides.pdf' });
    expect(backend.sent('ladex_save_download')[0]?.args).toEqual({ path: '/api/files/file_a', fileName: 'slides.pdf' });
  });

  it('lets a file saved on a phone be opened or shared onward by the id the app gave it', async () => {
    const { client, backend } = await startClient();
    backend.replies.set('ladex_request', { status: 200, body: '' });
    backend.replies.set('ladex_save_download', { shownAs: 'Download/LADEX/slides.pdf', savedId: 3 });

    const saved = await (await client.prepareDownload(SHARED_FILE)).saveToDisk?.();
    await saved?.open?.();
    await saved?.shareOnward?.();

    expect(backend.sent('ladex_open_saved')[0]?.args).toEqual({ savedId: 3 });
    expect(backend.sent('ladex_share_saved')[0]?.args).toEqual({ savedId: 3 });
  });

  it('reports why another app could not be handed a saved file', async () => {
    const { client, backend } = await startClient();
    backend.replies.set('ladex_request', { status: 200, body: '' });
    backend.replies.set('ladex_save_download', { shownAs: 'Download/LADEX/slides.pdf', savedId: 0 });
    backend.replies.set('ladex_open_saved', new Error('No app on this phone opens this kind of file'));

    const saved = await (await client.prepareDownload(SHARED_FILE)).saveToDisk?.();

    await expect(saved?.open?.()).rejects.toThrow('No app on this phone opens');
  });

  it('reports why a file could not be written to disk', async () => {
    const { client, backend } = await startClient();
    backend.replies.set('ladex_request', { status: 200, body: '' });
    backend.replies.set('ladex_save_download', new Error('could not save the file: No space left on device'));

    const ticket = await client.prepareDownload(SHARED_FILE);
    await expect(ticket.saveToDisk?.()).rejects.toThrow('No space left on device');
  });

  it('refuses a download the node says is gone, before saving anything', async () => {
    const { client, backend } = await startClient();
    backend.replies.set('ladex_request', { status: 404, body: '' });
    await expect(client.prepareDownload(SHARED_FILE)).rejects.toThrow('removed');
    expect(backend.sent('ladex_save_download')).toHaveLength(0);
  });

  it('uploads a file in pieces and reads the node’s answer', async () => {
    const { client, backend } = await startClient();
    backend.replies.set('ladex_upload_begin', 3);
    backend.replies.set('ladex_upload_write', null);
    backend.replies.set('ladex_upload_finish', { status: 201, body: JSON.stringify({ success: true, complete: true, file: { id: 'x' } }) });
    backend.replies.set('ladex_request', { status: 200, body: JSON.stringify({ offset: 0, complete: false }) });

    const file = new File([new Uint8Array(5 * 1024 * 1024)], 'big.bin');
    const progress: number[] = [];
    const outcome = await client.uploadFile(file, { signal: new AbortController().signal, onProgress: (sent) => progress.push(sent) });

    expect(outcome).toBe('shared');
    expect(backend.sent('ladex_upload_begin')[0]?.args).toMatchObject({ contentLength: 5 * 1024 * 1024 });
    expect(backend.sent('ladex_upload_write').map((call) => call.bytes?.length)).toEqual([4 * 1024 * 1024, 1024 * 1024]);
    expect(backend.sent('ladex_upload_write')[0]?.headers).toEqual({ 'x-upload-id': '3' });
    expect(progress.at(-1)).toBe(5 * 1024 * 1024);
  });

  it('stops an upload when the node refuses it and reports the node’s reason', async () => {
    const { client, backend } = await startClient();
    backend.replies.set('ladex_upload_begin', 4);
    backend.replies.set('ladex_upload_write', new Error('the node stopped reading the upload'));
    backend.replies.set('ladex_upload_finish', { status: 403, body: JSON.stringify({ message: 'that device is not connected here' }) });
    backend.replies.set('ladex_request', { status: 200, body: JSON.stringify({ offset: 0, complete: false }) });

    const file = new File([new Uint8Array(1024)], 'a.bin');
    const upload = client.uploadFile(file, { signal: new AbortController().signal, onProgress: () => {} });
    await expect(upload).rejects.toThrow('that device is not connected here');
  });
});
