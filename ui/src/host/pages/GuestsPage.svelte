<script lang="ts">
  import { onMount } from 'svelte';
  import { formatAgo, formatBytes } from '../../client/format';
  import { TransferError } from '../../client/types';
  import Icon from '../../components/Icon.svelte';
  import QrCode from '../../components/QrCode.svelte';
  import { clock } from '../../state/clock.svelte';
  import { useApp } from '../../state/context';
  import {
    answerUpload,
    issueInvitation,
    listGuests,
    listWaitingUploads,
    PERMISSION_LABELS,
    signOutGuest,
    type GuestPermission,
    type GuestSummary,
    type Invitation,
    type WaitingUpload,
  } from '../guests-api';

  const { toasts } = useApp();

  const REFRESH_MS = 3000;
  const permissions: GuestPermission[] = ['read-only', 'upload', 'upload-with-approval'];

  let permission = $state<GuestPermission>('read-only');
  let invitation = $state<Invitation | null>(null);
  let issuedAt = $state(0);
  let guests = $state<GuestSummary[]>([]);
  let waiting = $state<WaitingUpload[]>([]);
  let isIssuing = $state(false);

  const secondsLeft = $derived(
    invitation ? Math.max(0, invitation.expires_in_secs - Math.floor((clock.now - issuedAt) / 1000)) : 0,
  );

  async function refresh(): Promise<void> {
    try {
      [guests, waiting] = await Promise.all([listGuests(), listWaitingUploads()]);
    } catch {
      // The next refresh tries again.
    }
  }

  onMount(() => {
    void refresh();
    const timer = setInterval(refresh, REFRESH_MS);
    return () => clearInterval(timer);
  });

  function reportFailure(error: unknown, fallback: string): void {
    toasts.show('error', error instanceof TransferError ? error.message : fallback);
  }

  async function invite(): Promise<void> {
    isIssuing = true;
    try {
      invitation = await issueInvitation(permission);
      issuedAt = Date.now();
    } catch (error) {
      reportFailure(error, 'Could not create an invitation.');
    } finally {
      isIssuing = false;
    }
  }

  async function signOut(guest: GuestSummary): Promise<void> {
    try {
      await signOutGuest(guest.id);
      await refresh();
    } catch (error) {
      reportFailure(error, 'Could not sign that guest out.');
    }
  }

  async function answer(upload: WaitingUpload, accepted: boolean): Promise<void> {
    try {
      await answerUpload(upload.id, accepted);
      toasts.show('success', accepted ? `Accepted ${upload.name}` : `Declined ${upload.name}`);
      await refresh();
    } catch (error) {
      reportFailure(error, 'Could not answer that upload.');
    }
  }

  async function copy(text: string): Promise<void> {
    try {
      await navigator.clipboard.writeText(text);
      toasts.show('success', 'Copied');
    } catch {
      toasts.show('error', 'Could not copy. Select the text and copy it by hand.');
    }
  }

  function formatCountdown(seconds: number): string {
    return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
  }
</script>

<div class="title">
  <h1>Guests</h1>
  <p class="muted">Let someone join from a browser, with nothing to install.</p>
</div>

