<script lang="ts">
  import { useApp } from '../state/context';
  import Icon from './Icon.svelte';

  let { text, label }: { text: string; label: string } = $props();

  const { toasts } = useApp();

  async function copy(): Promise<void> {
    try {
      await navigator.clipboard.writeText(text);
      toasts.show('success', 'Copied');
    } catch {
      toasts.show('error', 'Could not copy. Select the text and copy it by hand.');
    }
  }
</script>

<button class="btn btn-ghost btn-icon" aria-label={label} title={label} onclick={copy}><Icon name="copy" size={16} /></button>
