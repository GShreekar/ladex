<script lang="ts">
  import { onMount } from 'svelte';
  import Icon from '../components/Icon.svelte';
  import { pageFingerprint, redeemInvitation, type InvitationFragment } from './session';

  let {
    invitation,
    sessionEnded,
    onjoined,
  }: { invitation: InvitationFragment; sessionEnded: boolean; onjoined: () => void } = $props();

  // A QR link carries the fingerprint the host's screen showed, so the person checks it before the token is spent.
  const needsFingerprintCheck = $derived(invitation.token !== null && invitation.fingerprint !== null);

  let code = $state('');
  let problem = $state('');
  let isJoining = $state(false);
  const nodeFingerprint = pageFingerprint();

  async function join(secret: string): Promise<void> {
    isJoining = true;
    problem = '';
    const refusal = await redeemInvitation(secret);
    isJoining = false;
    if (refusal === null) onjoined();
    else problem = refusal;
  }

  onMount(() => {
    if (invitation.token && !needsFingerprintCheck) void join(invitation.token);
  });
</script>

<main>
  <header>
    <span class="mark"><Icon name="hub" size={20} /></span>
    <strong>LADEX</strong>
    <span class="badge badge-accent">Guest link</span>
  </header>

  {#if sessionEnded}
    <div class="notice" role="status">
      <Icon name="info" size={20} />
      <p>Your session has ended. Ask the host for a new invitation to join again.</p>
    </div>
  {/if}

  <h1>Join the room</h1>
  <p class="muted">Enter the code shown on the host's screen, or scan the QR code with your camera.</p>

  {#if needsFingerprintCheck && invitation.token}
    {@const token = invitation.token}
    <section class="card panel">
      <h2>Check before you join</h2>
      <p class="muted">The invitation says the host's certificate fingerprint is:</p>
      <code>{invitation.fingerprint}</code>
      <p class="small muted">
        Open your browser's certificate details from the security warning and check that it shows exactly this. If it differs,
        someone else may be answering: close this page.
      </p>
      <button class="btn btn-primary btn-block" disabled={isJoining} onclick={() => join(token)}>
        <Icon name="shield" /> The fingerprints match, join
      </button>
      {#if problem}<p class="problem" role="alert">{problem}</p>{/if}
    </section>
  {:else}
    <form class="card panel" onsubmit={(event) => { event.preventDefault(); void join(code.trim()); }}>
      <label>
        <span class="small muted">Invitation code</span>
        <input
          class="input code"
          autocomplete="off"
          autocapitalize="characters"
          spellcheck="false"
          maxlength="9"
          placeholder="ABCD-EFGH"
          bind:value={code}
        />
      </label>
      {#if problem}<p class="problem" role="alert">{problem}</p>{/if}
      <button class="btn btn-primary btn-block" disabled={isJoining || !code.trim()}>
        <Icon name="arrow-right" /> Join the room
      </button>
    </form>
  {/if}

  {#if nodeFingerprint}
    <section class="card panel">
      <h2>This host's fingerprint</h2>
      <code>{nodeFingerprint}</code>
      <p class="small muted">Compare it with the one on the host's screen before you share anything.</p>
    </section>
  {/if}
</main>

<style>
  main {
    display: flex;
    flex-direction: column;
    gap: 16px;
    width: min(480px, 100%);
    margin: 0 auto;
    padding: 20px 16px 40px;
  }

  header {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-bottom: 8px;
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

  .panel {
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 20px;
  }

  label {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .code {
    height: 52px;
    font-family: ui-monospace, 'SFMono-Regular', Menlo, monospace;
    font-size: 22px;
    letter-spacing: 0.15em;
    text-align: center;
    text-transform: uppercase;
  }

  code {
    padding: 10px 12px;
    border-radius: var(--radius-sm);
    background: var(--surface-low);
    font-size: 12px;
    overflow-wrap: anywhere;
  }

  .problem {
    padding: 10px 12px;
    border-radius: var(--radius-sm);
    background: var(--danger-tint);
    color: var(--danger);
  }

  .notice {
    display: flex;
    gap: 10px;
    padding: 12px 14px;
    border-radius: var(--radius-md);
    background: var(--warning-tint);
    color: var(--warning);
  }

  .notice p {
    color: var(--text);
  }
</style>
