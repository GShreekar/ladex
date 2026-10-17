import {
  TransferCancelled,
  TransferError,
  type Capabilities,
  type ClientEvent,
  type ClientListener,
  type Device,
  type DownloadTicket,
  type FileUploadRequest,
  type IncomingOffer,
  type LadexClient,
  type SharedFile,
  type UploadOutcome,
  type UploadRequest,
} from './types';
import { runTusUpload, SessionEnded, type TusPatchResult, type TusState, type TusWire } from './tus-upload';
import type { WireFile } from './wire';

const TUS_HEADERS = { 'Tus-Resumable': '1.0.0' };
const RESTART_DELAY_MS = 3000;
const APPROVAL_POLL_MS = 4000;

export interface GuestMe {
  id: string;
  label: string;
  permission: 'read-only' | 'upload' | 'upload-with-approval';
  can_upload: boolean;
  needs_approval: boolean;
  expires_in_secs: number;
  node_id: string;
  node_name: string;
}

interface WireDevice {
  id: string;
  name: string;
  node_id: string;
  node_name: string;
  is_here: boolean;
}

function describeProblem(status: number): string {
  if (status === 404) return 'That file has been removed.';
  if (status === 503) return 'No device that has this file is online right now.';
  return `The host could not provide that file (error ${status}).`;
}

function encodeMetadata(text: string): string {
  const bytes = new TextEncoder().encode(text);
  return btoa(Array.from(bytes, (byte) => String.fromCharCode(byte)).join(''));
}

function readMessage(text: string): string | null {
  try {
    return (JSON.parse(text) as { message?: string }).message ?? null;
  } catch {
    return null;
  }
}

function numberHeader(request: XMLHttpRequest, name: string): number | null {
  const value = request.getResponseHeader(name);
  return value === null ? null : Number(value);
}

function patchWithXhr(
  location: string,
  offset: number,
  piece: Blob,
  signal: AbortSignal,
  onProgress: (loaded: number) => void,
): Promise<TusPatchResult> {
  return new Promise((resolve) => {
    const request = new XMLHttpRequest();
    request.open('PATCH', location);
    for (const [name, value] of Object.entries(TUS_HEADERS)) request.setRequestHeader(name, value);
    request.setRequestHeader('Content-Type', 'application/offset+octet-stream');
    request.setRequestHeader('Upload-Offset', String(offset));
    request.upload.onprogress = (event) => {
      if (event.lengthComputable) onProgress(event.loaded);
    };
    const failure = { status: 0, isNetworkFailure: false, isAborted: false, offset: null, state: null, message: null };
    request.onload = () =>
      resolve({
        ...failure,
        status: request.status,
        offset: numberHeader(request, 'Upload-Offset'),
        state: request.getResponseHeader('Ladex-Upload-State') as TusState | null,
        message: readMessage(request.responseText),
      });
    request.onerror = () => resolve({ ...failure, isNetworkFailure: true });
    request.ontimeout = request.onerror;
    request.onabort = () => resolve({ ...failure, isAborted: true });
    signal.addEventListener('abort', () => request.abort(), { once: true });
    request.send(piece);
  });
}

/** A browser that joined a node with an invitation: it reads the live catalog and, if allowed, uploads. */
export class GuestClient implements LadexClient {
  private readonly listeners = new Set<ClientListener>();
  private readonly nodeNames = new Map<string, string>();
  private devices: WireDevice[] = [];
  private files: WireFile[] = [];
  private events: EventSource | null = null;
  private restartTimer: ReturnType<typeof setTimeout> | null = null;
  private readonly approvalTimers = new Set<ReturnType<typeof setInterval>>();
  private isStopped = false;

  constructor(private readonly me: GuestMe) {
    this.nodeNames.set(me.node_id, me.node_name);
  }

  get capabilities(): Capabilities {
    return {
      canUpload: this.me.can_upload,
      uploadNeedsApproval: this.me.needs_approval,
      canShareFolders: false,
      canChat: false,
      canRename: false,
      canSendToDevice: false,
      canManageGuests: false,
      canLeave: true,
    };
  }

