<script lang="ts">
  import Icon from '../components/Icon.svelte';
  import type { IconName } from '../components/icons';
  import { useApp } from '../state/context';
  import DropOverlay from './DropOverlay.svelte';
  import { useHost } from './host-context';
  import IncomingShareDialog from './IncomingShareDialog.svelte';
  import FilesPage from './pages/FilesPage.svelte';
  import GuestsPage from './pages/GuestsPage.svelte';
  import NetworkPage from './pages/NetworkPage.svelte';
  import OfferDialog from './OfferDialog.svelte';
  import SettingsPage from './pages/SettingsPage.svelte';
  import { shell, type Page } from './shell-state.svelte';
  import TransfersPage from './pages/TransfersPage.svelte';

  const { room, transfers } = useApp();
  const { phone } = useHost();

  let fileChooser: HTMLInputElement;

  const navigation = $derived<Array<{ page: Page; label: string; icon: IconName; badge?: number }>>([
    { page: 'files', label: 'Shared files', icon: 'files' },
    { page: 'transfers', label: 'Transfers', icon: 'swap', badge: transfers.active.length },
    ...(room.capabilities.canManageGuests ? [{ page: 'guests' as const, label: 'Guests', icon: 'users' as const }] : []),
    ...(room.capabilities.canManageGuests ? [{ page: 'network' as const, label: 'Network', icon: 'wifi' as const }] : []),
    { page: 'settings', label: 'Settings', icon: 'settings' },
  ]);

  const connectionLabel = $derived(
    room.connection === 'online'
      ? `Connected to ${room.otherDevices.length} other device${room.otherDevices.length === 1 ? '' : 's'}`
      : room.connection === 'connecting'
        ? 'Connecting…'
        : 'Disconnected, retrying…',
  );

  function chooseFiles(event: Event): void {
    const input = event.target as HTMLInputElement;
    void transfers.share([...(input.files ?? [])]);
    input.value = '';
  }
</script>

