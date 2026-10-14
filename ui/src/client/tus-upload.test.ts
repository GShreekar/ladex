import { describe, expect, it } from 'vitest';
import { PIECE_BYTES, runTusUpload, SessionEnded, type TusPatchResult, type TusState, type TusWire } from './tus-upload';
import { TransferCancelled, TransferError } from './types';

const noPause = async (): Promise<void> => undefined;

function accepted(offset: number, state: TusState = 'receiving'): TusPatchResult {
  return { status: 204, isNetworkFailure: false, isAborted: false, offset, state, message: null };
}

const NETWORK_DROP: TusPatchResult = { status: 0, isNetworkFailure: true, isAborted: false, offset: null, state: null, message: null };

function fakeFile(size: number): File {
  return new File([new Uint8Array(size)], 'photo.jpg', { type: 'image/jpeg' });
}

function scriptedWire(patches: TusPatchResult[], headOffsets: Array<number | null> = []) {
  const patchedAt: number[] = [];
  const events: string[] = [];
  const wire: TusWire = {
    create: async () => ({ location: '/uploads/abc', state: 'receiving' }),
    patch: async (_location, offset) => {
      patchedAt.push(offset);
      return patches.shift() ?? accepted(offset);
    },
    head: async () => headOffsets.shift() ?? null,
    cancel: async () => {
      events.push('cancelled');
    },
  };
  return { wire, patchedAt, events };
}

function options(file: File, signal: AbortSignal = new AbortController().signal) {
  return { file, signal, onProgress: () => undefined, pause: noPause };
}

describe('runTusUpload', () => {
  it('uploads a small file in one piece and reports it shared', async () => {
    const { wire, patchedAt } = scriptedWire([accepted(1000, 'shared')]);
    const outcome = await runTusUpload(wire, options(fakeFile(1000)));
    expect(outcome).toBe('shared');
    expect(patchedAt).toEqual([0]);
  });

  it('sends a large file in pieces, each starting where the node says the last ended', async () => {
    const size = PIECE_BYTES * 2 + 5;
    const { wire, patchedAt } = scriptedWire([accepted(PIECE_BYTES), accepted(PIECE_BYTES * 2), accepted(size, 'shared')]);
    await runTusUpload(wire, options(fakeFile(size)));
    expect(patchedAt).toEqual([0, PIECE_BYTES, PIECE_BYTES * 2]);
  });

  it('reports a file held for the host’s approval as such', async () => {
    const { wire } = scriptedWire([accepted(1000, 'awaiting-approval')]);
    expect(await runTusUpload(wire, options(fakeFile(1000)))).toBe('awaiting-approval');
  });

  it('asks the node where it got to after the network drops and resumes from there', async () => {
    const size = PIECE_BYTES * 2;
    const { wire, patchedAt } = scriptedWire([accepted(PIECE_BYTES), NETWORK_DROP, accepted(size, 'shared')], [PIECE_BYTES]);
    await runTusUpload(wire, options(fakeFile(size)));
    expect(patchedAt).toEqual([0, PIECE_BYTES, PIECE_BYTES]);
  });

  it('realigns to the offset in a 409 answer', async () => {
    const conflict: TusPatchResult = { status: 409, isNetworkFailure: false, isAborted: false, offset: 0, state: 'receiving', message: null };
    const { wire, patchedAt } = scriptedWire([conflict, accepted(1000, 'shared')]);
    await runTusUpload(wire, options(fakeFile(1000)));
    expect(patchedAt).toEqual([0, 0]);
  });

  it('fails with the host’s message when it refuses a piece', async () => {
    const refusal: TusPatchResult = { status: 507, isNetworkFailure: false, isAborted: false, offset: null, state: null, message: 'disk full' };
    const { wire } = scriptedWire([refusal]);
    await expect(runTusUpload(wire, options(fakeFile(1000)))).rejects.toThrow('disk full');
  });

  it('reports an ended session when the node answers 401', async () => {
    const unauthorized: TusPatchResult = { status: 401, isNetworkFailure: false, isAborted: false, offset: null, state: null, message: null };
    const { wire } = scriptedWire([unauthorized]);
    await expect(runTusUpload(wire, options(fakeFile(1000)))).rejects.toBeInstanceOf(SessionEnded);
  });

  it('gives up after repeated dropped connections', async () => {
    const drops = Array.from({ length: 10 }, () => NETWORK_DROP);
    const { wire } = scriptedWire(drops);
    await expect(runTusUpload(wire, options(fakeFile(1000)))).rejects.toBeInstanceOf(TransferError);
  });

  it('removes the partial upload from the node when cancelled', async () => {
    const controller = new AbortController();
    controller.abort();
    const { wire, events } = scriptedWire([]);
    await expect(runTusUpload(wire, options(fakeFile(1000), controller.signal))).rejects.toBeInstanceOf(TransferCancelled);
    expect(events).toEqual(['cancelled']);
  });

  it('needs no pieces for an empty file', async () => {
    const { wire, patchedAt } = scriptedWire([]);
    await runTusUpload(wire, options(fakeFile(0)));
    expect(patchedAt).toEqual([]);
  });
});
