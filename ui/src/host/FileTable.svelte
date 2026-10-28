<script lang="ts">
  import { describeHolders, formatAgo, formatBytes } from '../client/format';
  import type { SharedFile } from '../client/types';
  import { iconFor } from '../components/file-icon';
  import Icon from '../components/Icon.svelte';
  import { clock } from '../state/clock.svelte';
  import { useApp } from '../state/context';

  let { files, hasFiles }: { files: SharedFile[]; hasFiles: boolean } = $props();

  const { room } = useApp();

  function describe(file: SharedFile): string {
    return file.isFolder ? `Folder · ${file.folderFileCount} file${file.folderFileCount === 1 ? '' : 's'}` : file.mimeType;
  }

  function holdersTitle(file: SharedFile): string {
    if (file.holderNames.length === 0) return 'No single node has all of it; it is fetched from several';
    return `Whole copies on ${file.holderNames.join(', ')}`;
  }

  function startDrag(event: DragEvent, file: SharedFile): void {
    event.dataTransfer?.setData('text/plain', file.id);
    if (event.dataTransfer) event.dataTransfer.effectAllowed = 'copy';
  }
</script>

{#if files.length === 0}
  <div class="empty">
    <Icon name="files" size={28} />
    <p>{hasFiles ? 'No files match your search.' : 'Nothing is shared yet. Drop a file above to get started.'}</p>
  </div>
{:else}
  <div class="scroll">
    <table>
      <thead>
        <tr>
          <th>Name</th>
          <th class="hide-narrow">Size</th>
          <th class="hide-narrow">Shared by</th>
          <th class="hide-phone">Available on</th>
          <th class="hide-narrow">When</th>
          <th class="actions-column"><span class="sr-only">Actions</span></th>
        </tr>
      </thead>
      <tbody>
        {#each files as file (file.id)}
          <tr draggable="true" ondragstart={(event) => startDrag(event, file)}>
            <td>
              <div class="name-cell">
                <span class="icon"><Icon name={iconFor(file)} size={20} /></span>
                <div class="names">
                  <span class="name truncate" title={file.name}>{file.name}</span>
                  <span class="type truncate">{describe(file)}<span class="narrow-only"> · {formatBytes(file.sizeBytes)}</span></span>
                  {#if file.needsThisDeviceOnline}
                    <span class="only-here" role="note">Only your phone has this; keep the app open until someone downloads it.</span>
                  {/if}
                </div>
              </div>
            </td>
            <td class="hide-narrow nowrap muted">{formatBytes(file.sizeBytes)}</td>
            <td class="hide-narrow nowrap">{file.sharedBy}</td>
            <td class="hide-phone">
              {#if file.isAvailable}
                <span class="badge badge-success" title={holdersTitle(file)}>
                  <Icon name="hub" size={13} />
                  {describeHolders(file.holderNames.length, true)}
                </span>
              {:else}
                <span class="badge" title="Every device that has this file is offline"><Icon name="wifi-off" size={13} /> Host offline</span>
              {/if}
            </td>
            <td class="hide-narrow nowrap small muted">{formatAgo(file.sharedAt, clock.now)}</td>
            <td class="actions-column">
              <div class="actions">
                {#if file.isOwn}
                  <span class="badge" title="Shared from this device, so it already has it">Yours</span>
                {:else if file.isAvailable}
                  <button class="btn btn-primary btn-small" onclick={() => room.download(file)}>
                    <Icon name="download" size={15} /> {file.isFolder ? 'Zip' : 'Download'}
                  </button>
                {:else}
                  <button class="btn btn-small" disabled>Unavailable</button>
                {/if}
                {#if file.canUnshare}
                  <button class="btn btn-ghost btn-icon" aria-label="Unshare {file.name}" title="Unshare" onclick={() => room.client.unshare(file)}>
                    <Icon name="trash" size={18} />
                  </button>
                {/if}
              </div>
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  </div>
{/if}

<style>
  .scroll {
    overflow-x: auto;
  }

  table {
    width: 100%;
    border-collapse: collapse;
  }

  th {
    padding: 10px 16px;
    background: var(--surface-low);
    color: var(--text-muted);
    font-size: 12px;
    font-weight: 500;
    text-align: left;
  }

  td {
    padding: 12px 16px;
    border-top: 1px solid var(--border);
    vertical-align: middle;
  }

  tbody tr:hover {
    background: color-mix(in srgb, var(--surface-low) 70%, transparent);
  }

  .nowrap {
    white-space: nowrap;
  }

  .name-cell {
    display: flex;
    align-items: center;
    gap: 12px;
    min-width: 0;
  }

  .icon {
    display: grid;
    place-items: center;
    flex: none;
    width: 34px;
    height: 34px;
    border-radius: var(--radius-sm);
    background: var(--accent-tint);
    color: var(--on-accent-tint);
  }

  .names {
    display: flex;
    flex-direction: column;
    min-width: 0;
    max-width: 280px;
  }

  .name {
    font-weight: 500;
  }

  .type {
    color: var(--text-faint);
    font-size: 11px;
    letter-spacing: 0.02em;
  }

  .only-here {
    margin-top: 2px;
    color: var(--warning);
    font-size: 12px;
  }

  .actions-column {
    text-align: right;
  }

  .actions {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 4px;
  }

  .empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
    padding: 40px 16px;
    border-top: 1px solid var(--border);
    color: var(--text-faint);
    text-align: center;
  }

  .narrow-only {
    display: none;
  }

  @media (max-width: 720px) {
    .hide-narrow {
      display: none;
    }

    .narrow-only {
      display: inline;
    }
  }

  /* A phone has room for the name and the download button; the button already says when a file is unavailable. */
  @media (max-width: 600px) {
    .hide-phone {
      display: none;
    }

    th,
    td {
      padding-left: 12px;
      padding-right: 12px;
    }
  }
</style>