  subscribe(listener: ClientListener): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  async start(): Promise<void> {
    this.isStopped = false;
    this.emit({ type: 'self', self: { id: this.me.id, name: this.me.label, nodeName: this.me.node_name } });
    this.emit({ type: 'connection', state: 'connecting' });
    this.watchCatalog();
  }

  stop(): void {
    this.isStopped = true;
    if (this.restartTimer) clearTimeout(this.restartTimer);
    this.events?.close();
    this.events = null;
    for (const timer of this.approvalTimers) clearInterval(timer);
    this.approvalTimers.clear();
  }

  async uploadFile(file: File, request: FileUploadRequest): Promise<UploadOutcome> {
    if (!this.me.can_upload) throw new TransferError('This invitation does not allow uploads.');
    let location = '';
    try {
      const outcome = await runTusUpload(this.tusWire(request.signal), {
        file,
        signal: request.signal,
        onProgress: request.onProgress,
        onCreated: (created) => (location = created),
      });
      if (outcome === 'awaiting-approval' && request.onAnswered) this.watchApproval(location, request.onAnswered);
      return outcome;
    } catch (error) {
      if (error instanceof SessionEnded) this.endSession();
      throw error;
    }
  }

  async uploadFolder(_files: File[], _request: UploadRequest): Promise<UploadOutcome> {
    throw new TransferError('Folders can only be shared from a device running LADEX.');
  }

  async prepareDownload(file: SharedFile): Promise<DownloadTicket> {
    if (file.isFolder) {
      return { url: `/blobs/${encodeURIComponent(file.id)}.zip`, filename: `${file.name}.zip` };
    }
    const url = `/blobs/${encodeURIComponent(file.id)}`;
    let response: Response;
    try {
      response = await fetch(url, { method: 'HEAD', cache: 'no-store' });
    } catch {
      throw new TransferError('Could not reach the host.');
    }
    if (response.status === 401) {
      this.endSession();
      throw new TransferError('Your session has ended.');
    }
    if (!response.ok) throw new TransferError(describeProblem(response.status));
    return { url, filename: file.name };
  }

  unshare(): void {}

  sendMessage(): void {}

  rename(): void {}

  offerFile(_file: SharedFile, _device: Device): void {}

  async answerOffer(_offer: IncomingOffer, _accepted: boolean): Promise<DownloadTicket | null> {
    return null;
  }

  async leave(): Promise<void> {
    this.stop();
    await fetch('/api/me', { method: 'DELETE' }).catch(() => undefined);
    this.emit({ type: 'connection', state: 'ended' });
  }

  // The node says nothing when the host answers, so the upload is asked after until it is shared or gone.
  private watchApproval(location: string, onAnswered: (isAccepted: boolean) => void): void {
    let isAnswered = false;
    const timer = setInterval(async () => {
      const response = await fetch(location, { method: 'HEAD', headers: TUS_HEADERS, cache: 'no-store' }).catch(() => null);
      const isAccepted = response?.ok === true && response.headers.get('Ladex-Upload-State') === 'shared';
      const isDeclined = response?.status === 404;
      if (isAnswered || (!isAccepted && !isDeclined)) return;
      isAnswered = true;
      clearInterval(timer);
      this.approvalTimers.delete(timer);
      onAnswered(isAccepted);
    }, APPROVAL_POLL_MS);
    this.approvalTimers.add(timer);
  }

  private emit(event: ClientEvent): void {
    for (const listener of this.listeners) listener(event);
  }

  private endSession(): void {
    this.stop();
    this.emit({ type: 'connection', state: 'ended' });
  }

