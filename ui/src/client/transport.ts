import type { SendResult } from './resumable-upload';

export interface TransportReply {
  status: number;
  text: string;
}

export interface RequestOptions {
  headers?: Record<string, string>;
  body?: string;
}

export interface SocketHandlers {
  onOpen: () => void;
  onMessage: (text: string) => void;
  onClose: () => void;
}

export interface NodeSocket {
  readonly isOpen: boolean;
  send(text: string): void;
  close(): void;
}

/** How a client reaches its node: over HTTP from a browser, or inside the app without leaving the process. */
export interface NodeTransport {
  /** Rejects only when the node cannot be reached; a refusal is a reply with its status. */
  request(method: string, path: string, options?: RequestOptions): Promise<TransportReply>;
  upload(path: string, headers: Record<string, string>, body: Blob, signal: AbortSignal, onProgress: (sentBytes: number) => void): Promise<SendResult>;
  openSocket(handlers: SocketHandlers): NodeSocket;
}

export function parseUploadReply(text: string): SendResult['reply'] {
  try {
    return JSON.parse(text) as SendResult['reply'];
  } catch {
    return {};
  }
}
