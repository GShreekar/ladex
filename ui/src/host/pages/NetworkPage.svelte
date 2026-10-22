<script lang="ts">
  import { onMount } from 'svelte';
  import { TransferError } from '../../client/types';
  import Icon from '../../components/Icon.svelte';
  import type { IconName } from '../../components/icons';
  import { useHost } from '../host-context';
  import { formatReport, withDeviceChecks, worstVerdict } from '../doctor-report';
  import AddressSection from '../network/AddressSection.svelte';
  import PairingSection from '../network/PairingSection.svelte';
  import TrustSection from '../network/TrustSection.svelte';
  import type { TrustedNode } from '../pairing-api';
  import type { Check, Verdict } from '../preferences-api';

  const { pairingApi, preferencesApi, desktop, isMobile, toasts } = useHost();

  const icons: Record<Verdict, IconName> = { ok: 'check-circle', warning: 'alert', problem: 'alert' };
  const summaries: Record<Verdict, string> = {
    ok: 'Everything looks fine.',
    warning: 'LADEX works, but something could be better.',
    problem: 'Something is stopping LADEX from working properly.',
  };

  let checks = $state<Check[]>([]);
  let trusted = $state<TrustedNode[]>([]);
  let isChecking = $state(false);
  let hasChecked = $state(false);

  const verdict = $derived(worstVerdict(checks));

  async function run(): Promise<void> {
    isChecking = true;
    try {
      const [nodeChecks, deviceChecks] = await Promise.all([
        preferencesApi.diagnose(),
        isMobile && desktop ? desktop.deviceChecks() : Promise.resolve([]),
      ]);
      checks = withDeviceChecks(nodeChecks, deviceChecks);
      hasChecked = true;
    } catch (error) {
      toasts.show('error', error instanceof TransferError ? error.message : 'Could not check the network.');
    } finally {
      isChecking = false;
    }
  }

  async function copyReport(): Promise<void> {
    const report = formatReport(checks, { userAgent: navigator.userAgent, when: new Date() });
    try {
      await navigator.clipboard.writeText(report);
      toasts.show('success', 'Report copied. Paste it into your bug report.');
    } catch {
      toasts.show('error', 'Could not copy. Select the text and copy it by hand.');
    }
  }

  async function loadTrusted(): Promise<void> {
    try {
      trusted = await pairingApi.trustedNodes();
    } catch (error) {
      toasts.show('error', error instanceof TransferError ? error.message : 'Could not list the trusted nodes.');
    }
  }

  onMount(() => {
    void run();
    void loadTrusted();
  });
</script>

<div class="title">
  <h1>Network</h1>
  <p class="muted">Checks that other devices can find and reach this one, and which nodes it trusts.</p>
</div>

<div class="sections">
  <section class="card summary {hasChecked ? verdict : ''}" aria-live="polite">
    {#if hasChecked}
      <Icon name={icons[verdict]} size={28} />
      <div>
        <h2>{summaries[verdict]}</h2>
        <p class="small muted">Checked just now.</p>
      </div>
    {:else}
      <Icon name="wifi" size={28} />
      <div><h2>Checking…</h2></div>
    {/if}
    <div class="actions">
      <button class="btn" disabled={isChecking} onclick={run}><Icon name="refresh" size={16} /> {isChecking ? 'Checking…' : 'Check again'}</button>
      <button class="btn" disabled={!hasChecked} onclick={copyReport}><Icon name="copy" size={16} /> Copy report</button>
    </div>
  </section>

  <AddressSection />

  {#if hasChecked}
    <section class="card">
      <ul>
        {#each checks as check (check.name)}
          <li class={check.verdict}>
            <span class="mark"><Icon name={icons[check.verdict]} size={20} /></span>
            <div>
              <strong>{check.name}</strong>
              <p class="small">{check.detail}</p>
            </div>
          </li>
        {/each}
      </ul>
    </section>
  {/if}

  <PairingSection onpaired={loadTrusted} />
  <TrustSection nodes={trusted} onchange={loadTrusted} />
</div>

<style>
  .title {
    margin-bottom: 20px;
  }

  .sections {
    display: flex;
    flex-direction: column;
    gap: 16px;
    max-width: 720px;
  }

  .summary {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 14px;
    padding: 18px 20px;
  }

  .summary > div:first-of-type {
    flex: 1;
    min-width: 180px;
  }

  .summary.ok {
    color: var(--success);
    background: var(--success-tint);
  }

  .summary.warning {
    color: var(--warning);
    background: var(--warning-tint);
  }

  .summary.problem {
    color: var(--danger);
    background: var(--danger-tint);
  }

  .summary h2,
  .summary p {
    color: var(--text);
  }

  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }

  ul {
    margin: 0;
    padding: 0;
    list-style: none;
  }

  li {
    display: flex;
    gap: 12px;
    padding: 14px 20px;
    border-top: 1px solid var(--border);
  }

  li:first-child {
    border-top: 0;
  }

  li p {
    color: var(--text-muted);
    overflow-wrap: anywhere;
  }

  li.ok .mark {
    color: var(--success);
  }

  li.warning .mark {
    color: var(--warning);
  }

  li.problem .mark {
    color: var(--danger);
  }
</style>
