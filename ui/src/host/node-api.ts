import type { NodeTransport } from '../client/transport';
import { TransferError } from '../client/types';

/** Sends a JSON request to the node's own page; a refusal becomes a TransferError carrying the node's reason. */
export async function callNode<T>(transport: NodeTransport, method: string, path: string, body?: unknown): Promise<T> {
  const headers = body === undefined ? undefined : { 'Content-Type': 'application/json' };
  const reply = await transport.request(method, path, { headers, body: body === undefined ? undefined : JSON.stringify(body) });
  let parsed: T & { message?: string };
  try {
    parsed = JSON.parse(reply.text) as T & { message?: string };
  } catch {
    parsed = {} as T & { message?: string };
  }
  if (reply.status < 200 || reply.status >= 300) throw new TransferError(parsed.message ?? `the node answered ${reply.status}`);
  return parsed;
}
