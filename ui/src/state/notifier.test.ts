import { beforeEach, describe, expect, it } from 'vitest';
import type { ClientEvent, ClientListener, Device, IncomingOffer, LadexClient, SharedFile } from '../client/types';
import type { DesktopShell } from '../desktop/desktop';
import { Notifier } from './notifier';

class FakeDesktop implements DesktopShell {
  notifications: Array<{ title: string; body: string }> = [];
  otherDeviceCounts: number[] = [];

  notify(title: string, body: string): void {
    this.notifications.push({ title, body });
  }

  setOtherDevices(count: number): void {
    this.otherDeviceCounts.push(count);
  }
}

function file(overrides: Partial<SharedFile>): SharedFile {
  return {
    id: 'file_a',
    name: 'slides.pdf',
    mimeType: 'application/pdf',
    isFolder: false,
    sizeBytes: 1000,
    folderFileCount: 0,
    sharedAt: 0,
    sharedBy: 'Alex',
    isOwn: false,
    holderNames: [],
    isAvailable: true,
    canUnshare: false,
    needsThisDeviceOnline: false,
    ...overrides,
  };
}

function device(overrides: Partial<Device>): Device {
  return { id: 'device_a', name: 'Alex', nodeName: 'alex-laptop', nodeId: 'node_a', kind: 'laptop', isSelf: false, latencyMs: null, ...overrides };
}

const offer: IncomingOffer = { fileId: 'file_a', fileName: 'holiday.jpg', sizeBytes: 10, senderName: 'Sam', senderId: 'device_s', isDirect: true };

describe('Notifier', () => {
  let desktop: FakeDesktop;
  let send: ClientListener;

  beforeEach(() => {
    desktop = new FakeDesktop();
    const client = { subscribe: (listener: ClientListener) => ((send = listener), () => {}) } as unknown as LadexClient;
    new Notifier(client, desktop);
  });

  const deliver = (event: ClientEvent) => send(event);

  it('does_not_announce_files_that_were_already_in_the_room', () => {
    deliver({ type: 'files', files: [file({ id: 'old' })] });
    expect(desktop.notifications).toEqual([]);
  });

  it('announces_a_file_another_device_shares', () => {
    deliver({ type: 'files', files: [] });
    deliver({ type: 'files', files: [file({ id: 'new', name: 'notes.txt' })] });
    expect(desktop.notifications).toEqual([{ title: 'Alex shared notes.txt', body: 'Open LADEX to download it.' }]);
  });

  it('counts_several_files_from_one_device_in_one_notification', () => {
    deliver({ type: 'files', files: [] });
    deliver({ type: 'files', files: [file({ id: 'one' }), file({ id: 'two' }), file({ id: 'three' })] });
    expect(desktop.notifications.map((notification) => notification.title)).toEqual(['Alex shared 3 files']);
  });

  it('does_not_announce_what_this_device_shared_itself', () => {
    deliver({ type: 'files', files: [] });
    deliver({ type: 'files', files: [file({ id: 'mine', isOwn: true, canUnshare: true })] });
    expect(desktop.notifications).toEqual([]);
  });

  it('announces_a_guest_upload_to_this_node_even_though_this_machine_may_unshare_it', () => {
    deliver({ type: 'files', files: [] });
    deliver({ type: 'files', files: [file({ id: 'guest', sharedBy: 'Phone', canUnshare: true })] });
    expect(desktop.notifications.map((notification) => notification.title)).toEqual(['Phone shared slides.pdf']);
  });

  it('treats_the_catalog_after_a_reconnect_as_a_catch_up', () => {
    deliver({ type: 'files', files: [] });
    deliver({ type: 'connection', state: 'offline' });
    deliver({ type: 'connection', state: 'online' });
    deliver({ type: 'files', files: [file({ id: 'missed' })] });
    expect(desktop.notifications).toEqual([]);
  });

  it('announces_an_offer_with_who_sent_it_and_what', () => {
    deliver({ type: 'offer', offer });
    expect(desktop.notifications).toEqual([{ title: 'Sam wants to send you a file', body: 'holiday.jpg' }]);
  });

  it('reports_the_other_devices_only_when_their_number_changes', () => {
    deliver({ type: 'devices', devices: [device({ isSelf: true }), device({})] });
    deliver({ type: 'devices', devices: [device({ isSelf: true }), device({ latencyMs: 5 })] });
    deliver({ type: 'devices', devices: [device({ isSelf: true })] });
    expect(desktop.otherDeviceCounts).toEqual([1, 0]);
  });
});
