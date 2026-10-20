import type { NodeTransport } from '../client/transport';
import { callNode } from './node-api';

export type GuestPermission = 'read-only' | 'upload' | 'upload-with-approval';

export interface Invitation {
  url: string | null;
  code: string;
  permission: GuestPermission;
  expires_in_secs: number;
  certificate_fingerprint: string | null;
}

export interface GuestSummary {
  id: string;
  label: string;
  ip: string;
  permission: GuestPermission;
  created_at: string;
  last_seen: string;
}

export interface WaitingUpload {
  id: string;
  guest: string | null;
  name: string;
  size: number;
  mime_type: string;
  waiting_secs: number;
}

export const PERMISSION_LABELS: Record<GuestPermission, string> = {
  'read-only': 'Download only',
  upload: 'Download & upload',
  'upload-with-approval': 'Needs approval',
};

/** What the person at the node's machine does about guests: invite them, see them, sign them out, answer their uploads. */
export class GuestsApi {
  constructor(private readonly transport: NodeTransport) {}

  issueInvitation(permission: GuestPermission): Promise<Invitation> {
    return this.call<Invitation>('POST', '/api/guests/invites', { permission });
  }

  async listGuests(): Promise<GuestSummary[]> {
    return (await this.call<{ guests: GuestSummary[] }>('GET', '/api/guests/sessions')).guests;
  }

  async signOutGuest(id: string): Promise<void> {
    await this.call('DELETE', `/api/guests/sessions/${encodeURIComponent(id)}`);
  }

  async listWaitingUploads(): Promise<WaitingUpload[]> {
    return (await this.call<{ uploads: WaitingUpload[] }>('GET', '/api/guests/uploads')).uploads;
  }

  async answerUpload(id: string, accepted: boolean): Promise<void> {
    await this.call('POST', `/api/guests/uploads/${encodeURIComponent(id)}/${accepted ? 'approve' : 'deny'}`);
  }

  private call<T>(method: string, path: string, body?: unknown): Promise<T> {
    return callNode<T>(this.transport, method, path, body);
  }
}
