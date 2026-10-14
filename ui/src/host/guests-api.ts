import { TransferError } from '../client/types';

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

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(path, { cache: 'no-store', ...init });
  const body = (await response.json().catch(() => ({}))) as T & { message?: string };
  if (!response.ok) throw new TransferError(body.message ?? `the node answered ${response.status}`);
  return body;
}

export async function issueInvitation(permission: GuestPermission): Promise<Invitation> {
  return request<Invitation>('/api/guests/invites', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ permission }),
  });
}

export async function listGuests(): Promise<GuestSummary[]> {
  return (await request<{ guests: GuestSummary[] }>('/api/guests/sessions')).guests;
}

export async function signOutGuest(id: string): Promise<void> {
  await request(`/api/guests/sessions/${encodeURIComponent(id)}`, { method: 'DELETE' });
}

export async function listWaitingUploads(): Promise<WaitingUpload[]> {
  return (await request<{ uploads: WaitingUpload[] }>('/api/guests/uploads')).uploads;
}

export async function answerUpload(id: string, accepted: boolean): Promise<void> {
  await request(`/api/guests/uploads/${encodeURIComponent(id)}/${accepted ? 'approve' : 'deny'}`, { method: 'POST' });
}
