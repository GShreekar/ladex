<script lang="ts">
  import { onMount } from 'svelte';
  import { TransferError } from '../../client/types';
  import CopyButton from '../../components/CopyButton.svelte';
  import Icon from '../../components/Icon.svelte';
  import { useHost } from '../host-context';
  import type { PreferencesView } from '../preferences-api';

  const { preferencesApi, toasts } = useHost();

  let view = $state<PreferencesView | null>(null);
  let draft = $state('');
  let isSaving = $state(false);

  const added = $derived(view?.preferences.peer_addresses ?? []);

  async function save(addresses: string[]): Promise<boolean> {
    isSaving = true;
    try {
      view = await preferencesApi.update({ peer_addresses: addresses });
      return true;
    } catch (error) {
      toasts.show('error', error instanceof TransferError ? error.message : 'Could not save that address.');
      return false;
    } finally {
      isSaving = false;
    }
  }

  async function add(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    const address = draft.trim();
    if (!address) return;
    if (await save([...added, address])) {
      draft = '';
      toasts.show('success', `Connecting to ${address}…`);
    }
  }

  function remove(address: string): void {
    void save(added.filter((other) => other !== address));
  }

  onMount(async () => {
    try {
      view = await preferencesApi.get();
    } catch (error) {
      toasts.show('error', error instanceof TransferError ? error.message : 'Could not read the added devices.');
    }
  });
</script>

<section class="card addresses">
  <h2><Icon name="wifi" size={18} /> Add a device by address</h2>
  <p class="small muted">
    For when devices in the same mesh can't find each other on their own, as on networks that block discovery. Type the other device's
    address here; it stays connected from then on.
  </p>

  {#if view?.this_address}
    <div class="own">
      <span class="small faint">This device's address</span>
      <code>{view.this_address}</code>
      <CopyButton text={view.this_address} label="Copy this device's address" />
    </div>
  {/if}

  <form onsubmit={add}>
    <label for="peer-address" class="sr-only">The other device's address</label>
    <input
      id="peer-address"
      class="input"
      bind:value={draft}
      placeholder="192.168.1.20:8080"
      inputmode="url"
      autocomplete="off"
      autocapitalize="off"
      spellcheck="false"
    />
    <button class="btn btn-primary" disabled={!draft.trim() || isSaving}>Add</button>
  </form>

  {#if added.length}
    <ul>
      {#each added as address (address)}
        <li>
          <code>{address}</code>
          <button class="btn btn-ghost" disabled={isSaving} onclick={() => remove(address)}>Remove</button>
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .addresses {
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 20px;
  }

  h2 {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .own {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    align-self: flex-start;
    gap: 8px;
    padding: 4px 4px 4px 12px;
    border-radius: var(--radius-sm);
    background: var(--surface-low);
  }

  form {
    display: flex;
    gap: 8px;
  }

  form .input {
    flex: 1;
    min-width: 0;
  }

  ul {
    margin: 0;
    padding: 0;
    list-style: none;
  }

  li {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    padding: 6px 0;
    border-top: 1px solid var(--border);
  }

  li code {
    overflow-wrap: anywhere;
  }
</style>
