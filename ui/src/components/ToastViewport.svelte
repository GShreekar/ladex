<script lang="ts">
  import type { Toasts } from '../state/toasts.svelte';
  import Icon from './Icon.svelte';
  import type { IconName } from './icons';

  let { toasts }: { toasts: Toasts } = $props();

  const ICON_FOR_LEVEL: Record<string, IconName> = {
    info: 'info',
    success: 'check-circle',
    warning: 'alert',
    error: 'alert',
  };
</script>

<div class="viewport" role="status" aria-live="polite">
  {#each toasts.items as toast (toast.id)}
    <div class="toast {toast.level}">
      <Icon name={ICON_FOR_LEVEL[toast.level] ?? 'info'} size={20} />
      <span class="text">{toast.text}</span>
      {#each toast.actions as action (action.label)}
        <button class="btn btn-small" onclick={() => { action.run(); toasts.dismiss(toast.id); }}>{action.label}</button>
      {/each}
      <button class="btn btn-ghost btn-icon close" aria-label="Dismiss" onclick={() => toasts.dismiss(toast.id)}>
        <Icon name="x" size={16} />
      </button>
    </div>
  {/each}
</div>

<style>
  .viewport {
    position: fixed;
    right: calc(16px + var(--safe-right));
    bottom: calc(16px + var(--bottom-bar-height) + var(--safe-bottom));
    z-index: 100;
    display: flex;
    flex-direction: column;
    gap: 8px;
    width: min(380px, calc(100vw - 32px));
  }

  .toast {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 12px 8px 12px 14px;
    border: 1px solid var(--border);
    border-left-width: 3px;
    border-radius: var(--radius-md);
    background: var(--surface);
    box-shadow: var(--shadow-overlay);
  }

  .toast.info { border-left-color: var(--accent); }
  .toast.success { border-left-color: var(--success); }
  .toast.warning { border-left-color: var(--warning); }
  .toast.error { border-left-color: var(--danger); }

  .toast.success :global(svg:first-child) { color: var(--success); }
  .toast.warning :global(svg:first-child) { color: var(--warning); }
  .toast.error :global(svg:first-child) { color: var(--danger); }
  .toast.info :global(svg:first-child) { color: var(--accent-text); }

  .text {
    flex: 1;
    min-width: 0;
    overflow-wrap: anywhere;
  }

  .close {
    margin-left: auto;
  }
</style>
