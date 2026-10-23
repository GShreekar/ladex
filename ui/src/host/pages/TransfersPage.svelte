<script lang="ts">
  import { formatBytes, formatSpeed, formatTimeLeft } from '../../client/format';
  import Icon from '../../components/Icon.svelte';
  import { useApp } from '../../state/context';
  import type { Transfer, TransferStatus } from '../../state/transfers.svelte';

  const { room, transfers } = useApp();

  const STATUS_LABELS: Record<TransferStatus, string> = {
    queued: 'Waiting',
    sending: 'Uploading',
    done: 'Shared',
    'awaiting-approval': 'Waiting for the host',
    offered: 'Waiting to be accepted',
    failed: 'Failed',
    cancelled: 'Cancelled',
  };

  const STATUS_BADGES: Partial<Record<TransferStatus, string>> = {
    done: 'badge-success',
    failed: 'badge-danger',
    offered: 'badge-warning',
    'awaiting-approval': 'badge-warning',
    sending: 'badge-accent',
  };

  const aggregateSpeed = $derived(
    transfers.active.reduce((sum, transfer) => sum + (transfer.bytesPerSecond ?? 0), 0),
  );

  function percent(transfer: Transfer): number {
    return Math.min(100, Math.round((transfer.sentBytes / Math.max(1, transfer.sizeBytes)) * 100));
  }

  function timeLeft(transfer: Transfer): string | null {
    if (!transfer.bytesPerSecond) return null;
    return formatTimeLeft((transfer.sizeBytes - transfer.sentBytes) / transfer.bytesPerSecond);
  }
</script>

<div class="title">
  <div>
    <h1>Transfers</h1>
    <p class="muted">Files this device is sending. Downloads are saved by your browser.</p>
  </div>
  {#if transfers.finished.length > 0}
    <button class="btn" onclick={() => transfers.clearFinished()}>Clear finished</button>
  {/if}
</div>

{#if transfers.items.length === 0 && room.saved.length === 0}
  <div class="card empty">
    <Icon name="swap" size={28} />
    <p>Nothing is being sent right now.</p>
  </div>
{/if}

{#if transfers.active.length > 0}
  <section>
    <h2>In progress <span class="badge badge-accent">{transfers.active.length}</span>
      {#if aggregateSpeed > 0}<span class="small faint">{formatSpeed(aggregateSpeed)}</span>{/if}
    </h2>
    {#each transfers.active as transfer (transfer.id)}
      <div class="card row">
        <div class="line">
          <Icon name={transfer.isFolder ? 'folder' : 'file'} size={22} />
          <div class="info">
            <div class="truncate"><strong>{transfer.name}</strong></div>
            <div class="small faint">
              {transfer.targetName ? `To ${transfer.targetName}` : 'To the room'} · {formatBytes(transfer.sizeBytes)}
            </div>
          </div>
          <span class="badge {STATUS_BADGES[transfer.status] ?? ''}">{STATUS_LABELS[transfer.status]}</span>
          <button class="btn btn-ghost btn-icon" aria-label="Cancel {transfer.name}" onclick={() => transfers.cancel(transfer.id)}>
            <Icon name="x" />
          </button>
        </div>
        <div class="progress"><span style="width: {percent(transfer)}%"></span></div>
        <div class="small muted stats">
          <span>{formatBytes(transfer.sentBytes)} of {formatBytes(transfer.sizeBytes)} ({percent(transfer)}%)</span>
          {#if transfer.bytesPerSecond}<span>{formatSpeed(transfer.bytesPerSecond)} · {timeLeft(transfer) ?? ''}</span>{/if}
        </div>
      </div>
    {/each}
  </section>
{/if}

{#if transfers.finished.length > 0}
  <section>
    <h2>Finished</h2>
    {#each transfers.finished as transfer (transfer.id)}
      <div class="card row">
        <div class="line">
          <Icon name={transfer.isFolder ? 'folder' : 'file'} size={22} />
          <div class="info">
            <div class="truncate"><strong>{transfer.name}</strong></div>
            <div class="small faint">
              {transfer.targetName ? `To ${transfer.targetName}` : 'To the room'} · {formatBytes(transfer.sizeBytes)}
              {#if transfer.message}· {transfer.message}{/if}
            </div>
          </div>
          <span class="badge {STATUS_BADGES[transfer.status] ?? ''}">{STATUS_LABELS[transfer.status]}</span>
        </div>
      </div>
    {/each}
  </section>
{/if}

{#if room.saved.length > 0}
  <section>
    <h2>Saved on this phone</h2>
    {#each room.saved as file (file.id)}
      <div class="card row">
        <div class="line">
          <Icon name="file" size={22} />
          <div class="info">
            <div class="truncate"><strong>{file.name}</strong></div>
            <div class="small faint truncate">{file.shownAs}</div>
          </div>
          <button class="btn btn-small" onclick={() => room.openSaved(file)}>Open</button>
          <button class="btn btn-small" onclick={() => room.shareSaved(file)}>Share</button>
        </div>
      </div>
    {/each}
  </section>
{/if}

<style>
  .title {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 16px;
    margin-bottom: 20px;
  }

  section {
    margin-bottom: 24px;
  }

  h2 {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 12px;
  }

  .row {
    margin-bottom: 8px;
    padding: 14px 16px;
  }

  .line {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .info {
    flex: 1;
    min-width: 0;
  }

  .progress {
    margin: 12px 0 6px;
  }

  .stats {
    display: flex;
    justify-content: space-between;
    gap: 12px;
  }

  .empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
    padding: 48px 16px;
    color: var(--text-faint);
  }
</style>
