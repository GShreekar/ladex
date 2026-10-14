<script lang="ts">
  import { onMount } from 'svelte';
  import { LocalNodeClient } from '../client/local-node-client';
  import ToastViewport from '../components/ToastViewport.svelte';
  import { provideApp } from '../state/context';
  import { Room } from '../state/room.svelte';
  import { Toasts } from '../state/toasts.svelte';
  import { Transfers } from '../state/transfers.svelte';
  import Login from './Login.svelte';
  import Shell from './Shell.svelte';

  const toasts = new Toasts();
  const client = new LocalNodeClient();
  const room = new Room(client, toasts);
  const transfers = new Transfers(client, toasts);
  provideApp({ room, transfers, toasts });

  onMount(() => {
    void room.start();
    return () => room.dispose();
  });

  // Leaving mid-upload stops it: the page does the sending, the node only keeps what already arrived.
  function warnBeforeLeaving(event: BeforeUnloadEvent): void {
    if (!transfers.hasActive) return;
    event.preventDefault();
    event.returnValue = '';
  }
</script>

<svelte:window onbeforeunload={warnBeforeLeaving} />

{#if room.connection === 'signed-out'}
  <Login onsignedin={() => room.start()} />
{:else}
  <Shell />
{/if}

<ToastViewport {toasts} />
