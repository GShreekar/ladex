import { describe, expect, it } from 'vitest';
import type { IncomingFile, PhoneShell, ShareProgress } from '../desktop/phone';
import type { NoticeLevel } from '../client/types';
import { IncomingShare } from './incoming-share.svelte';

const BEACH: IncomingFile = { id: 'share1-0', name: 'beach.jpg', size: 2048, mimeType: 'image/jpeg' };
const VOICE: IncomingFile = { id: 'share1-1', name: 'voice.ogg', size: null, mimeType: 'audio/ogg' };

class FakePhone implements PhoneShell {
  waiting: IncomingFile[] = [];
  setAside = 0;
  shareResult: number | Error = 1;
  private listener: ((files: IncomingFile[]) => void) | null = null;

  async incomingFiles(): Promise<IncomingFile[]> {
    return this.waiting;
  }

  async onIncomingFiles(listener: (files: IncomingFile[]) => void): Promise<() => void> {
    this.listener = listener;
    return () => (this.listener = null);
  }

  shareIntoTheApp(files: IncomingFile[]): void {
    this.listener?.(files);
  }

  async shareIncoming(_onProgress: (progress: ShareProgress) => void): Promise<number> {
    if (this.shareResult instanceof Error) throw this.shareResult.message;
    return this.shareResult;
  }

  async setIncomingAside(): Promise<void> {
    this.setAside += 1;
  }

  async saveFolder(): Promise<string | null> {
    return null;
  }

  async pickSaveFolder(): Promise<string | null> {
    return null;
  }

  async forgetSaveFolder(): Promise<void> {}
}

class FakeToasts {
  shown: Array<{ level: NoticeLevel; text: string }> = [];

  show(level: NoticeLevel, text: string): void {
    this.shown.push({ level, text });
  }
}

async function started(phone: FakePhone, toasts = new FakeToasts()): Promise<IncomingShare> {
  const incoming = new IncomingShare(phone, toasts);
  await incoming.start();
  return incoming;
}

describe('IncomingShare', () => {
  it('offers the files shared into the app before the page opened', async () => {
    const phone = new FakePhone();
    phone.waiting = [BEACH];
    expect((await started(phone)).files).toEqual([BEACH]);
  });

  it('offers files shared into the app while it is open', async () => {
    const phone = new FakePhone();
    const incoming = await started(phone);
    phone.shareIntoTheApp([BEACH, VOICE]);
    expect(incoming.files.map((file) => file.name)).toEqual(['beach.jpg', 'voice.ogg']);
  });

  it('adds up the sizes it knows and says when one is unknown', async () => {
    const phone = new FakePhone();
    phone.waiting = [BEACH, VOICE];
    const incoming = await started(phone);
    expect([incoming.knownBytes, incoming.hasUnknownSizes]).toEqual([2048, true]);
  });

  it('names the one file it shared', async () => {
    const phone = new FakePhone();
    const toasts = new FakeToasts();
    phone.waiting = [BEACH];
    const incoming = await started(phone, toasts);
    await incoming.share();
    expect(toasts.shown).toEqual([{ level: 'success', text: 'Shared beach.jpg with everyone' }]);
  });

  it('is empty again once the files are shared', async () => {
    const phone = new FakePhone();
    phone.waiting = [BEACH, VOICE];
    phone.shareResult = 2;
    const incoming = await started(phone);
    await incoming.share();
    expect([incoming.files, incoming.isSharing]).toEqual([[], false]);
  });

  it('shows the progress the app reports while sharing', async () => {
    const phone = new FakePhone();
    phone.waiting = [BEACH];
    const incoming = await started(phone);
    const seen: Array<ShareProgress | null> = [];
    phone.shareIncoming = async (onProgress) => {
      onProgress({ done: 1024, total: 2048 });
      seen.push(incoming.progress);
      return 1;
    };
    await incoming.share();
    expect(seen).toEqual([{ done: 1024, total: 2048 }]);
  });

  it('says why sharing failed, in the words the app gave', async () => {
    const phone = new FakePhone();
    const toasts = new FakeToasts();
    phone.waiting = [BEACH];
    phone.shareResult = new Error('could not share beach.jpg: there is no room for it');
    const incoming = await started(phone, toasts);
    await incoming.share();
    expect(toasts.shown).toEqual([{ level: 'error', text: 'Could not share beach.jpg: there is no room for it' }]);
  });

  it('sets the files aside on the phone when the person declines', async () => {
    const phone = new FakePhone();
    phone.waiting = [BEACH];
    const incoming = await started(phone);
    await incoming.setAside();
    expect([incoming.files, phone.setAside]).toEqual([[], 1]);
  });

  it('stops listening once stopped', async () => {
    const phone = new FakePhone();
    const incoming = await started(phone);
    incoming.stop();
    phone.shareIntoTheApp([BEACH]);
    expect(incoming.files).toEqual([]);
  });
});
