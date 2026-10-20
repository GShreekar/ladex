<script lang="ts">
  import { onDestroy } from 'svelte';
  import { TransferError } from '../../client/types';
  import CopyButton from '../../components/CopyButton.svelte';
  import Icon from '../../components/Icon.svelte';
  import { useHost } from '../host-context';
  import type { PairingStatus, WaitingCode } from '../pairing-api';

  let { onpaired }: { onpaired: () => void } = $props();

  const { pairingApi, toasts } = useHost();

  const POLL_MS = 1000;

  let status = $state<PairingStatus | null>(null);
  let address = $state('');
  let timer: ReturnType<typeof setInterval> | null = null;
  let newestOutcome: string | null = null;

  function reportFailure(error: unknown, fallback: string): void {
    toasts.show('error', error instanceof TransferError ? error.message : fallback);
  }

  async function refresh(): Promise<void> {
    try {
      status = await pairingApi.status();
    } catch {
      return;
    }
    // Each finished pairing is announced once, when it first shows up.
    const newest = status.recent[0];
    const key = newest ? JSON.stringify(newest) : null;
    if (newest && key !== newestOutcome && newestOutcome !== null) {
      toasts.show(newest.paired ? 'success' : 'error', newest.paired ? `Paired with ${newest.name}` : `${newest.name}: ${newest.message}`);
      if (newest.paired) onpaired();
    }
    newestOutcome = key ?? '';
    if (!status.open && status.waiting.length === 0) stopPolling();
  }

  function stopPolling(): void {
    if (timer) clearInterval(timer);
    timer = null;
  }

  async function start(): Promise<void> {
    try {
      await pairingApi.open();
      await refresh();
      stopPolling();
      timer = setInterval(refresh, POLL_MS);
    } catch (error) {
      reportFailure(error, 'Could not start pairing.');
    }
  }

  // Leaving the page stops accepting pairings, so nobody can pair while no one is watching for codes.
  async function stop(): Promise<void> {
    stopPolling();
    try {
      await pairingApi.close();
      await refresh();
    } catch (error) {
      reportFailure(error, 'Could not stop pairing.');
    }
  }

  async function dial(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    if (!address.trim()) return;
    try {
      await pairingApi.dial(address.trim());
      address = '';
    } catch (error) {
      reportFailure(error, 'Could not dial that device.');
    }
  }

  async function answer(code: WaitingCode, accepted: boolean): Promise<void> {
    try {
      await pairingApi.answer(code.id, accepted);
      await refresh();
    } catch (error) {
      reportFailure(error, 'That pairing is no longer waiting.');
    }
  }

  function formatCountdown(seconds: number): string {
    return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`;
  }

  onDestroy(() => {
    if (timer) void stop();
  });
</script>

<section class="card pairing">
  <h2><Icon name="link" size={18} /> Pair a device</h2>
  <p class="small muted">
    Pairing makes two nodes trust each other's key, even with different passphrases. Both people compare six words, so nobody in between
    can pretend to be the other device.
  </p>

  {#if status?.open}
    <div class="window">
      {#if status.address}
        <div class="own">
          <span class="small faint">This device's address</span>
          <code>{status.address}</code>
          <CopyButton text={status.address} label="Copy this device's address" />
        </div>
      {/if}
      <p class="small">Accepting pairings for {formatCountdown(status.seconds_left)}. Type the address above on the other device, or dial it from here:</p>
      <form onsubmit={dial}>
        <label for="pair-address" class="sr-only">The other device's address</label>
        <input id="pair-address" class="input" bind:value={address} placeholder="192.168.1.20:8080" autocomplete="off" spellcheck="false" />
        <button class="btn" disabled={!address.trim()}>Dial</button>
      </form>
      <button class="btn btn-ghost btn-small" onclick={stop}>Stop pairing</button>
    </div>
  {:else}
    <button class="btn btn-primary" onclick={start}><Icon name="link" size={16} /> Pair a device</button>
  {/if}

  {#each status?.waiting ?? [] as code (code.id)}
    <div class="code" aria-live="polite">
      <strong>{code.dialed ? `Pairing with ${code.name}` : `${code.name} wants to pair`}</strong>
      <p class="small muted">Check that the other device shows these same words, in this order:</p>
      <div class="words">
        {#each code.words as word, index (index)}<span>{word}</span>{/each}
      </div>
      <div class="actions">
        <button class="btn btn-small" onclick={() => answer(code, false)}>They don't match</button>
        <button class="btn btn-primary btn-small" onclick={() => answer(code, true)}><Icon name="check" size={15} /> They match</button>
      </div>
    </div>
  {/each}
</section>

<style>
  .pairing {
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

  .window {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 10px;
  }

  .own {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 4px 4px 12px;
    border-radius: var(--radius-sm);
    background: var(--surface-low);
  }

  form {
    display: flex;
    gap: 8px;
    width: 100%;
  }

  form .input {
    flex: 1;
  }

  .code {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 14px;
    border-radius: var(--radius-md);
    background: var(--accent-tint);
  }

  .words {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }

  .words span {
    padding: 4px 10px;
    border-radius: var(--radius-sm);
    background: var(--surface);
    color: var(--on-accent-tint);
    font-family: ui-monospace, 'SFMono-Regular', Menlo, monospace;
    font-size: 15px;
  }

  .actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
  }
</style>
