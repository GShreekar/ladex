<script lang="ts">
  import { onMount } from 'svelte';
  import { GuestClient, type GuestMe } from '../client/guest-client';
  import ToastViewport from '../components/ToastViewport.svelte';
  import { Toasts } from '../state/toasts.svelte';
  import JoinScreen from './JoinScreen.svelte';
  import RoomScreen from './RoomScreen.svelte';
  import { fetchMe, takeInvitationFragment } from './session';

  type Phase = 'loading' | 'join' | 'room' | 'unreachable';

  const toasts = new Toasts();
  const invitation = takeInvitationFragment();

  let phase = $state<Phase>('loading');
  let me = $state<GuestMe | null>(null);
  let endedNotice = $state(false);

  async function resume(): Promise<void> {
    try {
      me = await fetchMe();
      phase = me ? 'room' : 'join';
    } catch {
      phase = 'unreachable';
    }
  }

  function joined(): void {
    endedNotice = false;
    void resume();
  }

  function ended(): void {
    me = null;
    endedNotice = true;
    phase = 'join';
  }

  onMount(() => void resume());
</script>

{#if phase === 'loading'}
  <p class="center muted">Connecting…</p>
{:else if phase === 'unreachable'}
  <main class="center">
    <h1>Host not reachable</h1>
    <p class="muted">Check that you are on the same Wi-Fi as the host and that LADEX is still running there.</p>
    <button class="btn btn-primary" onclick={resume}>Try again</button>
  </main>
{:else if phase === 'join'}
  <JoinScreen {invitation} sessionEnded={endedNotice} onjoined={joined} />
{:else if me}
  {#key me.id}
    <RoomScreen client={new GuestClient(me)} {toasts} onended={ended} />
  {/key}
{/if}

<ToastViewport {toasts} />

<style>
  .center {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 12px;
    padding: 64px 24px;
    text-align: center;
  }
</style>
