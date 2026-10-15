import type { SendResult } from './resumable-upload';
import { parseUploadReply, type NodeSocket, type NodeTransport, type RequestOptions, type SocketHandlers, type TransportReply } from './transport';

/** A node reached with fetch, XHR and a WebSocket from the page it served. */
export class HttpTransport implements NodeTransport {
  async request(method: string, path: string, options: RequestOptions = {}): Promise<TransportReply> {
    const response = await fetch(path, {
      method,
      cache: 'no-store',
      credentials: 'same-origin',
      headers: options.headers,
      body: options.body,
    });
    return { status: response.status, text: await response.text() };
  }

  upload(
    path: string,
    headers: Record<string, string>,
    body: Blob,
    signal: AbortSignal,
    onProgress: (sentBytes: number) => void,
  ): Promise<SendResult> {
    return new Promise((resolve) => {
      const request = new XMLHttpRequest();
      request.open('PUT', path);
      for (const [name, value] of Object.entries(headers)) request.setRequestHeader(name, value);
      request.upload.onprogress = (event) => {
        if (event.lengthComputable) onProgress(event.loaded);
      };
      request.onload = () =>
        resolve({ status: request.status, isNetworkFailure: false, isAborted: false, reply: parseUploadReply(request.responseText) });
      request.onerror = () => resolve({ status: 0, isNetworkFailure: true, isAborted: false, reply: {} });
      request.ontimeout = request.onerror;
      request.onabort = () => resolve({ status: 0, isNetworkFailure: false, isAborted: true, reply: {} });
      signal.addEventListener('abort', () => request.abort(), { once: true });
      request.send(body);
    });
  }

  openSocket(handlers: SocketHandlers): NodeSocket {
    const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const socket = new WebSocket(`${scheme}//${location.host}/ws`);
    socket.onopen = handlers.onOpen;
    socket.onmessage = (event) => handlers.onMessage(event.data as string);
    socket.onclose = handlers.onClose;
    return {
      get isOpen() {
        return socket.readyState === WebSocket.OPEN;
      },
      send: (text) => socket.send(text),
      close: () => socket.close(),
    };
  }
}
