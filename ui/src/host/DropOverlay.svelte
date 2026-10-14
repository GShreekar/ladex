<script lang="ts">
  import { collectDropped } from '../client/dropped-files';
  import Icon from '../components/Icon.svelte';
  import { useApp } from '../state/context';

  const { transfers } = useApp();

  let depth = $state(0);

  function carriesFiles(event: DragEvent): boolean {
    return event.dataTransfer?.types.includes('Files') ?? false;
  }

  function enter(event: DragEvent): void {
    if (!carriesFiles(event)) return;
    event.preventDefault();
    depth++;
  }

  function leave(event: DragEvent): void {
    if (!carriesFiles(event)) return;
    depth = Math.max(0, depth - 1);
  }

  function over(event: DragEvent): void {
    if (!carriesFiles(event)) return;
    event.preventDefault();
  }

  async function drop(event: DragEvent): Promise<void> {
    if (!carriesFiles(event) || !event.dataTransfer) return;
    event.preventDefault();
    depth = 0;
    const { files, folders } = await collectDropped(event.dataTransfer);
    if (files.length > 0) void transfers.share(files);
    for (const folder of folders) void transfers.shareFolder(folder);
  }
</script>

<svelte:window ondragenter={enter} ondragleave={leave} ondragover={over} ondrop={drop} />

{#if depth > 0}
  <div class="overlay" aria-hidden="true">
    <div class="target">
      <Icon name="upload-cloud" size={40} />
      <h2>Drop to share with everyone</h2>
    </div>
  </div>
{/if}

<style>
  .overlay {
    position: fixed;
    inset: 0;
    z-index: 80;
    display: grid;
    place-items: center;
    padding: 24px;
    background: color-mix(in srgb, var(--accent) 8%, var(--scrim));
    pointer-events: none;
  }

  .target {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 12px;
    width: min(560px, 100%);
    padding: 48px 24px;
    border: 1.5px dashed var(--accent);
    border-radius: var(--radius-md);
    background: var(--surface);
    color: var(--accent-text);
  }
</style>
