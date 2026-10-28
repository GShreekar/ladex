import { runLimited, sleep } from './concurrency';
import { deviceKindOf, friendlyDeviceName, randomId } from './format';
import { HttpTransport } from './http-transport';
import { runResumableUpload, SignedOut, type UploadReply, type UploadWire } from './resumable-upload';
import {
  TransferError,
  type Capabilities,
  type ChatMessage,
  type ClientEvent,
  type ClientListener,
  type ConnectionState,
  type Device,
  type DownloadTicket,
  type FileUploadRequest,
  type IncomingOffer,
  type LadexClient,
  type NoticeLevel,
  type SelfInfo,
  type SharedFile,
  type UploadOutcome,
  type UploadRequest,
} from './types';
import { parseUploadReply, type NodeSocket, type NodeTransport } from './transport';
import type { AuthStatus, ServerMessage, WireFile, WireMessage, WirePeer } from './wire';

const SESSION_KEY = 'ladex_session';
const NICKNAME_KEY = 'ladex_nickname';
const NICKNAME_MAX_CHARS = 40;
const RECONNECT_DELAY_MS = 3000;
const FOLDER_CONCURRENCY = 3;
const MAX_FOLDER_FILES = 100_000;
const ACCEPT_POLL_ATTEMPTS = 10;
const ACCEPT_POLL_DELAY_MS = 300;

function loadSessionId(): string {
  try {
    const saved = sessionStorage.getItem(SESSION_KEY);
    if (saved) return saved;
    const created = randomId('peer_', 10);
    sessionStorage.setItem(SESSION_KEY, created);
    return created;
  } catch {
    return randomId('peer_', 10);
  }
}

function loadNickname(): string {
  try {
    return localStorage.getItem(NICKNAME_KEY) ?? '';
  } catch {
    return '';
  }
}

export function saveNickname(nickname: string): void {
  try {
    localStorage.setItem(NICKNAME_KEY, nickname);
  } catch {
    // The name still applies for this visit.
  }
}

function isSuccess(status: number): boolean {
  return status >= 200 && status < 300;
}

function describeProblem(status: number): string {
  if (status === 404) return 'That file has been removed.';
  if (status === 503) return 'No device that has this file is online right now.';
  if (status === 507) return 'This node does not have room to fetch that file.';
  return `The node could not provide that file (error ${status}).`;
}

interface SendOptions {
  parent?: string;
  to?: string;
  signal: AbortSignal;
  onProgress: (bytesSent: number) => void;
}

/** A node reached through a transport: from a browser on the same machine or LAN over HTTP, signed in with the node's passphrase. */
export class LocalNodeClient implements LadexClient {
  private readonly listeners = new Set<ClientListener>();
  private readonly peers = new Map<string, WirePeer>();
  private readonly nodeNames = new Map<string, string>();
  // Names of devices seen, kept after they leave so what they shared stays credited to them.
  private readonly deviceNames = new Map<string, string>();
  // Files that just left the catalog, so the notice that follows can name them.
  private readonly removedFiles = new Map<string, WireFile>();
  private readonly pendingUnshares = new Map<string, string>();
  private readonly sentFiles = new Map<string, string>();
  private readonly sessionId = loadSessionId();
  private nickname = loadNickname();
  private files: WireFile[] = [];
  private messages: WireMessage[] = [];
  private status: AuthStatus = { authenticated: false, auth_required: false, node_id: '', is_host: false };
  private socket: NodeSocket | null = null;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private isStopped = false;

  constructor(readonly transport: NodeTransport = new HttpTransport()) {}

  get capabilities(): Capabilities {
    return {
      canUpload: true,
      uploadNeedsApproval: false,
      canShareFolders: true,
      canChat: true,
      canRename: true,
      canSendToDevice: true,
      canManageGuests: this.status.is_host,
      canLeave: this.status.auth_required,
    };
  }

