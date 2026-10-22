<script lang="ts">
  import type { Snippet } from 'svelte';
  import Icon from './Icon.svelte';

  let {
    title,
    subtitle,
    onclose,
    children,
  }: { title: string; subtitle?: string; onclose: () => void; children: Snippet } = $props();
</script>

<svelte:window onkeydown={(event) => event.key === 'Escape' && onclose()} />

<div class="scrim" role="presentation" onclick={(event) => event.target === event.currentTarget && onclose()}>
  <div class="dialog card" role="dialog" aria-modal="true" aria-label={title}>
    <header>
      <div class="heading">
        <h2>{title}</h2>
        {#if subtitle}<p class="small faint">{subtitle}</p>{/if}
      </div>
      <button class="btn btn-ghost btn-icon" aria-label="Close" onclick={onclose}><Icon name="x" /></button>
    </header>
    {@render children()}
  </div>
</div>

<style>
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 90;
    display: grid;
    place-items: center;
    padding: 16px;
    background: var(--scrim);
  }

  .dialog {
    width: min(480px, 100%);
    max-height: calc(var(--viewport-height) - 32px - var(--safe-bottom));
    overflow: auto;
    padding: 24px;
    box-shadow: var(--shadow-overlay);
  }

  header {
    display: flex;
    justify-content: space-between;
    gap: 12px;
    margin-bottom: 16px;
  }
</style>
