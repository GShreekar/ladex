<script lang="ts">
  import Icon from '../components/Icon.svelte';

  let { onsignedin }: { onsignedin: () => void } = $props();

  let passphrase = $state('');
  let problem = $state('');
  let isSubmitting = $state(false);

  async function submit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    isSubmitting = true;
    problem = '';
    try {
      const response = await fetch('/auth', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ passphrase }),
      });
      const body = (await response.json().catch(() => ({}))) as { message?: string };
      if (response.ok) onsignedin();
      else problem = body.message ?? 'Could not sign in.';
    } catch {
      problem = 'Could not reach the node.';
    } finally {
      isSubmitting = false;
    }
  }
</script>

<main>
  <form class="card" onsubmit={submit}>
    <div class="logo"><Icon name="hub" size={28} /></div>
    <h1>Sign in to LADEX</h1>
    <p class="muted">Enter the passphrase this mesh was created with.</p>
    <label>
      <span class="small muted">Passphrase</span>
      <input class="input" type="password" autocomplete="current-password" bind:value={passphrase} required />
    </label>
    {#if problem}<p class="problem" role="alert">{problem}</p>{/if}
    <button class="btn btn-primary btn-block" disabled={isSubmitting || !passphrase}>Sign in</button>
  </form>
</main>

<style>
  main {
    min-height: var(--viewport-height);
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    place-items: center;
    padding: 16px;
  }

  form {
    display: flex;
    flex-direction: column;
    gap: 16px;
    width: min(400px, 100%);
    padding: 32px;
  }

  .logo {
    display: grid;
    place-items: center;
    width: 48px;
    height: 48px;
    border-radius: var(--radius-md);
    background: var(--accent);
    color: var(--on-accent);
  }

  label {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .problem {
    padding: 10px 12px;
    border-radius: var(--radius-sm);
    background: var(--danger-tint);
    color: var(--danger);
  }
</style>
