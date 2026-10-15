import type { SendResult } from './resumable-upload';
import { parseUploadReply, type NodeSocket, type NodeTransport, type RequestOptions, type SocketHandlers, type TransportReply } from './transport';

// Pieces are sent in whole MiB: the node stores uploads in 1 MiB chunks.
const UPLOAD_PIECE_BYTES = 4 * 1024 * 1024;

/** What the app's Rust side offers; `ipc-client.ts` binds it to Tauri, tests bind it to a fake. */
export interface IpcBackend {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
  invokeWithBytes<T>(command: string, bytes: Uint8Array, headers: Record<string, string>): Promise<T>;
  /** Makes a channel the Rust side can send to; pass the result as an argument. */
  openChannel<T>(onMessage: (message: T) => void): unknown;
}

interface WireReply {
  status: number;
  body: string;
}

export type SocketEvent = { kind: 'message'; text: string } | { kind: 'closed' };

/** A node reached by commands into the app's own process: no port, no certificate, no cookies. */
export class IpcTransport implements NodeTransport {
  constructor(private readonly backend: IpcBackend) {}

  async request(method: string, path: string, options: RequestOptions = {}): Promise<TransportReply> {
    const reply = await this.backend.invoke<WireReply>('ladex_request', {
      method,
      path,
      headers: Object.entries(options.headers ?? {}),
      body: options.body ?? '',
    });
    return { status: reply.status, text: reply.body };
  }

  async upload(
    path: string,
    headers: Record<string, string>,
    body: Blob,
    signal: AbortSignal,
    onProgress: (sentBytes: number) => void,
  ): Promise<SendResult> {
    const uploadId = await this.backend.invoke<number>('ladex_upload_begin', {
      path,
      headers: Object.entries(headers),
      contentLength: body.size,
    });
    const aborted = (): SendResult => ({ status: 0, isNetworkFailure: false, isAborted: true, reply: {} });
    const idHeader = { 'x-upload-id': String(uploadId) };

    for (let sent = 0; sent < body.size; sent += UPLOAD_PIECE_BYTES) {
      if (signal.aborted) {
        await this.backend.invoke('ladex_upload_abort', { uploadId }).catch(() => undefined);
        return aborted();
      }
      const piece = new Uint8Array(await body.slice(sent, sent + UPLOAD_PIECE_BYTES).arrayBuffer());
      try {
        await this.backend.invokeWithBytes('ladex_upload_write', piece, idHeader);
      } catch {
        // The node stopped reading because it had already answered; that answer says why.
        break;
      }
      onProgress(Math.min(body.size, sent + piece.length));
    }

    if (signal.aborted) {
      await this.backend.invoke('ladex_upload_abort', { uploadId }).catch(() => undefined);
      return aborted();
    }
    try {
      const reply = await this.backend.invoke<WireReply>('ladex_upload_finish', { uploadId });
      return { status: reply.status, isNetworkFailure: false, isAborted: false, reply: parseUploadReply(reply.body) };
    } catch {
      return { status: 0, isNetworkFailure: true, isAborted: false, reply: {} };
    }
  }

  openSocket(handlers: SocketHandlers): NodeSocket {
    let socketId: number | null = null;
    let isClosed = false;

    const channel = this.backend.openChannel<SocketEvent>((event) => {
      if (isClosed) return;
      if (event.kind === 'message') {
        handlers.onMessage(event.text);
        return;
      }
      isClosed = true;
      handlers.onClose();
    });

    this.backend
      .invoke<number>('ladex_socket_open', { onEvent: channel })
      .then((id) => {
        if (isClosed) {
          void this.backend.invoke('ladex_socket_close', { socketId: id });
          return;
        }
        socketId = id;
        handlers.onOpen();
      })
      .catch(() => {
        if (isClosed) return;
        isClosed = true;
        handlers.onClose();
      });

    return {
      get isOpen() {
        return socketId !== null && !isClosed;
      },
      send: (text) => {
        if (socketId !== null && !isClosed) void this.backend.invoke('ladex_socket_send', { socketId, text });
      },
      close: () => {
        if (isClosed) return;
        isClosed = true;
        if (socketId !== null) void this.backend.invoke('ladex_socket_close', { socketId });
      },
    };
  }
}
