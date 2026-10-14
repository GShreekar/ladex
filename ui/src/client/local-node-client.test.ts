import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { LocalNodeClient } from './local-node-client';
import type { ClientEvent, SharedFile } from './types';
import type { ServerMessage, WireFile } from './wire';

class FakeSocket {
  static instances: FakeSocket[] = [];
  static readonly OPEN = 1;
  readyState = FakeSocket.OPEN;
  sent: Array<Record<string, unknown>> = [];
  onopen: (() => void) | null = null;
  onmessage: ((event: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;

  constructor(readonly url: string) {
    FakeSocket.instances.push(this);
  }

  send(text: string): void {
    this.sent.push(JSON.parse(text) as Record<string, unknown>);
  }

  close(): void {}

  deliver(message: ServerMessage): void {
    this.onmessage?.({ data: JSON.stringify(message) });
  }
}

function wireFile(overrides: Partial<WireFile> = {}): WireFile {
  return {
    id: 'file_a',
    name: 'slides.pdf',
    size: 1000,
    mime_type: 'application/pdf',
    uploader_id: 'peer_other',
    uploader_node: 'node_b',
    hosts: ['node_a', 'node_b'],
    uploaded_at: '2026-10-13T12:00:00Z',
    is_folder: false,
    parent: null,
    folder_bytes: 0,
    folder_files: 0,
    ...overrides,
  };
}

async function startClient(isHost = false): Promise<{ client: LocalNodeClient; socket: FakeSocket; events: ClientEvent[] }> {
  vi.stubGlobal('WebSocket', FakeSocket);
  vi.stubGlobal(
    'fetch',
    vi.fn(async () => new Response(JSON.stringify({ authenticated: true, auth_required: false, node_id: 'node_a', is_host: isHost }))),
  );
  const client = new LocalNodeClient();
  const events: ClientEvent[] = [];
  client.subscribe((event) => events.push(event));
  await client.start();
  const socket = FakeSocket.instances.at(-1) as FakeSocket;
  socket.onopen?.();
  return { client, socket, events };
}

function lastOf<T extends ClientEvent['type']>(events: ClientEvent[], type: T): Extract<ClientEvent, { type: T }> {
  return events.findLast((event) => event.type === type) as Extract<ClientEvent, { type: T }>;
}

beforeEach(() => {
  FakeSocket.instances = [];
  sessionStorage.clear();
  localStorage.clear();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('LocalNodeClient', () => {
  it('announces itself to the node when the socket opens', async () => {
    const { socket } = await startClient();
    expect(socket.sent[0]).toMatchObject({ type: 'join', nickname: null });
  });

  it('reports a node that needs a login as signed out and opens no socket', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response(JSON.stringify({ authenticated: false, auth_required: true, node_id: 'n', is_host: false }))));
    const client = new LocalNodeClient();
    const events: ClientEvent[] = [];
    client.subscribe((event) => events.push(event));
    await client.start();
    expect(lastOf(events, 'connection').state).toBe('signed-out');
    expect(FakeSocket.instances).toHaveLength(0);
  });

  it('lists other devices after this one, named from nickname or browser', async () => {
    const { socket, events } = await startClient();
    socket.deliver({
      type: 'peer_joined',
      peer: { session_id: 'peer_x', nickname: 'Priya’s Pixel', hosting_node_id: 'node_b', hosting_node_name: 'pixel' },
      total_peers: 2,
    });
    const { devices } = lastOf(events, 'devices');
    expect(devices.map((device) => [device.name, device.isSelf])).toEqual([
      ['This device', true],
      ['Priya’s Pixel', false],
    ]);
  });

  it('drops a device that left', async () => {
    const { socket, events } = await startClient();
    socket.deliver({ type: 'peer_joined', peer: { session_id: 'peer_x', hosting_node_id: 'n', hosting_node_name: 'n' }, total_peers: 2 });
    socket.deliver({ type: 'peer_left', session_id: 'peer_x', total_peers: 1 });
    expect(lastOf(events, 'devices').devices).toHaveLength(1);
  });

  it('shows a file with the names of the nodes that hold it', async () => {
    const { socket, events } = await startClient();
    socket.deliver({ type: 'peer_joined', peer: { session_id: 'peer_x', hosting_node_id: 'node_b', hosting_node_name: 'Daniel’s laptop' }, total_peers: 2 });
    socket.deliver({ type: 'file_list_update', files: [wireFile()] });
    const [file] = lastOf(events, 'files').files as [SharedFile];
    expect(file.holderNames).toEqual(['node node_a', 'Daniel’s laptop']);
  });

  it('marks a file with no holders as unavailable', async () => {
    const { socket, events } = await startClient();
    socket.deliver({ type: 'file_list_update', files: [wireFile({ hosts: [] })] });
    expect(lastOf(events, 'files').files[0]?.isAvailable).toBe(false);
  });

  it('hides files inside a folder from the top-level list', async () => {
    const { socket, events } = await startClient();
    socket.deliver({ type: 'file_list_update', files: [wireFile({ id: 'file_child', parent: 'folder_1' })] });
    expect(lastOf(events, 'files').files).toEqual([]);
  });

  it('lets the machine running the node unshare any file it published', async () => {
    const { socket, events } = await startClient(true);
    socket.deliver({ type: 'file_list_update', files: [wireFile({ uploader_node: 'node_a' })] });
    expect(lastOf(events, 'files').files[0]?.canUnshare).toBe(true);
  });

  it('does not let another device unshare someone else’s file', async () => {
    const { socket, events } = await startClient(false);
    socket.deliver({ type: 'file_list_update', files: [wireFile({ uploader_node: 'node_a' })] });
    expect(lastOf(events, 'files').files[0]?.canUnshare).toBe(false);
  });

  it('sizes a folder by what is inside it', async () => {
    const { socket, events } = await startClient();
    socket.deliver({ type: 'file_list_update', files: [wireFile({ is_folder: true, size: 90, folder_bytes: 5000, folder_files: 3 })] });
    expect(lastOf(events, 'files').files[0]).toMatchObject({ sizeBytes: 5000, folderFileCount: 3 });
  });

  it('raises an offer for a file sent to this device alone', async () => {
    const { socket, events } = await startClient();
    socket.deliver({
      type: 'incoming_file_offer',
      file_id: 'file_p',
      from_session_id: 'peer_x',
      pushed: { name: 'holiday.jpg', size: 2048 },
    });
    expect(lastOf(events, 'offer').offer).toMatchObject({ fileName: 'holiday.jpg', sizeBytes: 2048, isDirect: true });
  });

  it('tells the person when a device declines a file they sent', async () => {
    const { socket, events } = await startClient();
    socket.deliver({ type: 'file_offer_declined', file_id: 'file_p', from_session_id: 'peer_x' });
    expect(lastOf(events, 'notice').notice.level).toBe('warning');
  });

  it('sends a trimmed chat message and ignores an empty one', async () => {
    const { client, socket } = await startClient();
    client.sendMessage('  hello  ');
    client.sendMessage('   ');
    const chat = socket.sent.filter((message) => message.type === 'text_message');
    expect(chat).toEqual([expect.objectContaining({ content: 'hello' })]);
  });

  it('shares a new nickname with the node and remembers it', async () => {
    const { client, socket } = await startClient();
    client.rename('  Maya’s MacBook ');
    expect(socket.sent.at(-1)).toMatchObject({ type: 'set_nickname', nickname: 'Maya’s MacBook' });
    expect(localStorage.getItem('ladex_nickname')).toBe('Maya’s MacBook');
  });

  it('shows the no-peers warning as its own event', async () => {
    const { socket, events } = await startClient();
    socket.deliver({ type: 'no_peers_warning', message: 'none' });
    expect(events.some((event) => event.type === 'no-peers')).toBe(true);
  });
});
