<script lang="ts">
  import { collectDropped } from '../../client/dropped-files';
  import { categoryOf } from '../../client/format';
  import Icon from '../../components/Icon.svelte';
  import { useApp } from '../../state/context';
  import ChatPanel from '../ChatPanel.svelte';
  import DevicesPanel from '../DevicesPanel.svelte';
  import FileTable from '../FileTable.svelte';
  import { shell, type FileFilter } from '../shell-state.svelte';

  const { room, transfers } = useApp();

  let fileChooser: HTMLInputElement;
  let folderChooser: HTMLInputElement;
  let isDraggingOver = $state(false);

  const filters: Array<{ id: FileFilter; label: string }> = [
    { id: 'all', label: 'All' },
    { id: 'documents', label: 'Documents' },
    { id: 'media', label: 'Media' },
  ];

  const visibleFiles = $derived.by(() => {
    const query = shell.query.trim().toLowerCase();
    return room.files
      .filter((file) => !query || file.name.toLowerCase().includes(query))
      .filter((file) => {
        const category = categoryOf(file.mimeType, file.isFolder);
        return shell.filter === 'all' || (shell.filter === 'documents' ? category === 'document' : category === 'media');
      })
      .toSorted((a, b) => b.sharedAt - a.sharedAt);
  });

  function chooseFiles(event: Event): void {
    const input = event.target as HTMLInputElement;
    void transfers.share([...(input.files ?? [])]);
    input.value = '';
  }

  function chooseFolder(event: Event): void {
    const input = event.target as HTMLInputElement;
    void transfers.shareFolder([...(input.files ?? [])]);
    input.value = '';
  }

  async function dropOnZone(event: DragEvent): Promise<void> {
    if (!event.dataTransfer?.types.includes('Files')) return;
    event.preventDefault();
    event.stopPropagation();
    isDraggingOver = false;
    const { files, folders } = await collectDropped(event.dataTransfer);
    if (files.length > 0) void transfers.share(files);
    for (const folder of folders) void transfers.shareFolder(folder);
  }
</script>

{#if room.hasNoPeersWarning}
  <div class="banner" role="alert">
    <Icon name="wifi-off" size={22} />
    <div>
      <strong>No other LADEX devices found on this network.</strong>
      <p class="small">
        Check that every device is on the same Wi-Fi, that AP/client isolation is off on the router (common on guest
        networks and phone hotspots), and that a firewall isn't blocking LADEX's ports.
      </p>
    </div>
    <button class="btn btn-ghost btn-icon" aria-label="Dismiss" onclick={() => room.dismissNoPeersWarning()}><Icon name="x" /></button>
  </div>
{/if}

<div class="layout">
  <div class="main">
    <section
      class="dropzone"
      class:over={isDraggingOver}
      ondragenter={() => (isDraggingOver = true)}
      ondragleave={() => (isDraggingOver = false)}
      ondragover={(event) => event.preventDefault()}
      ondrop={dropOnZone}
      aria-label="Share files"
    >
      <span class="cloud"><Icon name="upload-cloud" size={28} /></span>
      <h2>Drop files or folders here to share with everyone</h2>
      <p class="muted">
        or <button class="link" onclick={() => fileChooser.click()}>choose files</button> ·
        <button class="link" onclick={() => folderChooser.click()}>choose a folder</button>
      </p>
      <p class="small faint"><Icon name="shield" size={14} /> Devices that have a file help send it to the others</p>
      <input bind:this={fileChooser} type="file" multiple hidden onchange={chooseFiles} />
      <input bind:this={folderChooser} type="file" webkitdirectory hidden onchange={chooseFolder} />
    </section>

    <section class="card shared">
      <div class="shared-head">
        <h2>Shared with the room</h2>
        <span class="badge badge-accent">{room.files.length} {room.files.length === 1 ? 'file' : 'files'}</span>
        <div class="filters" role="group" aria-label="Filter files">
          {#each filters as filter (filter.id)}
            <button class:active={shell.filter === filter.id} onclick={() => (shell.filter = filter.id)}>{filter.label}</button>
          {/each}
        </div>
      </div>
      <FileTable files={visibleFiles} hasFiles={room.files.length > 0} />
    </section>
  </div>

  <div class="side">
    <DevicesPanel />
    <ChatPanel />
  </div>
</div>

<style>
  .banner {
    display: flex;
    align-items: flex-start;
    gap: 12px;
    margin-bottom: 16px;
    padding: 14px 16px;
    border-radius: var(--radius-md);
    background: var(--warning-tint);
    color: var(--warning);
  }

  .banner > div {
    flex: 1;
    color: var(--text);
  }

  .layout {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 320px;
    gap: 24px;
    align-items: start;
  }

  .main,
  .side {
    display: flex;
    flex-direction: column;
    gap: 24px;
    min-width: 0;
  }

  .dropzone {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
    padding: 32px 16px;
    border: 1.5px dashed var(--border-strong);
    border-radius: var(--radius-md);
    background: var(--surface);
    text-align: center;
    transition: border-color 0.12s, background 0.12s;
  }

  .dropzone.over {
    border-color: var(--accent);
    background: color-mix(in srgb, var(--accent) 5%, var(--surface));
  }

  .cloud {
    display: grid;
    place-items: center;
    width: 56px;
    height: 56px;
    border-radius: 9999px;
    background: var(--surface-high);
    color: var(--accent-text);
  }

  .link {
    padding: 0;
    border: 0;
    background: none;
    color: var(--accent-text);
    font-weight: 500;
  }

  .link:hover {
    text-decoration: underline;
  }

  .dropzone .small {
    display: flex;
    align-items: center;
    gap: 6px;
  }

  .shared-head {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px 12px;
    padding: 16px;
  }

  .filters {
    display: flex;
    gap: 2px;
    margin-left: auto;
    padding: 3px;
    border-radius: var(--radius-sm);
    background: var(--surface-low);
  }

  .filters button {
    padding: 3px 10px;
    border: 0;
    border-radius: 6px;
    background: transparent;
    color: var(--text-muted);
    font-size: 12px;
  }

  .filters button.active {
    background: var(--surface);
    color: var(--accent-text);
    font-weight: 500;
  }

  @media (max-width: 1100px) {
    .layout {
      grid-template-columns: minmax(0, 1fr);
    }
  }
</style>
