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
  let hasDismissedStart = $state(readDismissedStart());

  const START_KEY = 'ladex_start_dismissed';

  function readDismissedStart(): boolean {
    try {
      return localStorage.getItem(START_KEY) === 'yes';
    } catch {
      return false;
    }
  }

  function dismissStart(): void {
    hasDismissedStart = true;
    try {
      localStorage.setItem(START_KEY, 'yes');
    } catch {
      // It comes back next visit, which does no harm.
    }
  }

  const filters: Array<{ id: FileFilter; label: string }> = [
    { id: 'all', label: 'All' },
    { id: 'documents', label: 'Documents' },
    { id: 'media', label: 'Media' },
  ];

  const showsGettingStarted = $derived(room.capabilities.canManageGuests && !hasDismissedStart && room.files.length === 0);

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
  <div class="banner" role="status">
    <Icon name="info" size={22} />
    <div>
      <strong>This is the only computer running LADEX on this network so far.</strong>
      <p class="small">
        Phones and other browsers can still join as guests with a QR code. If LADEX is running on another computer too,
        check that both are on the same Wi-Fi, that client isolation is off on the router (common on guest networks and
        phone hotspots), and that a firewall isn't blocking LADEX.
      </p>
      {#if room.capabilities.canManageGuests}
        <div class="banner-actions">
          <button class="btn btn-small" onclick={() => shell.go('guests')}><Icon name="qr" size={15} /> Invite a phone</button>
          <button class="btn btn-small" onclick={() => shell.go('network')}><Icon name="wifi" size={15} /> Check the network</button>
        </div>
      {/if}
    </div>
    <button class="btn btn-ghost btn-icon" aria-label="Dismiss" onclick={() => room.dismissNoPeersWarning()}><Icon name="x" /></button>
  </div>
{/if}

<div class="layout">
  <div class="main">
    {#if showsGettingStarted}
      <section class="card start" aria-label="Getting started">
        <div class="start-head">
          <h2>Get started</h2>
          <button class="btn btn-ghost btn-icon" aria-label="Hide this" onclick={dismissStart}><Icon name="x" /></button>
        </div>
        <ol>
          <li>
            <span class="number">1</span>
            <div>
              <strong>Invite a phone or another computer</strong>
              <p class="small muted">Scan a QR code with the phone’s camera. Nothing to install.</p>
            </div>
            <button class="btn btn-primary" onclick={() => shell.go('guests')}><Icon name="qr" size={16} /> Show the QR code</button>
          </li>
          <li>
            <span class="number">2</span>
            <div>
              <strong>Share a folder</strong>
              <p class="small muted">Everyone in the room can download it, and devices that have it help send it on.</p>
            </div>
            <button class="btn" onclick={() => folderChooser.click()}><Icon name="folder" size={16} /> Choose a folder</button>
          </li>
        </ol>
      </section>
    {/if}

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
    background: var(--accent-tint);
    color: var(--on-accent-tint);
  }

  .banner > div {
    flex: 1;
    color: var(--text);
  }

  .banner-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }

  .start {
    padding: 20px;
  }

  .start-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 8px;
  }

  .start ol {
    display: flex;
    flex-direction: column;
    gap: 12px;
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .start li {
    display: flex;
    align-items: center;
    gap: 12px;
  }

  .start li > div {
    flex: 1;
  }

  .number {
    display: grid;
    place-items: center;
    width: 28px;
    height: 28px;
    border-radius: 9999px;
    background: var(--accent-tint);
    color: var(--on-accent-tint);
    font-weight: 600;
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
