<script lang="ts">
  import { onMount, untrack } from 'svelte';
  import { formatBytes } from '../client/format';
  import Icon from '../components/Icon.svelte';
  import Modal from '../components/Modal.svelte';
  import type { PhoneShell } from '../desktop/phone';
  import { useApp } from '../state/context';
  import { IncomingShare } from './incoming-share.svelte';

  let { phone }: { phone: PhoneShell } = $props();

  const { toasts } = useApp();
  // The app gives the page one phone for its whole life.
  const incoming = new IncomingShare(untrack(() => phone), toasts);

  const title = $derived(incoming.files.length === 1 ? 'Share this file?' : `Share ${incoming.files.length} files?`);
  const percent = $derived(
    incoming.progress ? Math.min(100, Math.round((incoming.progress.done / Math.max(1, incoming.progress.total)) * 100)) : 0,
  );

  onMount(() => {
    void incoming.start();
    return () => incoming.stop();
  });
</script>

{#if incoming.files.length > 0}
  <Modal {title} subtitle="From another app on this phone" onclose={() => void incoming.setAside()}>
    <ul class="files">
      {#each incoming.files as file (file.id)}
        <li>
          <span class="icon"><Icon name="file" size={20} /></span>
          <div class="details">
            <div class="name truncate">{file.name}</div>
            {#if file.size !== null}<div class="small faint">{formatBytes(file.size)}</div>{/if}
          </div>
        </li>
      {/each}
    </ul>

    {#if incoming.progress}
      <div class="progress" role="progressbar" aria-valuenow={percent} aria-valuemin={0} aria-valuemax={100}>
        <span style="width: {percent}%"></span>
      </div>
      <p class="small muted note">Sharing… {formatBytes(incoming.progress.done)} of {formatBytes(incoming.progress.total)}</p>
    {:else}
      <p class="muted note">
        Everyone in your mesh will see {incoming.files.length === 1 ? 'it' : 'them'} and can download {incoming.files.length === 1 ? 'it' : 'them'}.
        {#if incoming.knownBytes > 0}{formatBytes(incoming.knownBytes)}{incoming.hasUnknownSizes ? ' and more' : ''} in all.{/if}
      </p>
    {/if}

    <div class="actions">
      <button class="btn" disabled={incoming.isSharing} onclick={() => void incoming.setAside()}>Not now</button>
      <button class="btn btn-primary" disabled={incoming.isSharing} onclick={() => void incoming.share()}>
        <Icon name="plus" /> {incoming.isSharing ? 'Sharing…' : 'Share with everyone'}
      </button>
    </div>
  </Modal>
{/if}

<style>
  .files {
    display: flex;
    flex-direction: column;
    gap: 8px;
    max-height: 40vh;
    margin: 0;
    padding: 0;
    overflow: auto;
    list-style: none;
  }

  li {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 10px 12px;
    border-radius: var(--radius-md);
    background: var(--surface-low);
  }

  .icon {
    display: grid;
    flex: none;
    place-items: center;
    width: 36px;
    height: 36px;
    border-radius: var(--radius-sm);
    background: var(--accent-tint);
    color: var(--on-accent-tint);
  }

  .details {
    min-width: 0;
  }

  .name {
    font-weight: 500;
  }

  .progress {
    margin-top: 16px;
  }

  .note {
    margin: 12px 0 20px;
  }

  .actions {
    display: flex;
    flex-wrap: wrap;
    justify-content: flex-end;
    gap: 8px;
  }
</style>