<div class="layout">
  <section class="card invite">
    <h2>Invite someone with a browser</h2>

    <div class="roles" role="radiogroup" aria-label="What the guest may do">
      {#each permissions as option (option)}
        <button role="radio" aria-checked={permission === option} class:active={permission === option} onclick={() => (permission = option)}>
          {PERMISSION_LABELS[option]}
        </button>
      {/each}
    </div>

    <button class="btn btn-primary" disabled={isIssuing} onclick={invite}>
      <Icon name={invitation ? 'refresh' : 'qr'} /> {invitation ? 'Generate a new invitation' : 'Create invitation'}
    </button>

    {#if invitation}
      <div class="ticket" class:expired={secondsLeft === 0}>
        {#if invitation.url}
          <div class="qr"><QrCode text={invitation.url} label="Invitation QR code" /></div>
          <p class="small faint">Scan with the phone camera</p>
          <div class="copy-row">
            <Icon name="link" size={16} />
            <code class="truncate">{invitation.url.split('#')[0]}</code>
            <button class="btn btn-ghost btn-icon" aria-label="Copy the invitation link" onclick={() => copy(invitation?.url ?? '')}>
              <Icon name="copy" size={16} />
            </button>
          </div>
        {/if}
        <div class="code-box">
          <span class="small faint">TYPED CODE</span>
          <strong class="code">{invitation.code}</strong>
          <span class="small" class:danger={secondsLeft === 0}>
            {secondsLeft > 0 ? `Expires in ${formatCountdown(secondsLeft)} · works once` : 'Expired. Generate a new one.'}
          </span>
        </div>
        {#if invitation.certificate_fingerprint}
          <div class="fingerprint">
            <div class="small"><Icon name="shield" size={14} /> This node's certificate fingerprint (SHA-256)</div>
            <code>{invitation.certificate_fingerprint}</code>
            <p class="small faint">
              A browser will warn that the certificate is self-signed. Proceed only if its fingerprint matches this one.
            </p>
          </div>
        {/if}
      </div>
    {/if}
  </section>

  <div class="column">
    {#if waiting.length > 0}
      <section class="card waiting">
        <h2>{waiting.length} upload{waiting.length === 1 ? '' : 's'} waiting for you</h2>
        <ul>
          {#each waiting as upload (upload.id)}
            <li>
              <div class="who">
                <div class="truncate"><strong>{upload.name}</strong></div>
                <div class="small faint">{formatBytes(upload.size)} · {upload.guest ?? 'a guest'}</div>
              </div>
              <button class="btn btn-small" onclick={() => answer(upload, false)}>Decline</button>
              <button class="btn btn-primary btn-small" onclick={() => answer(upload, true)}><Icon name="check" size={15} /> Accept</button>
            </li>
          {/each}
        </ul>
      </section>
    {/if}

    <section class="card connected">
      <h2>Connected guests <span class="badge badge-success">{guests.length} active</span></h2>
      {#if guests.length === 0}
        <p class="muted empty">Nobody has joined yet.</p>
      {:else}
        <ul>
          {#each guests as guest (guest.id)}
            <li>
              <span class="device"><Icon name="phone" size={20} /></span>
              <div class="who">
                <div class="truncate"><strong>{guest.label}</strong></div>
                <div class="small faint">
                  <span class="badge badge-accent">{PERMISSION_LABELS[guest.permission]}</span>
                  {guest.ip} · joined {formatAgo(Date.parse(guest.created_at), clock.now)}
                </div>
              </div>
              <button class="btn btn-small" onclick={() => signOut(guest)}><Icon name="log-out" size={15} /> Sign out</button>
            </li>
          {/each}
        </ul>
      {/if}
    </section>

    <section class="note">
      <Icon name="shield" size={20} />
      <p class="small muted">
        Guests only see what has been shared with the room. They can't reach other devices, and each invitation works once.
      </p>
    </section>
  </div>
</div>

<style>
  .title {
    margin-bottom: 20px;
  }

  .layout {
    display: grid;
    grid-template-columns: minmax(0, 420px) minmax(0, 1fr);
    gap: 24px;
    align-items: start;
  }

  .invite,
  .waiting,
  .connected {
    padding: 20px;
  }

  .invite {
    display: flex;
    flex-direction: column;
    gap: 16px;
  }

  .column {
    display: flex;
    flex-direction: column;
    gap: 24px;
  }

  .roles {
    display: flex;
    gap: 2px;
    padding: 3px;
    border-radius: var(--radius-sm);
    background: var(--surface-low);
  }

  .roles button {
    flex: 1;
    padding: 6px 8px;
    border: 0;
    border-radius: 6px;
    background: transparent;
    color: var(--text-muted);
    font-size: 12px;
  }

  .roles button.active {
    background: var(--surface);
    color: var(--accent-text);
    font-weight: 500;
  }

  .ticket {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 12px;
  }

  .ticket.expired .qr {
    opacity: 0.35;
  }

  .qr {
    width: 220px;
  }

  .copy-row {
    display: flex;
    align-items: center;
    gap: 8px;
    width: 100%;
    padding: 4px 4px 4px 12px;
    border-radius: var(--radius-sm);
    background: var(--surface-low);
  }

  .copy-row code {
    flex: 1;
  }

  .code-box {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 4px;
    width: 100%;
    padding: 12px;
    border-radius: var(--radius-md);
    background: var(--accent-tint);
  }

  .code {
    color: var(--on-accent-tint);
    font-family: ui-monospace, 'SFMono-Regular', Menlo, monospace;
    font-size: 24px;
    letter-spacing: 0.12em;
  }

  .danger {
    color: var(--danger);
  }

  .fingerprint {
    width: 100%;
    padding: 12px;
    border-radius: var(--radius-sm);
    background: var(--surface-low);
  }

  .fingerprint code {
    display: block;
    margin: 6px 0;
    font-size: 11px;
    overflow-wrap: anywhere;
  }

  h2 {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  ul {
    margin: 12px 0 0;
    padding: 0;
    list-style: none;
  }

  li {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 12px 0;
  }

  li + li {
    border-top: 1px solid var(--border);
  }

  .who {
    flex: 1;
    min-width: 0;
  }

  .device {
    display: grid;
    place-items: center;
    width: 36px;
    height: 36px;
    border-radius: var(--radius-sm);
    background: var(--surface-low);
    color: var(--text-muted);
  }

  .waiting {
    background: var(--accent-tint);
    border-color: transparent;
  }

  .empty {
    margin-top: 12px;
  }

  .note {
    display: flex;
    gap: 12px;
    padding: 16px;
    border-radius: var(--radius-md);
    background: var(--surface-low);
    color: var(--accent-text);
  }

  @media (max-width: 1000px) {
    .layout {
      grid-template-columns: minmax(0, 1fr);
    }
  }
</style>
