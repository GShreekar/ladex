import type { GuestMe } from '../client/guest-client';

// Longer than any typed code, so it can only be the token from a QR link.
const TYPED_CODE_MAX_CHARS = 12;

export interface InvitationFragment {
  token: string | null;
  fingerprint: string | null;
}

/** Reads the invitation out of the address bar and removes it from there at once. */
export function takeInvitationFragment(): InvitationFragment {
  const raw = location.hash.slice(1);
  if (!raw) return { token: null, fingerprint: null };
  history.replaceState(null, '', location.pathname);
  const fields = new URLSearchParams(raw);
  if (!fields.has('t')) return { token: raw, fingerprint: null };
  return { token: fields.get('t'), fingerprint: fields.get('fp') };
}

/** The fingerprint this node's certificate has, as the node itself reports it on the page. */
export function pageFingerprint(): string | null {
  const value = document.querySelector<HTMLMetaElement>('meta[name="ladex-fingerprint"]')?.content ?? '';
  return value && !value.includes('{{') ? value : null;
}

export async function fetchMe(): Promise<GuestMe | null> {
  const response = await fetch('/api/me', { cache: 'no-store' });
  return response.ok ? ((await response.json()) as GuestMe) : null;
}

/** Redeems an invitation; resolves with the reason it was refused, or null when it worked. */
export async function redeemInvitation(secret: string): Promise<string | null> {
  const field = secret.length > TYPED_CODE_MAX_CHARS ? 'token' : 'code';
  try {
    const response = await fetch('/join', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ [field]: secret }),
    });
    if (response.ok) return null;
    const body = (await response.json().catch(() => ({}))) as { message?: string };
    return body.message ?? 'Could not join.';
  } catch {
    return 'Could not reach the host.';
  }
}
