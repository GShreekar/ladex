<script lang="ts">
  import { onMount } from 'svelte';
  import Icon from '../../components/Icon.svelte';
  import { theme, type ThemePreference } from '../../state/theme.svelte';
  import { useHost } from '../host-context';
  import type { PreferencesView } from '../preferences-api';
  import BackgroundSection from '../settings/BackgroundSection.svelte';
  import CommandLineSection from '../settings/CommandLineSection.svelte';
  import PassphraseSection from '../settings/PassphraseSection.svelte';
  import ReceivingSection from '../settings/ReceivingSection.svelte';
  import StartupSection from '../settings/StartupSection.svelte';
  import StorageSection from '../settings/StorageSection.svelte';

  const { room, preferencesApi, desktop, isMobile, toasts } = useHost();

  let nickname = $state('');
  let view = $state<PreferencesView | null>(null);

  // Only the machine running the node may change how it is set up.
  const isHost = room.capabilities.canManageGuests;

  onMount(async () => {
    if (!isHost) return;
    try {
      view = await preferencesApi.get();
    } catch {
      toasts.show('error', 'Could not load the settings of this node.');
    }
  });

  const themes: Array<{ id: ThemePreference; label: string }> = [
    { id: 'system', label: 'System' },
    { id: 'light', label: 'Light' },
    { id: 'dark', label: 'Dark' },
  ];

  function saveName(event: SubmitEvent): void {
    event.preventDefault();
    if (nickname.trim()) room.client.rename(nickname);
    nickname = '';
  }
</script>

<div class="title"><h1>Settings</h1></div>

<div class="sections">
  {#if room.capabilities.canRename}
    <section class="card">
      <h2>This device</h2>
      <p class="muted">Other devices see this name in their device list.</p>
      <form onsubmit={saveName}>
        <input class="input" maxlength="40" placeholder={room.self.name} bind:value={nickname} aria-label="Device name" />
        <button class="btn btn-primary" disabled={!nickname.trim()}>Save</button>
      </form>
      <p class="small faint">Connected through {room.self.nodeName || 'this node'}.</p>
    </section>
  {/if}

  {#if isHost && view}
    <ReceivingSection {view} onchange={(next) => (view = next)} />
    <StorageSection {view} onchange={(next) => (view = next)} />
    <PassphraseSection {view} onchange={(next) => (view = next)} />
    {#if desktop && !isMobile}<StartupSection />{/if}
    {#if isMobile}<BackgroundSection {view} onchange={(next) => (view = next)} />{/if}
  {/if}

  <section class="card">
    <h2>Appearance</h2>
    <div class="themes" role="radiogroup" aria-label="Theme">
      {#each themes as option (option.id)}
        <button role="radio" aria-checked={theme.preference === option.id} class:active={theme.preference === option.id} onclick={() => theme.choose(option.id)}>
          {option.label}
        </button>
      {/each}
    </div>
  </section>

  {#if isHost && view && !isMobile}
    <CommandLineSection {view} />
  {/if}

  {#if room.capabilities.canLeave}
    <section class="card">
      <h2>Session</h2>
      <p class="muted">Signing out ends this browser's access. Other devices stay signed in.</p>
      <div><button class="btn btn-danger" onclick={() => room.client.leave()}><Icon name="log-out" /> Sign out</button></div>
    </section>
  {/if}
</div>

<style>
  .title {
    margin-bottom: 20px;
  }

  .sections {
    display: flex;
    flex-direction: column;
    gap: 16px;
    max-width: 640px;
  }

  section {
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 20px;
  }

  form {
    display: flex;
    gap: 8px;
  }

  .themes {
    display: flex;
    gap: 2px;
    align-self: flex-start;
    padding: 3px;
    border-radius: var(--radius-sm);
    background: var(--surface-low);
  }

  .themes button {
    padding: 6px 16px;
    border: 0;
    border-radius: 6px;
    background: transparent;
    color: var(--text-muted);
  }

  .themes button.active {
    background: var(--surface);
    color: var(--accent-text);
    font-weight: 500;
  }
</style>