  subscribe(listener: ClientListener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  async start(): Promise<void> {
    this.isStopped = false;
    this.emit({ type: 'connection', state: 'connecting' });
    try {
      this.status = await this.fetchStatus();
    } catch {
      this.emit({ type: 'connection', state: 'offline' });
      this.scheduleReconnect();
      return;
    }
    if (this.status.auth_required && !this.status.authenticated) {
      this.emit({ type: 'connection', state: 'signed-out' });
      return;
    }
    this.publishSelf();
    this.openSocket();
  }

  stop(): void {
    this.isStopped = true;
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.socket?.close();
    this.socket = null;
  }

  async uploadFile(file: File, request: FileUploadRequest): Promise<UploadOutcome> {
    const fileId = randomId('file_', 12);
    await this.sendFile(file, fileId, { signal: request.signal, onProgress: request.onProgress, to: request.toDeviceId });
    if (!request.toDeviceId) return 'shared';
    this.sentFiles.set(fileId, file.name);
    return 'offered';
  }

  async uploadFolder(files: File[], request: UploadRequest): Promise<UploadOutcome> {
    if (files.length > MAX_FOLDER_FILES) throw new TransferError(`a folder can hold at most ${MAX_FOLDER_FILES} files`);
    const folderId = randomId('file_', 12);
    const folderName = files[0]?.webkitRelativePath.split('/')[0] || 'folder';
    const sentBytes = new Array<number>(files.length).fill(0);
    const children: Array<{ path: string; file_id: string }> = [];

    try {
      await runLimited(files, FOLDER_CONCURRENCY, async (file, index) => {
        const fileId = randomId('file_', 12);
        await this.sendFile(file, fileId, {
          parent: folderId,
          signal: request.signal,
          onProgress: (bytes) => {
            sentBytes[index] = bytes;
            request.onProgress(sentBytes.reduce((sum, value) => sum + value, 0));
          },
        });
        children.push({ path: file.webkitRelativePath || file.name, file_id: fileId });
      });
      await this.publishFolder(folderId, folderName, children);
    } catch (error) {
      // Files that did arrive stay hidden but take up room, so they are removed with the folder that never was.
      for (const child of children) this.send({ type: 'delete_file', session_id: this.sessionId, file_id: child.file_id });
      throw error;
    }
    return 'shared';
  }

  async prepareDownload(file: SharedFile): Promise<DownloadTicket> {
    if (file.isFolder) {
      return this.ticket(`/api/folders/${encodeURIComponent(file.id)}.zip`, `${file.name}.zip`);
    }
    const url = `/api/files/${encodeURIComponent(file.id)}`;
    await this.probe(url);
    return this.ticket(url, file.name);
  }

  unshare(file: SharedFile): void {
    this.pendingUnshares.set(file.id, file.name);
    this.send({ type: 'delete_file', session_id: this.sessionId, file_id: file.id });
  }

  sendMessage(text: string): void {
    const content = text.trim();
    if (content) this.send({ type: 'text_message', session_id: this.sessionId, content });
  }

  rename(nickname: string): void {
    this.nickname = nickname.trim().slice(0, NICKNAME_MAX_CHARS);
    saveNickname(this.nickname);
    if (this.nickname) this.send({ type: 'set_nickname', session_id: this.sessionId, nickname: this.nickname });
    this.publishSelf();
    this.publishDevices();
    this.publishFiles();
    this.publishMessages();
  }

  offerFile(file: SharedFile, device: Device): void {
    if (this.files.find((listed) => listed.id === file.id)?.uploader_id === device.id) {
      this.notify('info', `${device.name} shared "${file.name}", so it already has it`);
      return;
    }
    this.send({ type: 'offer_file_to', session_id: this.sessionId, target_session_id: device.id, file_id: file.id });
    this.notify('info', `Offered "${file.name}" to ${device.name}`);
  }

  async answerOffer(offer: IncomingOffer, accepted: boolean): Promise<DownloadTicket | null> {
    if (!accepted) {
      this.send({ type: 'decline_file_offer', session_id: this.sessionId, target_session_id: offer.senderId, file_id: offer.fileId });
      return null;
    }
    if (!offer.isDirect) {
      const listed = this.files.find((file) => file.id === offer.fileId);
      if (!listed) throw new TransferError('That file is no longer available.');
      return this.prepareDownload(this.toSharedFile(listed));
    }
    this.send({ type: 'accept_file_offer', session_id: this.sessionId, file_id: offer.fileId });
    return this.waitForPushedFile(offer);
  }

  async leave(): Promise<void> {
    this.stop();
    await this.transport.request('POST', '/logout').catch(() => undefined);
    this.emit({ type: 'connection', state: 'signed-out' });
  }

  private emit(event: ClientEvent): void {
    for (const listener of this.listeners) listener(event);
  }

  private notify(level: NoticeLevel, text: string): void {
    this.emit({ type: 'notice', notice: { level, text } });
  }

  private async fetchStatus(): Promise<AuthStatus> {
    const reply = await this.transport.request('GET', '/auth-status');
    return JSON.parse(reply.text) as AuthStatus;
  }

  private setConnection(state: ConnectionState): void {
    this.emit({ type: 'connection', state });
  }

  private openSocket(): void {
    const socket: NodeSocket = this.transport.openSocket({
      onOpen: () => {
        this.setConnection('online');
        this.send({ type: 'join', session_id: this.sessionId, user_agent: navigator.userAgent, nickname: this.nickname || null });
      },
      onMessage: (text) => {
        try {
          this.handleMessage(JSON.parse(text) as ServerMessage);
        } catch (error) {
          console.error('Could not read a message from the node', error);
        }
      },
      onClose: () => {
        if (this.isStopped || this.socket !== socket) return;
        this.setConnection('offline');
        this.scheduleReconnect();
      },
    });
    this.socket = socket;
  }

  // Signed out means the node would refuse the socket, so ask first instead of looping.
  private scheduleReconnect(): void {
    if (this.isStopped) return;
    this.reconnectTimer = setTimeout(async () => {
      try {
        this.status = await this.fetchStatus();
        if (this.status.auth_required && !this.status.authenticated) {
          this.setConnection('signed-out');
          return;
        }
      } catch {
        this.scheduleReconnect();
        return;
      }
      this.openSocket();
    }, RECONNECT_DELAY_MS);
  }

  private send(message: Record<string, unknown>): void {
    if (this.socket?.isOpen) this.socket.send(JSON.stringify(message));
  }

  private handleMessage(message: ServerMessage): void {
    switch (message.type) {
      case 'peer_joined':
        this.rememberPeer(message.peer);
        this.publishDevices();
        this.publishFiles();
        break;
      case 'peer_left':
        this.peers.delete(message.session_id);
        this.publishDevices();
        break;
      case 'peer_sync':
        for (const peer of message.peers) {
          if (peer.hosting_node_id == null) this.peers.delete(peer.session_id);
          else this.rememberPeer({ ...this.peers.get(peer.session_id), ...peer });
        }
        this.publishDevices();
        this.publishFiles();
        break;
      case 'file_list_update':
        this.replaceFiles(message.files);
        this.publishFiles();
        break;
      case 'file_removed':
        this.confirmRemoval(message.file_id);
        break;
      case 'text_message':
        this.messages = [...this.messages, message.message];
        this.publishMessages();
        break;
      case 'message_history':
        this.messages = message.messages;
        this.publishMessages();
        break;
      case 'incoming_file_offer':
        this.emit({ type: 'offer', offer: this.toOffer(message) });
        break;
      case 'file_offer_declined':
        this.reportDeclined(message.file_id, message.from_session_id);
        break;
      case 'file_push_delivered':
        this.reportDelivered(message.file_id, message.to_session_id);
        break;
      case 'no_peers_warning':
        this.emit({ type: 'no-peers' });
        break;
      case 'error':
        this.notify('error', message.message);
        break;
      case 'pong':
        break;
    }
  }

  private rememberPeer(peer: WirePeer): void {
    if (peer.hosting_node_id && peer.hosting_node_name) this.nodeNames.set(peer.hosting_node_id, peer.hosting_node_name);
    const name = peer.nickname || friendlyDeviceName(peer.user_agent);
    if (name) this.deviceNames.set(peer.session_id, name);
    this.peers.set(peer.session_id, peer);
  }

  private replaceFiles(files: WireFile[]): void {
    const listed = new Set(files.map((file) => file.id));
    for (const file of this.files) {
      if (!listed.has(file.id)) this.removedFiles.set(file.id, file);
    }
    for (const id of listed) this.removedFiles.delete(id);
    this.files = files;
  }

  private nodeName(nodeId: string): string {
    return this.nodeNames.get(nodeId) ?? `node ${nodeId.slice(-6)}`;
  }

  private deviceName(sessionId: string): string {
    if (sessionId === this.sessionId) {
      return this.nickname || friendlyDeviceName(navigator.userAgent) || 'This device';
    }
    const peer = this.peers.get(sessionId);
    return peer?.nickname || friendlyDeviceName(peer?.user_agent) || this.deviceNames.get(sessionId) || `Peer ${sessionId.slice(-6)}`;
  }

  private publishSelf(): void {
    const own = this.peers.get(this.sessionId);
    const nodeName = own?.hosting_node_name ?? this.nodeNames.get(this.status.node_id) ?? 'this node';
    const self: SelfInfo = { id: this.sessionId, name: this.deviceName(this.sessionId), nodeName };
    this.emit({ type: 'self', self });
  }

  private publishDevices(): void {
    const others = [...this.peers.values()].filter((peer) => peer.session_id !== this.sessionId && !peer.left);
    const toDevice = (peer: WirePeer): Device => ({
      id: peer.session_id,
      name: this.deviceName(peer.session_id),
      nodeName: peer.hosting_node_name ?? '',
      nodeId: peer.hosting_node_id ?? '',
      kind: deviceKindOf(peer.user_agent),
      isSelf: false,
      latencyMs: peer.node_rtt_ms ?? null,
    });
    const self: Device = {
      id: this.sessionId,
      name: this.deviceName(this.sessionId),
      nodeName: this.peers.get(this.sessionId)?.hosting_node_name ?? '',
      nodeId: this.peers.get(this.sessionId)?.hosting_node_id ?? this.status.node_id,
      kind: deviceKindOf(navigator.userAgent),
      isSelf: true,
      latencyMs: null,
    };
    this.emit({ type: 'devices', devices: [self, ...others.map(toDevice)] });
    this.publishSelf();
  }

  // Shared by this device, or from the command line of the machine this page runs on.
  private isOwn(file: WireFile): boolean {
    if (file.uploader_id === '') return this.status.is_host && file.uploader_node === this.status.node_id;
    return file.uploader_id === this.sessionId;
  }

  // The node this page belongs to shared the file, holds the only copy, and is a part-time seed.
  private needsThisDeviceOnline(file: WireFile): boolean {
    const isOnlyHolder = file.hosts.length === 1 && file.hosts[0] === this.status.node_id;
    return this.status.is_host && file.uploader_node === this.status.node_id && isOnlyHolder && file.only_part_time_hosts === true;
  }

  private holderName(nodeId: string): string {
    return nodeId === this.status.node_id ? `${this.nodeName(nodeId)} (this node)` : this.nodeName(nodeId);
  }

  private toSharedFile(file: WireFile): SharedFile {
    const isOwn = this.isOwn(file);
    const uploaderKnown = file.uploader_id !== '' && (this.peers.has(file.uploader_id) || this.deviceNames.has(file.uploader_id));
    const holderNames = file.hosts.map((nodeId) => this.holderName(nodeId));
    return {
      id: file.id,
      name: file.name,
      mimeType: file.mime_type,
      isFolder: file.is_folder,
      sizeBytes: file.is_folder ? file.folder_bytes : file.size,
      folderFileCount: file.folder_files,
      sharedAt: Date.parse(file.uploaded_at),
      sharedBy: isOwn ? 'You' : uploaderKnown ? this.deviceName(file.uploader_id) : this.nodeName(file.uploader_node),
      isOwn,
      holderNames,
      isAvailable: file.available ?? holderNames.length > 0,
      canUnshare: file.uploader_id === this.sessionId || (this.status.is_host && file.uploader_node === this.status.node_id),
      needsThisDeviceOnline: this.needsThisDeviceOnline(file),
    };
  }

  private publishFiles(): void {
    const files = this.files.filter((file) => !file.parent).map((file) => this.toSharedFile(file));
    this.emit({ type: 'files', files });
  }

  private publishMessages(): void {
    const messages: ChatMessage[] = this.messages.map((message) => ({
      id: message.id,
      text: message.content,
      senderName: message.sender_id === this.sessionId ? 'You' : message.sender_name || this.deviceName(message.sender_id),
      isOwn: message.sender_id === this.sessionId,
      sentAt: Date.parse(message.timestamp),
    }));
    this.emit({ type: 'messages', messages });
  }

  private toOffer(message: Extract<ServerMessage, { type: 'incoming_file_offer' }>): IncomingOffer {
    const listed = this.files.find((file) => file.id === message.file_id);
    const pushed = message.pushed;
    const fallbackSize = listed ? (listed.is_folder ? listed.folder_bytes : listed.size) : null;
    return {
      fileId: message.file_id,
      fileName: pushed?.name ?? listed?.name ?? 'a file',
      sizeBytes: pushed?.size ?? fallbackSize,
      senderName: this.deviceName(message.from_session_id),
      senderId: message.from_session_id,
      isDirect: pushed !== undefined,
    };
  }

  private confirmRemoval(fileId: string): void {
    const removed = this.removedFiles.get(fileId);
    this.removedFiles.delete(fileId);
    const name = this.pendingUnshares.get(fileId);
    if (name !== undefined) {
      this.pendingUnshares.delete(fileId);
      this.notify('success', `Unshared ${name}`);
      return;
    }
    // The files inside a folder go with it, and the folder's own notice covers them.
    if (removed?.parent) return;
    this.notify('info', removed ? `"${removed.name}" is no longer shared` : 'A shared file was removed');
  }

  private reportDeclined(fileId: string, fromSessionId: string): void {
    const who = this.deviceName(fromSessionId);
    const name = this.sentFiles.get(fileId);
    this.sentFiles.delete(fileId);
    this.notify('warning', name ? `${who} declined "${name}"` : `${who} declined your file offer`);
  }

  private reportDelivered(fileId: string, toSessionId: string): void {
    const who = this.deviceName(toSessionId);
    const name = this.sentFiles.get(fileId) ?? 'your file';
    this.sentFiles.delete(fileId);
    this.notify('success', `${who} received "${name}"`);
  }

  private handleSignedOut(): never {
    this.setConnection('signed-out');
    throw new TransferError('You have been signed out.');
  }

  /** What the page needs to save a file; a subclass may save it itself. */
  protected ticket(url: string, filename: string): DownloadTicket {
    return { url, filename };
  }

  private async probe(url: string): Promise<void> {
    let reply;
    try {
      reply = await this.transport.request('HEAD', url);
    } catch {
      throw new TransferError('Could not reach the node.');
    }
    if (reply.status === 401) this.handleSignedOut();
    if (!isSuccess(reply.status)) throw new TransferError(describeProblem(reply.status));
  }

  // The node learns of the acceptance over the WebSocket, which this request can overtake.
  private async waitForPushedFile(offer: IncomingOffer): Promise<DownloadTicket> {
    const url = `/api/files/${encodeURIComponent(offer.fileId)}?to=${encodeURIComponent(this.sessionId)}`;
    for (let attempt = 0; attempt < ACCEPT_POLL_ATTEMPTS; attempt++) {
      const reply = await this.transport.request('HEAD', url).catch(() => null);
      if (reply?.status === 401) this.handleSignedOut();
      if (reply && isSuccess(reply.status)) return this.ticket(url, offer.fileName);
      await sleep(ACCEPT_POLL_DELAY_MS);
    }
    throw new TransferError(`Could not receive ${offer.fileName}.`);
  }

  private uploadWire(file: File, fileId: string, options: SendOptions): UploadWire {
    const baseHeaders: Record<string, string> = {
      'X-Ladex-Session': this.sessionId,
      'X-Ladex-Size': String(file.size),
      'X-Ladex-Name': encodeURIComponent(file.name),
      'Content-Type': file.type || 'application/octet-stream',
    };
    if (options.parent) baseHeaders['X-Ladex-Parent'] = options.parent;
    if (options.to) baseHeaders['X-Ladex-To'] = options.to;

    return {
      queryProgress: async () => {
        const reply = await this.transport.request('GET', `/api/files/${fileId}/upload?size=${file.size}`);
        if (reply.status === 401) throw new SignedOut();
        const body = JSON.parse(reply.text) as { offset?: number; complete?: boolean };
        return { offset: body.offset ?? 0, isComplete: body.complete ?? false };
      },
      sendFrom: (offset, onProgress) => {
        const headers = { ...baseHeaders, 'X-Ladex-Offset': String(offset) };
        const body = offset > 0 ? file.slice(offset) : file;
        return this.transport.upload(`/api/files/${fileId}`, headers, body, options.signal, (sent) => onProgress(offset + sent));
      },
    };
  }

  private async sendFile(file: File, fileId: string, options: SendOptions): Promise<UploadReply> {
    try {
      return await runResumableUpload(this.uploadWire(file, fileId, options), {
        signal: options.signal,
        onProgress: options.onProgress,
        fileId,
        size: file.size,
      });
    } catch (error) {
      if (error instanceof SignedOut) this.handleSignedOut();
      throw error;
    }
  }

  private async publishFolder(folderId: string, name: string, children: Array<{ path: string; file_id: string }>): Promise<void> {
    const reply = await this.transport.request('PUT', `/api/folders/${folderId}`, {
      headers: { 'Content-Type': 'application/json', 'X-Ladex-Session': this.sessionId },
      body: JSON.stringify({ name, children }),
    });
    if (isSuccess(reply.status)) return;
    throw new TransferError(parseUploadReply(reply.text).message ?? `the node answered ${reply.status}`);
  }
}
