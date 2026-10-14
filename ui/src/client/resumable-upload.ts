import { sleep } from './concurrency';
import { TransferCancelled, TransferError } from './types';

export const MAX_RETRIES = 6;
const MAX_BACKOFF_MS = 15_000;

export interface UploadReply {
  complete?: boolean;
  message?: string;
  offset?: number;
  file?: { id: string };
}

export interface SendResult {
  status: number;
  isNetworkFailure: boolean;
  isAborted: boolean;
  reply: UploadReply;
}

export interface UploadProgress {
  offset: number;
  isComplete: boolean;
}

/** The two requests an upload is made of; the node says where it can continue from. */
export interface UploadWire {
  queryProgress(): Promise<UploadProgress>;
  sendFrom(offset: number, onProgress: (bytesSent: number) => void): Promise<SendResult>;
}

export class SignedOut extends Error {
  constructor() {
    super('signed out');
  }
}

export interface ResumableUploadOptions {
  signal: AbortSignal;
  onProgress: (bytesSent: number) => void;
  fileId: string;
  size: number;
  pause?: (milliseconds: number) => Promise<void>;
}

function backoff(attempt: number): number {
  return Math.min(1000 * 2 ** attempt, MAX_BACKOFF_MS);
}

function describeFailure(result: SendResult): string {
  if (result.reply.message) return result.reply.message;
  return result.isNetworkFailure ? 'the connection was lost' : `the node answered ${result.status}`;
}

/** Sends a file, picking up from what the node already has after every interruption; resolves with the node's reply. */
export async function runResumableUpload(wire: UploadWire, options: ResumableUploadOptions): Promise<UploadReply> {
  const { signal, onProgress, fileId, size } = options;
  const pause = options.pause ?? sleep;

  for (let attempt = 0; ; attempt++) {
    if (signal.aborted) throw new TransferCancelled();

    let offset = 0;
    try {
      const progress = await wire.queryProgress();
      if (progress.isComplete) return { complete: true, file: { id: fileId } };
      offset = progress.offset;
    } catch (error) {
      if (error instanceof SignedOut) throw error;
      if (attempt >= MAX_RETRIES) throw new TransferError('the node could not be reached');
      await pause(backoff(attempt));
      continue;
    }

    const result = await wire.sendFrom(offset, onProgress);
    if (result.isAborted || signal.aborted) throw new TransferCancelled();
    if (result.status >= 200 && result.status < 300) {
      onProgress(size);
      return result.reply.file ? result.reply : { ...result.reply, file: { id: fileId } };
    }
    if (result.status === 401) throw new SignedOut();

    const isResumable = result.isNetworkFailure || (result.status === 409 && typeof result.reply.offset === 'number');
    if (!isResumable || attempt >= MAX_RETRIES) throw new TransferError(describeFailure(result));
    if (result.isNetworkFailure) await pause(backoff(attempt));
  }
}
