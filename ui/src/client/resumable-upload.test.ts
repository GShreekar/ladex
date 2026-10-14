import { describe, expect, it } from 'vitest';
import { MAX_RETRIES, runResumableUpload, SignedOut, type SendResult, type UploadProgress, type UploadWire } from './resumable-upload';
import { TransferCancelled, TransferError } from './types';

const OK: SendResult = { status: 201, isNetworkFailure: false, isAborted: false, reply: { complete: true, file: { id: 'file_1' } } };
const NETWORK_DROP: SendResult = { status: 0, isNetworkFailure: true, isAborted: false, reply: {} };
const noPause = async (): Promise<void> => undefined;

function scriptedWire(progress: UploadProgress[], sends: SendResult[]): UploadWire & { sentFrom: number[] } {
  const sentFrom: number[] = [];
  return {
    sentFrom,
    queryProgress: async () => progress.shift() ?? { offset: 0, isComplete: false },
    sendFrom: async (offset) => {
      sentFrom.push(offset);
      return sends.shift() ?? OK;
    },
  };
}

function options(signal: AbortSignal = new AbortController().signal) {
  return { signal, onProgress: () => undefined, fileId: 'file_1', size: 100, pause: noPause };
}

describe('runResumableUpload', () => {
  it('sends the whole file from the start when the node has nothing', async () => {
    const wire = scriptedWire([{ offset: 0, isComplete: false }], [OK]);
    await runResumableUpload(wire, options());
    expect(wire.sentFrom).toEqual([0]);
  });

  it('returns at once when the node already has the complete file', async () => {
    const wire = scriptedWire([{ offset: 100, isComplete: true }], []);
    const reply = await runResumableUpload(wire, options());
    expect(reply.file?.id).toBe('file_1');
    expect(wire.sentFrom).toEqual([]);
  });

  it('carries on from the offset the node reports after the network drops', async () => {
    const wire = scriptedWire(
      [
        { offset: 0, isComplete: false },
        { offset: 4 * 1024 * 1024, isComplete: false },
      ],
      [NETWORK_DROP, OK],
    );
    await runResumableUpload(wire, options());
    expect(wire.sentFrom).toEqual([0, 4 * 1024 * 1024]);
  });

  it('realigns when the node answers 409 with the offset it can resume from', async () => {
    const conflict: SendResult = { status: 409, isNetworkFailure: false, isAborted: false, reply: { offset: 1048576 } };
    const wire = scriptedWire(
      [
        { offset: 0, isComplete: false },
        { offset: 1048576, isComplete: false },
      ],
      [conflict, OK],
    );
    await runResumableUpload(wire, options());
    expect(wire.sentFrom).toEqual([0, 1048576]);
  });

  it('fails with the node’s own message when it refuses the upload', async () => {
    const refusal: SendResult = { status: 507, isNetworkFailure: false, isAborted: false, reply: { message: 'no room left' } };
    const wire = scriptedWire([{ offset: 0, isComplete: false }], [refusal]);
    await expect(runResumableUpload(wire, options())).rejects.toThrow('no room left');
  });

  it('reports a signed-out session so the page can ask for a login', async () => {
    const unauthorized: SendResult = { status: 401, isNetworkFailure: false, isAborted: false, reply: {} };
    const wire = scriptedWire([{ offset: 0, isComplete: false }], [unauthorized]);
    await expect(runResumableUpload(wire, options())).rejects.toBeInstanceOf(SignedOut);
  });

  it('gives up with a readable error after too many dropped connections', async () => {
    const drops = Array.from({ length: MAX_RETRIES + 2 }, () => NETWORK_DROP);
    const wire = scriptedWire([], drops);
    await expect(runResumableUpload(wire, options())).rejects.toThrow(TransferError);
  });

  it('stops without sending when already cancelled', async () => {
    const controller = new AbortController();
    controller.abort();
    const wire = scriptedWire([], []);
    await expect(runResumableUpload(wire, options(controller.signal))).rejects.toBeInstanceOf(TransferCancelled);
  });

  it('reports full progress once the node confirms the file', async () => {
    const reported: number[] = [];
    const wire = scriptedWire([{ offset: 0, isComplete: false }], [OK]);
    await runResumableUpload(wire, { ...options(), onProgress: (bytes) => reported.push(bytes) });
    expect(reported).toEqual([100]);
  });
});
