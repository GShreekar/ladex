<script lang="ts">
  import CopyButton from '../../components/CopyButton.svelte';
  import { autostartCommand, daemonCommand, dataDirVariable, nodeCommands, settingsCommands } from '../command-line';
  import type { PreferencesView } from '../preferences-api';

  let { view }: { view: PreferencesView } = $props();

  const groups = $derived([
    { title: 'Work with this node', hints: nodeCommands(view) },
    { title: 'Settings', hints: settingsCommands(view) },
    { title: 'Without the app', hints: [daemonCommand(view), autostartCommand(view)] },
  ]);
</script>

<section class="card">
  <h2>Command line</h2>
  <p class="muted">Everything in this app can be done from a terminal with <code>ladex</code>. Copy a command to try it.</p>

  <div class="folder">
    <span class="label">Data folder</span>
    <div class="line">
      <code class="truncate">{view.data_dir}</code>
      <CopyButton text={view.data_dir} label="Copy the data folder" />
    </div>
    <p class="small faint">
      {#if view.is_default_data_dir}
        This is where <code>ladex</code> looks, so its commands find this node without being told.
      {:else}
        It is not the default folder, so the commands below name it. To use plain <code>ladex</code> commands in a terminal, run
        <code>{dataDirVariable(view)}</code> first.
      {/if}
    </p>
  </div>

  {#each groups as group (group.title)}
    <div class="group">
      <h3>{group.title}</h3>
      {#each group.hints as hint (hint.label)}
        <div class="hint">
          <span class="small muted">{hint.label}</span>
          <div class="line">
            <code class="truncate">{hint.command}</code>
            <CopyButton text={hint.command} label={`Copy: ${hint.label}`} />
          </div>
        </div>
      {/each}
    </div>
  {/each}

  <p class="small faint">While the app is running, <code>ladex</code> in a terminal starts nothing new: it reaches this node. To run the node from a terminal instead, quit the app first.</p>
</section>

<style>
  section {
    display: flex;
    flex-direction: column;
    gap: 14px;
    padding: 20px;
  }

  h3 {
    font-size: 13px;
    font-weight: 600;
  }

  .group,
  .folder,
  .hint {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .label {
    font-weight: 500;
  }

  .line {
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 2px 4px 2px 12px;
    border-radius: var(--radius-sm);
    background: var(--surface-low);
  }

  .line code {
    flex: 1;
    padding: 6px 0;
    font-family: ui-monospace, 'SFMono-Regular', Menlo, monospace;
    font-size: 13px;
  }
</style>