  private watchCatalog(): void {
    const events = new EventSource('/api/catalog');
    this.events = events;

    events.onopen = () => this.emit({ type: 'connection', state: 'online' });
    events.addEventListener('catalog', (event) => {
      this.files = (JSON.parse((event as MessageEvent<string>).data) as { files: WireFile[] }).files;
      void this.refreshDevices();
    });
    events.addEventListener('ended', () => this.endSession());
    events.onerror = () => {
      if (this.isStopped) return;
      this.emit({ type: 'connection', state: 'offline' });
      // A closed stream will not retry by itself; it also means the node may have refused us for good.
      if (events.readyState === EventSource.CLOSED) void this.restartOrEnd();
    };
  }

  private async restartOrEnd(): Promise<void> {
    const me = await fetch('/api/me', { cache: 'no-store' }).catch(() => null);
    if (me?.status === 401) {
      this.endSession();
      return;
    }
    this.restartTimer = setTimeout(() => this.watchCatalog(), RESTART_DELAY_MS);
  }

  private async refreshDevices(): Promise<void> {
    try {
      const response = await fetch('/api/devices', { cache: 'no-store' });
      if (response.ok) this.devices = ((await response.json()) as { devices: WireDevice[] }).devices;
    } catch {
      // The list shown before stays until the next catalog change.
    }
    for (const device of this.devices) this.nodeNames.set(device.node_id, device.node_name);
    this.publish();
  }

  private nodeName(nodeId: string): string {
    return this.nodeNames.get(nodeId) ?? `node ${nodeId.slice(-6)}`;
  }

  private publish(): void {
    const devices: Device[] = this.devices.map((device) => ({
      id: device.id,
      name: device.name || `Device ${device.id.slice(-6)}`,
      nodeName: device.node_name,
      nodeId: device.node_id,
      kind: 'unknown',
      isSelf: false,
      latencyMs: null,
    }));
    const deviceNames = new Map(devices.map((device) => [device.id, device.name]));
    const files: SharedFile[] = this.files
      .filter((file) => !file.parent)
      .map((file) => {
        const holderNames = file.hosts.map((nodeId) => this.nodeName(nodeId));
        const isOwn = file.uploader_id === this.me.id;
        return {
          id: file.id,
          name: file.name,
          mimeType: file.mime_type,
          isFolder: file.is_folder,
          sizeBytes: file.is_folder ? file.folder_bytes : file.size,
          folderFileCount: file.folder_files,
          sharedAt: Date.parse(file.uploaded_at),
          sharedBy: isOwn ? 'You' : (deviceNames.get(file.uploader_id) ?? this.nodeName(file.uploader_node)),
          isOwn,
          holderNames,
          isAvailable: file.available ?? holderNames.length > 0,
          canUnshare: false,
        };
      });
    this.emit({ type: 'devices', devices });
    this.emit({ type: 'files', files });
  }

  private tusWire(signal: AbortSignal): TusWire {
    return {
      create: async (size, name, mimeType) => {
        const metadata = `filename ${encodeMetadata(name)},filetype ${encodeMetadata(mimeType)}`;
        const response = await fetch('/uploads', {
          method: 'POST',
          headers: { ...TUS_HEADERS, 'Upload-Length': String(size), 'Upload-Metadata': metadata },
          signal,
        }).catch(() => {
          throw signal.aborted ? new TransferCancelled() : new TransferError('Could not reach the host.');
        });
        if (response.status === 401) throw new SessionEnded();
        if (response.status !== 201) {
          const body = (await response.json().catch(() => ({}))) as { message?: string };
          throw new TransferError(body.message ?? `the host answered ${response.status}`);
        }
        const location = response.headers.get('Location') ?? '';
        const state = (response.headers.get('Ladex-Upload-State') ?? 'receiving') as TusState;
        return { location, state };
      },
      patch: (location, offset, piece, onProgress) => patchWithXhr(location, offset, piece, signal, onProgress),
      head: async (location) => {
        const response = await fetch(location, { method: 'HEAD', headers: TUS_HEADERS }).catch(() => null);
        if (!response?.ok) return null;
        return Number(response.headers.get('Upload-Offset'));
      },
      cancel: async (location) => {
        await fetch(location, { method: 'DELETE', headers: TUS_HEADERS }).catch(() => undefined);
      },
    };
  }
}
