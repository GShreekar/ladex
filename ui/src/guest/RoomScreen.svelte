<script lang="ts">
  import { onMount } from 'svelte';
  import { GuestClient } from '../client/guest-client';
  import { formatAgo, formatBytes, formatSpeed } from '../client/format';
  import { iconFor } from '../components/file-icon';
  import Icon from '../components/Icon.svelte';
  import { clock } from '../state/clock.svelte';
  import { provideApp } from '../state/context';
  import { Room } from '../state/room.svelte';
  import type { Toasts } from '../state/toasts.svelte';
  import { Transfers, type TransferStatus } from '../state/transfers.svelte';

  let { client, toasts, onended }: { client: GuestClient; toasts: Toasts; onended: () => void } = $props();

  // The parent keys this screen by session, so the client and toasts never change while it lives.
  // svelte-ignore state_referenced_locally
  const room = new Room(client, toasts);
  // svelte-ignore state_referenced_locally
  const transfers = new Transfers(client, toasts);
  // svelte-ignore state_referenced_locally
  provideApp({ room, transfers, toasts });

  let fileChooser = $state<HTMLInputElement>();

  const permissionLabel = $derived(
    room.capabilities.canUpload
      ? room.capabilities.uploadNeedsApproval
        ? 'Can upload with the host’s approval'
        : 'Can download and upload'
      : 'Can download only',
  );

  const STATUS_LABELS: Record<TransferStatus, string> = {
    queued: 'Waiting',
    sending: 'Uploading',
    done: 'Shared with the room',
    'awaiting-approval': 'Waiting for the host to accept it',
    offered: 'Sent',
    failed: 'Failed',
    cancelled: 'Cancelled',
  };

  onMount(() => {
    void room.start();
    return () => room.dispose();
  });

  $effect(() => {
    if (room.connection === 'ended') onended();
  });

  function chooseFiles(event: Event): void {
    const input = event.target as HTMLInputElement;
    void transfers.share([...(input.files ?? [])]);
    input.value = '';
  }
</script>

