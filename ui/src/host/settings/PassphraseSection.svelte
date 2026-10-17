<script lang="ts">
  import { TransferError } from '../../client/types';
  import CopyButton from '../../components/CopyButton.svelte';
  import Icon from '../../components/Icon.svelte';
  import { useHost } from '../host-context';
  import type { PreferencesView } from '../preferences-api';

  let { view, onchange }: { view: PreferencesView; onchange: (view: PreferencesView) => void } = $props();

  const { preferencesApi, desktop, toasts } = useHost();

  let isChanging = $state(false);
  let draft = $state('');
  let isRestarting = $state(false);
  let shown = $state<string | null>(null);

  async function toggleShown(): Promise<void> {
    if (shown !== null) {
      shown = null;
      return;
    }
    try {
      shown = (await preferencesApi.currentPassphrase()) ?? '';
    } catch (error) {
      toasts.show('error', error instanceof TransferError ? error.message : 'Could not read the passphrase.');
    }
  }

  async function suggest(): Promise<void> {
    try {
      draft = await preferencesApi.suggestPassphrase();
    } catch (error) {
      toasts.show('error', error instanceof TransferError ? error.message : 'Could not make a passphrase.');
    }
  }

  function startChanging(): void {
    isChanging = true;
    draft = '';
  }

  async function save(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    try {
      onchange(await preferencesApi.changePassphrase(draft.trim()));
      isChanging = false;
      draft = '';
    } catch (error) {
      toasts.show('error', error instanceof TransferError ? error.message : 'Could not save the passphrase.');
    }
  }

  async function restart(): Promise<void> {
    isRestarting = true;
    try {
      await desktop?.restartNode();
    } catch {
      isRestarting = false;
      toasts.show('error', 'LADEX could not restart. Quit it from the tray and open it again.');
    }
  }
</script>

<section class="card">
  <h2>Mesh passphrase</h2>
  <p class="muted">
    Every device that joins this mesh needs the same passphrase. {view.has_passphrase
      ? 'This node has one.'
      : 'This node runs without one, so anyone on the network can join.'}
  </p>

  {#if view.needs_restart}
    <div class="restart" role="status">
      <Icon name="info" size={20} />
      <div>
        <strong>The new passphrase starts working after a restart.</strong>
        <p class="small">
          {#if desktop}
            Devices already connected stay connected until then. Restarting takes a few seconds.
          {:else}
            Stop <code>ladex</code> and start it again.
          {/if}
        </p>
      </div>
      {#if desktop}
        <button class="btn btn-primary" disabled={isRestarting} onclick={restart}>{isRestarting ? 'Restarting…' : 'Restart now'}</button>
      {/if}
    </div>
  {:else if isChanging}
    <form onsubmit={save}>
      <label for="new-passphrase" class="sr-only">New passphrase</label>
      <input id="new-passphrase" class="input" bind:value={draft} placeholder="New passphrase" autocomplete="off" spellcheck="false" />
      <button type="button" class="btn" onclick={suggest}><Icon name="refresh" size={16} /> Make one</button>
      <button class="btn btn-primary" disabled={!draft.trim()}>Save</button>
      <button type="button" class="btn btn-ghost" onclick={() => (isChanging = false)}>Cancel</button>
    </form>
    <p class="small faint">Other devices will need the new passphrase to join. Changing it does not disconnect them until they restart.</p>
  {:else}
    {#if shown}
      <div class="current">
        <code>{shown}</code>
        <CopyButton text={shown} label="Copy the passphrase" />
      </div>
      <p class="small faint">Type it on the computer you are adding. Phones join by QR code instead.</p>
    {/if}
    <div class="buttons">
      {#if view.has_passphrase}
        <button class="btn" onclick={toggleShown}><Icon name="lock" size={16} /> {shown ? 'Hide passphrase' : 'Show passphrase'}</button>
      {/if}
      <button class="btn" onclick={startChanging}>Change passphrase…</button>
    </div>
  {/if}
</section>

<style>
  .current {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px 12px;
    border-radius: var(--radius-sm);
    background: var(--surface-low);
  }

  .current code {
    flex: 1;
    font-size: 16px;
    letter-spacing: 0.05em;
    word-break: break-all;
  }

  .buttons {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }

  section {
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 20px;
  }

  form {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }

  form .input {
    flex: 1;
    min-width: 200px;
    font-family: ui-monospace, 'SFMono-Regular', Menlo, monospace;
  }

  .restart {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 12px 14px;
    border-radius: var(--radius-md);
    background: var(--warning-tint);
  }

  .restart > div {
    flex: 1;
    color: var(--text);
  }
</style>
