import { sleep } from './concurrency';
import { MAX_RETRIES } from './resumable-upload';
import { TransferCancelled, TransferError, type UploadOutcome } from './types';

// A multiple of the node's 1 MiB chunk, so a failed piece loses at most one piece of work.
export const PIECE_BYTES = 8 * 1024 * 1024;
const MAX_BACKOFF_MS = 15_000;

export type TusState = 'receiving' | 'awaiting-approval' | 'shared';

export interface TusCreated {
  location: string;
  state: TusState;
}

export interface TusPatchResult {
  status: number;
  isNetworkFailure: boolean;
  isAborted: boolean;
  offset: number | null;
  state: TusState | null;
  message: string | null;
}

/** The three requests of the tus protocol the node speaks. */
export interface TusWire {
  create(size: number, name: string, mimeType: string): Promise<TusCreated>;
  patch(location: string, offset: number, piece: Blob, onProgress: (loaded: number) => void): Promise<TusPatchResult>;
  head(location: string): Promise<number | null>;
  cancel(location: string): Promise<void>;
}

export class SessionEnded extends Error {
  constructor() {
    super('session ended');
  }
}

export interface TusUploadOptions {
  file: File;
  signal: AbortSignal;
  onProgress: (bytesSent: number) => void;
  pause?: (milliseconds: number) => Promise<void>;
}

function toOutcome(state: TusState): UploadOutcome {
  return state === 'awaiting-approval' ? 'awaiting-approval' : 'shared';
}

function backoff(attempt: number): number {
  return Math.min(1000 * 2 ** attempt, MAX_BACKOFF_MS);
}

/** Uploads a file in pieces, carrying on from the offset the node confirms after every failure. */
export async function runTusUpload(wire: TusWire, options: TusUploadOptions): Promise<UploadOutcome> {
  const { file, signal, onProgress } = options;
  const pause = options.pause ?? sleep;

  const created = await wire.create(file.size, file.name, file.type || 'application/octet-stream');
  let state = created.state;
  let offset = 0;
  let failures = 0;

  while (offset < file.size) {
    if (signal.aborted) {
      await wire.cancel(created.location);
      throw new TransferCancelled();
    }
    const piece = file.slice(offset, offset + PIECE_BYTES);
    const base = offset;
    const result = await wire.patch(created.location, offset, piece, (loaded) => onProgress(base + loaded));

    if (result.isAborted || signal.aborted) {
      await wire.cancel(created.location);
      throw new TransferCancelled();
    }
    if (result.status === 401) throw new SessionEnded();
    if (result.isNetworkFailure) {
      if (++failures > MAX_RETRIES) throw new TransferError('the connection was lost');
      await pause(backoff(failures - 1));
      offset = (await wire.head(created.location)) ?? offset;
      continue;
    }
    const isAccepted = result.status === 204 || result.status === 409;
    if (!isAccepted || result.offset === null) throw new TransferError(result.message ?? `the node answered ${result.status}`);

    failures = 0;
    offset = result.offset;
    state = result.state ?? state;
    onProgress(offset);
  }
  return toOutcome(state);
}
