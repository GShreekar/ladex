<script lang="ts">
  import { formatBytes } from '../../client/format';
  import { TransferError } from '../../client/types';
  import { useHost } from '../host-context';
  import type { PreferencesView } from '../preferences-api';

  let { view, onchange }: { view: PreferencesView; onchange: (view: PreferencesView) => void } = $props();

  const { preferencesApi, toasts } = useHost();

  const BYTES_PER_GIB = 1024 ** 3;

  let limitText = $state('');

  $effect(() => {
    limitText = String(view.preferences.storage_limit_gib);
  });

  const limitBytes = $derived(view.preferences.storage_limit_gib * BYTES_PER_GIB);
  const usedPercent = $derived(limitBytes > 0 ? Math.min(100, Math.round((view.store_used_bytes / limitBytes) * 100)) : 0);
  const isChanged = $derived(Number(limitText) !== view.preferences.storage_limit_gib);

  async function save(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    try {
      onchange(await preferencesApi.update({ storage_limit_gib: Math.floor(Number(limitText)) }));
      toasts.show('success', 'Storage limit saved');
    } catch (error) {
      toasts.show('error', error instanceof TransferError ? error.message : 'Could not save the storage limit.');
    }
  }
</script>

<section class="card">
  <h2>Storage</h2>
  <p class="muted">Files shared with the room, and copies kept to help send them on, are stored on this computer up to this limit.</p>

  <div class="usage">
    <div class="progress" class:is-done={usedPercent >= 90}><span style:width="{usedPercent}%"></span></div>
    <span class="small faint">
      {formatBytes(view.store_used_bytes)} of {formatBytes(limitBytes)} used{view.disk_free_bytes === null
        ? ''
        : ` · ${formatBytes(view.disk_free_bytes)} free on this disk`}
    </span>
  </div>

  <form onsubmit={save}>
    <label for="storage-limit">Limit</label>
    <input id="storage-limit" class="input number" type="number" min="1" bind:value={limitText} />
    <span class="muted">GB</span>
    <button class="btn btn-primary" disabled={!isChanged}>Save</button>
  </form>
</section>

<style>
  section {
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 20px;
  }

  .usage {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  form {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .number {
    width: 110px;
  }
</style>
