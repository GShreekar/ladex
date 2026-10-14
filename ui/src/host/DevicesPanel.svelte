<script lang="ts">
  import { collectDropped } from '../client/dropped-files';
  import { initialsOf } from '../client/format';
  import type { Device } from '../client/types';
  import { deviceIcon } from '../components/file-icon';
  import Icon from '../components/Icon.svelte';
  import { useApp } from '../state/context';

  const { room, transfers } = useApp();

  let isRenaming = $state(false);
  let draftName = $state('');
  let draggedOver = $state<string | null>(null);
  let chooserTarget: Device | null = null;
  let fileChooser: HTMLInputElement;

  function beginRename(): void {
    if (!room.capabilities.canRename) return;
    draftName = '';
    isRenaming = true;
  }

  function commitRename(): void {
    if (!isRenaming) return;
    isRenaming = false;
    if (draftName.trim()) room.client.rename(draftName);
  }

  function chooseFor(device: Device): void {
    chooserTarget = device;
    fileChooser.click();
  }

  function sendChosen(event: Event): void {
    const input = event.target as HTMLInputElement;
    if (chooserTarget && input.files?.length) void transfers.sendTo([...input.files], chooserTarget);
    input.value = '';
  }

  async function dropOn(event: DragEvent, device: Device): Promise<void> {
    event.preventDefault();
    event.stopPropagation();
    draggedOver = null;
    if (!event.dataTransfer) return;
    // A file from this page's catalog is offered; a file from the computer is sent.
    if (!event.dataTransfer.types.includes('Files')) {
      const fileId = event.dataTransfer.getData('text/plain');
      const file = room.files.find((candidate) => candidate.id === fileId);
      if (file) room.client.offerFile(file, device);
      return;
    }
    const { files } = await collectDropped(event.dataTransfer);
    if (files.length > 0) void transfers.sendTo(files, device);
  }
</script>

<section class="card panel">
  <div class="head">
    <h2>Devices nearby</h2>
    <span class="badge badge-success"><span class="dot"></span>{room.devices.length} active</span>
  </div>

  <ul>
    {#each room.devices as device (device.id)}
      <li
        class:over={draggedOver === device.id}
        ondragover={(event) => {
          if (device.isSelf || !room.capabilities.canSendToDevice) return;
          event.preventDefault();
          draggedOver = device.id;
        }}
        ondragleave={() => (draggedOver = null)}
        ondrop={(event) => !device.isSelf && room.capabilities.canSendToDevice && dropOn(event, device)}
      >
        <span class="avatar" class:self={device.isSelf}>{initialsOf(device.name)}</span>
        <div class="who">
          {#if device.isSelf && isRenaming}
            <!-- svelte-ignore a11y_autofocus -->
            <input
              class="input rename"
              maxlength="40"
              placeholder="Name this device"
              bind:value={draftName}
              onblur={commitRename}
              onkeydown={(event) => {
                if (event.key === 'Enter') commitRename();
                if (event.key === 'Escape') isRenaming = false;
              }}
              autofocus
            />
          {:else}
            <div class="name truncate">
              {device.name}{#if device.isSelf}&nbsp;<span class="faint">(you)</span>{/if}
            </div>
            <div class="small faint truncate">
              <Icon name={deviceIcon(device.kind)} size={12} />
              {device.nodeName || 'This node'}{#if device.latencyMs !== null} · {device.latencyMs} ms{/if}
            </div>
          {/if}
        </div>
        {#if device.isSelf}
          {#if room.capabilities.canRename && !isRenaming}
            <button class="action" onclick={beginRename}>Rename</button>
          {/if}
        {:else if room.capabilities.canSendToDevice}
          <button class="action" onclick={() => chooseFor(device)}>Send file</button>
        {/if}
      </li>
    {/each}
  </ul>
  <input bind:this={fileChooser} type="file" multiple hidden onchange={sendChosen} />
</section>

<style>
  .panel {
    padding: 16px;
  }

  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 8px;
  }

  .dot {
    width: 6px;
    height: 6px;
    border-radius: 9999px;
    background: currentColor;
  }

  ul {
    margin: 0;
    padding: 0;
    list-style: none;
  }

  li {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 10px 6px;
    border-radius: var(--radius-sm);
  }

  li + li {
    border-top: 1px solid var(--border);
  }

  li.over {
    outline: 1.5px dashed var(--accent);
    background: color-mix(in srgb, var(--accent) 6%, transparent);
  }

  .avatar {
    display: grid;
    place-items: center;
    flex: none;
    width: 36px;
    height: 36px;
    border-radius: 9999px;
    background: var(--accent-tint);
    color: var(--on-accent-tint);
    font-size: 12px;
    font-weight: 600;
  }

  .avatar.self {
    background: var(--accent);
    color: var(--on-accent);
  }

  .who {
    flex: 1;
    min-width: 0;
  }

  .name {
    font-weight: 500;
  }

  .small {
    display: flex;
    align-items: center;
    gap: 4px;
  }

  .action {
    padding: 4px 6px;
    border: 0;
    background: none;
    color: var(--accent-text);
    font-size: 12px;
    font-weight: 500;
  }

  .action:hover {
    text-decoration: underline;
  }

  .rename {
    height: 30px;
  }
</style>
