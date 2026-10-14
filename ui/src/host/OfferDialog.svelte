<script lang="ts">
  import { formatBytes } from '../client/format';
  import type { IncomingOffer } from '../client/types';
  import Icon from '../components/Icon.svelte';
  import Modal from '../components/Modal.svelte';
  import { useApp } from '../state/context';

  let { offer }: { offer: IncomingOffer } = $props();

  const { room } = useApp();
</script>

<Modal title="Incoming file" subtitle="From {offer.senderName}" onclose={() => room.answerOffer(offer, false)}>
  <div class="file">
    <span class="icon"><Icon name="file" size={22} /></span>
    <div class="details">
      <div class="name truncate">{offer.fileName}</div>
      {#if offer.sizeBytes !== null}<div class="small faint">{formatBytes(offer.sizeBytes)}</div>{/if}
    </div>
  </div>
  <p class="muted note">Nothing is fetched until you accept.</p>
  <div class="actions">
    <button class="btn" onclick={() => room.answerOffer(offer, false)}>Decline</button>
    <button class="btn btn-primary" onclick={() => room.answerOffer(offer, true)}><Icon name="download" /> Accept</button>
  </div>
</Modal>

<style>
  .file {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 12px;
    border-radius: var(--radius-md);
    background: var(--surface-low);
  }

  .icon {
    display: grid;
    place-items: center;
    width: 40px;
    height: 40px;
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

  .note {
    margin: 12px 0 20px;
  }

  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }
</style>