<div class="shell">
  <aside>
    <div class="brand">
      <span class="mark"><Icon name="hub" size={20} /></span>
      <span class="brand-text"><strong>LADEX</strong><small>LOCAL MESH</small></span>
    </div>

    <nav aria-label="Main">
      {#each navigation as item (item.page)}
        <button class="nav-item" class:active={shell.page === item.page} onclick={() => shell.go(item.page)}>
          <Icon name={item.icon} size={20} />
          <span>{item.label}</span>
          {#if item.badge}<span class="count">{item.badge}</span>{/if}
        </button>
      {/each}
    </nav>

    <div class="identity">
      <div class="name truncate">{room.self.name}</div>
      <div class="status" class:offline={room.connection !== 'online'}>
        <span class="dot"></span>{connectionLabel}
      </div>
      <div class="small faint lock"><Icon name="lock" size={14} /> Encrypted, direct on your network</div>
    </div>
  </aside>

  <div class="workspace">
    <header>
      <label class="search">
        <Icon name="search" size={18} />
        <input class="input" type="search" placeholder="Search shared files" bind:value={shell.query} onfocus={() => shell.go('files')} />
      </label>
      {#if room.capabilities.canManageGuests}
        <button class="btn" onclick={() => shell.go('guests')}><Icon name="user-plus" /> <span class="label">Invite guest</span></button>
      {/if}
      <button class="btn btn-primary" onclick={() => fileChooser.click()}><Icon name="plus" /> <span class="label">Share files</span></button>
      <input bind:this={fileChooser} type="file" multiple hidden onchange={chooseFiles} />
    </header>

    <main>
      {#if shell.page === 'files'}
        <FilesPage />
      {:else if shell.page === 'transfers'}
        <TransfersPage />
      {:else if shell.page === 'guests' && room.capabilities.canManageGuests}
        <GuestsPage />
      {:else if shell.page === 'network' && room.capabilities.canManageGuests}
        <NetworkPage />
      {:else}
        <SettingsPage />
      {/if}
    </main>
  </div>
</div>

<DropOverlay />

{#if room.offers[0]}
  <OfferDialog offer={room.offers[0]} />
{/if}

{#if phone}
  <IncomingShareDialog {phone} />
{/if}

<style>
  .shell {
    display: grid;
    grid-template-columns: 240px 1fr;
    min-height: var(--viewport-height);
  }

  aside {
    position: sticky;
    top: var(--safe-top);
    align-self: start;
    height: var(--viewport-height);
    display: flex;
    flex-direction: column;
    background: var(--surface);
    border-right: 1px solid var(--border);
  }

  .brand {
    display: flex;
    align-items: center;
    gap: 10px;
    height: 64px;
    padding: 0 16px;
    border-bottom: 1px solid var(--border);
  }

  .mark {
    display: grid;
    place-items: center;
    width: 32px;
    height: 32px;
    border-radius: var(--radius-sm);
    background: var(--accent);
    color: var(--on-accent);
  }

  .brand-text {
    display: flex;
    flex-direction: column;
    line-height: 1.1;
  }

  .brand-text small {
    color: var(--accent-text);
    font-size: 10px;
    font-weight: 600;
    letter-spacing: 0.08em;
  }

  nav {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 8px;
  }

  .nav-item {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 8px 12px;
    border: 0;
    border-radius: var(--radius-sm);
    background: transparent;
    color: var(--text-muted);
    text-align: left;
  }

  .nav-item:hover {
    background: var(--surface-low);
    color: var(--text);
  }

  .nav-item.active {
    background: var(--surface-high);
    color: var(--accent-text);
    font-weight: 500;
  }

  .count {
    margin-left: auto;
    padding: 0 8px;
    border-radius: 9999px;
    background: var(--accent-tint);
    color: var(--on-accent-tint);
    font-size: 12px;
    font-weight: 500;
  }

  .identity {
    margin-top: auto;
    padding: 16px;
    border-top: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .name {
    font-weight: 500;
  }

  .status {
    display: flex;
    align-items: center;
    gap: 6px;
    color: var(--success);
    font-size: 12px;
    font-weight: 500;
  }

  .status.offline {
    color: var(--warning);
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 9999px;
    background: currentColor;
  }

  .lock {
    display: flex;
    align-items: center;
    gap: 6px;
  }

  .workspace {
    min-width: 0;
  }

  header {
    position: sticky;
    top: var(--safe-top);
    z-index: 10;
    display: flex;
    align-items: center;
    gap: 12px;
    height: 64px;
    padding: 0 24px;
    background: var(--surface);
    border-bottom: 1px solid var(--border);
  }

  .search {
    position: relative;
    flex: 1;
    max-width: 440px;
    margin-right: auto;
  }

  .search :global(svg) {
    position: absolute;
    left: 12px;
    top: 9px;
    color: var(--text-faint);
  }

  .search input {
    padding-left: 38px;
  }

  main {
    padding: 24px;
  }

  @media (max-width: 900px) {
    .shell {
      grid-template-columns: 1fr;
    }

    aside {
      position: static;
      height: auto;
      flex-direction: row;
      align-items: center;
      border-right: 0;
      border-bottom: 1px solid var(--border);
    }

    .brand {
      border-bottom: 0;
    }

    nav {
      flex-direction: row;
      overflow-x: auto;
    }

    .nav-item span:not(.count) {
      display: none;
    }

    .identity {
      display: none;
    }

    header {
      padding: 0 12px;
    }

    .label {
      display: none;
    }

    main {
      padding: 16px;
    }
  }

  /* Phones: the navigation moves to a bar along the bottom, within reach of a thumb. */
  @media (max-width: 600px) {
    :global(:root) {
      --bottom-bar-height: 64px;
    }

    .shell {
      padding-bottom: calc(var(--bottom-bar-height) + var(--safe-bottom));
    }

    aside {
      position: fixed;
      inset: auto 0 0;
      z-index: 20;
      padding: 0 var(--safe-right) var(--safe-bottom) var(--safe-left);
      border-top: 1px solid var(--border);
      border-bottom: 0;
    }

    .brand {
      display: none;
    }

    nav {
      flex: 1;
      gap: 0;
      padding: 0;
      overflow: visible;
    }

    .nav-item {
      position: relative;
      flex: 1;
      flex-direction: column;
      justify-content: center;
      gap: 2px;
      min-width: 0;
      height: var(--bottom-bar-height);
      padding: 6px 2px;
      font-size: 11px;
      line-height: 14px;
      text-align: center;
    }

    .nav-item span:not(.count) {
      display: block;
      max-width: 100%;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }

    .nav-item:hover {
      background: transparent;
    }

    .count {
      position: absolute;
      top: 6px;
      left: calc(50% + 6px);
      margin-left: 0;
      padding: 0 6px;
      font-size: 10px;
    }

    header {
      gap: 8px;
    }
  }
</style>
