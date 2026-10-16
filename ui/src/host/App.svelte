<script lang="ts">
  import { onMount } from 'svelte';
  import ToastViewport from '../components/ToastViewport.svelte';
  import { useApp } from '../state/context';
  import Login from './Login.svelte';
  import Shell from './Shell.svelte';

  const { room, transfers, toasts } = useApp();

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