<main>
  <header>
    <div class="brand">
      <span class="mark"><Icon name="hub" size={20} /></span>
      <strong>LADEX</strong>
      <span class="badge badge-accent">Guest</span>
    </div>
    <button class="btn btn-ghost" onclick={() => room.client.leave()}><Icon name="log-out" /> Leave</button>
  </header>

  <section class="host card">
    <div class="host-line">
      <h1 class="truncate">{room.self.nodeName || 'The host'}</h1>
      <span class="badge" class:badge-success={room.connection === 'online'} class:badge-warning={room.connection !== 'online'}>
        {room.connection === 'online' ? 'Live' : 'Reconnecting…'}
      </span>
    </div>
    <p class="small muted">Joined as <strong>{room.self.name}</strong> · {permissionLabel}</p>
  </section>

  {#if room.connection === 'offline'}
    <div class="banner warning" role="status">
      <Icon name="wifi-off" size={20} />
      <p>The host isn't answering right now. This page keeps trying and downloads resume when it's back.</p>
    </div>
  {/if}

  <section>
    <div class="section-head">
      <h2>Shared with the room</h2>
      <span class="badge">{room.files.length}</span>
    </div>

    {#each room.files as file (file.id)}
      <article class="card file">
        <div class="file-head">
          <span class="icon"><Icon name={iconFor(file)} size={22} /></span>
          <div class="meta">
            <div class="name">{file.name}</div>
            <div class="small muted">
              {file.isFolder ? `${file.folderFileCount} items · ` : ''}{formatBytes(file.sizeBytes)} · {file.sharedBy} · {formatAgo(file.sharedAt, clock.now)}
            </div>
          </div>
        </div>
        <div class="badges">
          {#if file.isAvailable}
            <span class="badge badge-success"><Icon name="hub" size={13} /> {file.holderNames.length} {file.holderNames.length === 1 ? 'device has' : 'devices have'} it</span>
          {:else}
            <span class="badge badge-warning"><Icon name="wifi-off" size={13} /> Host offline</span>
          {/if}
        </div>
        <button class="btn btn-block" class:btn-primary={file.isAvailable} disabled={!file.isAvailable} onclick={() => room.download(file)}>
          <Icon name="download" /> {file.isFolder ? 'Download as ZIP' : 'Download file'}
        </button>
      </article>
    {:else}
      <div class="card empty">
        <Icon name="files" size={28} />
        <p class="muted">Nothing is shared yet.</p>
      </div>
    {/each}
  </section>

  <section>
    <div class="section-head"><h2>Share from this device</h2></div>
    {#if room.capabilities.canUpload}
      <button class="btn btn-primary btn-block" onclick={() => fileChooser?.click()}><Icon name="upload" /> Choose files to share</button>
      <input bind:this={fileChooser} type="file" multiple hidden onchange={chooseFiles} />

      {#each transfers.items as transfer (transfer.id)}
        <article class="card upload">
          <div class="upload-line">
            <div class="truncate"><strong>{transfer.name}</strong></div>
            {#if transfer.status === 'sending' || transfer.status === 'queued'}
              <button class="btn btn-ghost btn-icon" aria-label="Cancel {transfer.name}" onclick={() => transfers.cancel(transfer.id)}>
                <Icon name="x" />
              </button>
            {/if}
          </div>
          <div class="progress" class:is-done={transfer.status === 'done'}>
            <span style="width: {transfer.status === 'sending' || transfer.status === 'queued' ? Math.min(100, (transfer.sentBytes / Math.max(1, transfer.sizeBytes)) * 100) : 100}%"></span>
          </div>
          <div class="small muted">
            {STATUS_LABELS[transfer.status]}
            {#if transfer.status === 'sending' && transfer.bytesPerSecond}· {formatSpeed(transfer.bytesPerSecond)}{/if}
            {#if transfer.message}· {transfer.message}{/if}
          </div>
        </article>
      {/each}
    {:else}
      <div class="banner">
        <Icon name="lock" size={20} />
        <p class="muted">This invitation lets you download only. Ask the host for an upload invitation to share files.</p>
      </div>
    {/if}
  </section>

  <section>
    <div class="section-head">
      <h2>Devices in the room</h2>
      <span class="badge">{room.devices.length}</span>
    </div>
    <div class="card devices">
      {#each room.devices as device (device.id)}
        <div class="device">
          <Icon name="laptop" size={20} />
          <div class="meta">
            <div class="truncate">{device.name}</div>
            {#if device.nodeName}<div class="small faint truncate">via {device.nodeName}</div>{/if}
          </div>
        </div>
      {:else}
        <p class="small faint empty-line">No other devices are connected.</p>
      {/each}
    </div>
  </section>

  <p class="small faint privacy"><Icon name="shield" size={14} /> Files travel straight across your local network, never through the internet.</p>
</main>

<style>
  main {
    display: flex;
    flex-direction: column;
    gap: 20px;
    width: min(640px, 100%);
    margin: 0 auto;
    padding: 12px 16px 48px;
  }

  header,
  .brand,
  .host-line,
  .section-head,
  .upload-line,
  .file-head,
  .device {
    display: flex;
    align-items: center;
    gap: 10px;
  }

  header,
  .host-line {
    justify-content: space-between;
  }

  .mark {
    display: grid;
    place-items: center;
    width: 32px;
    height: 32px;
    border-radius: var(--radius-sm);
    background: var(--accent);
    color: var(--on-accent);
  }

  .host {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 16px;
  }

  h1 {
    font-size: 20px;
  }

  section {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }

  .file,
  .upload {
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 16px;
  }

  .icon {
    display: grid;
    place-items: center;
    flex: none;
    width: 44px;
    height: 44px;
    border-radius: var(--radius-sm);
    background: var(--accent-tint);
    color: var(--on-accent-tint);
  }

  .meta {
    min-width: 0;
  }

  .name {
    font-weight: 600;
    overflow-wrap: anywhere;
  }

  .badges {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }

  .upload-line {
    justify-content: space-between;
  }

  .banner {
    display: flex;
    gap: 10px;
    padding: 12px 14px;
    border-radius: var(--radius-md);
    background: var(--surface-low);
  }

  .banner.warning {
    background: var(--warning-tint);
    color: var(--warning);
  }

  .banner.warning p {
    color: var(--text);
  }

  .empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
    padding: 32px 16px;
    color: var(--text-faint);
  }

  .devices {
    padding: 4px 16px;
  }

  .device {
    padding: 10px 0;
  }

  .device + .device {
    border-top: 1px solid var(--border);
  }

  .empty-line {
    padding: 12px 0;
  }

  .privacy {
    display: flex;
    align-items: center;
    gap: 6px;
    justify-content: center;
    text-align: center;
  }
</style>
