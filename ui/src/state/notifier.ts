import type { ClientEvent, LadexClient, SharedFile } from '../client/types';
import type { DesktopShell } from '../desktop/desktop';

/** Tells the person, outside the window, about what happens in the room while they are not looking. */
export class Notifier {
  // Null until the room's catalog has been seen once, so files that were already there are not announced.
  private knownFileIds: Set<string> | null = null;
  private otherDeviceCount: number | null = null;

  constructor(
    client: LadexClient,
    private readonly desktop: DesktopShell,
  ) {
    client.subscribe((event) => this.apply(event));
  }

  downloadSaved(fileName: string): void {
    this.desktop.notify('Download complete', fileName);
  }

  private apply(event: ClientEvent): void {
    switch (event.type) {
      case 'connection':
        // The catalog sent after a reconnect is a catch-up, not news.
        if (event.state !== 'online') this.knownFileIds = null;
        break;
      case 'devices':
        this.showOtherDevices(event.devices.filter((device) => !device.isSelf).length);
        break;
      case 'files':
        this.announceNewFiles(event.files);
        break;
      case 'offer':
        this.desktop.notify(`${event.offer.senderName} wants to send you a file`, event.offer.fileName);
        break;
    }
  }

  private showOtherDevices(count: number): void {
    if (count === this.otherDeviceCount) return;
    this.otherDeviceCount = count;
    this.desktop.setOtherDevices(count);
  }

  private announceNewFiles(files: SharedFile[]): void {
    const known = this.knownFileIds;
    this.knownFileIds = new Set(files.map((file) => file.id));
    if (!known) return;
    const fromOthers = files.filter((file) => !known.has(file.id) && !file.isOwn);
    for (const [sharer, shared] of groupBySharer(fromOthers)) {
      const [first] = shared;
      const what = shared.length === 1 && first ? first.name : `${shared.length} files`;
      this.desktop.notify(`${sharer} shared ${what}`, 'Open LADEX to download it.');
    }
  }
}

function groupBySharer(files: SharedFile[]): Map<string, SharedFile[]> {
  const groups = new Map<string, SharedFile[]>();
  for (const file of files) {
    groups.set(file.sharedBy, [...(groups.get(file.sharedBy) ?? []), file]);
  }
  return groups;
}
