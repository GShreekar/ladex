<script lang="ts">
  import { TransferError } from '../../client/types';
  import { useHost } from '../host-context';
  import type { AutoAcceptFrom, AutoAcceptRule, PreferencesView } from '../preferences-api';

  let { view, onchange }: { view: PreferencesView; onchange: (view: PreferencesView) => void } = $props();

  const { preferencesApi, desktop, phone, toasts } = useHost();

  const DEFAULT_SIZE_LIMIT_MIB = 500;
  const rules: Array<{ id: AutoAcceptFrom; label: string; hint: string }> = [
    { id: 'nobody', label: 'Always ask me', hint: 'Every file sent to this device waits for your answer.' },
    { id: 'paired', label: 'Accept from my paired devices', hint: 'Devices you paired with a code, and this computer’s own. Everyone else is asked about.' },
    { id: 'mesh', label: 'Accept from anyone in this mesh', hint: 'Everyone who knows the passphrase can save files here without asking.' },
  ];

  const rule = $derived(view.preferences.auto_accept);
  const hasSizeLimit = $derived(rule.ask_above_mib !== null);

  let downloadsFolder = $state<string | null>(null);
  // On a phone: the folder chosen through Android, by its name; none keeps files in LADEX's own storage.
  let phoneFolder = $state<string | null>(null);
  let sizeLimitText = $state(String(DEFAULT_SIZE_LIMIT_MIB));

  $effect(() => {
    sizeLimitText = String(view.preferences.auto_accept.ask_above_mib ?? DEFAULT_SIZE_LIMIT_MIB);
  });

  $effect(() => {
    if (phone) void phone.saveFolder().then((folder) => (phoneFolder = folder));
    else void desktop?.downloadsFolder().then((folder) => (downloadsFolder = folder));
  });

  async function save(update: Parameters<typeof preferencesApi.update>[0]): Promise<void> {
    try {
      onchange(await preferencesApi.update(update));
      void desktop?.downloadsFolder().then((folder) => (downloadsFolder = folder));
    } catch (error) {
      toasts.show('error', error instanceof TransferError ? error.message : 'Could not save that setting.');
    }
  }

  function chooseRule(from: AutoAcceptFrom): void {
    void save({ auto_accept: { ...rule, from } });
  }

  function toggleSizeLimit(event: Event): void {
    const isOn = (event.target as HTMLInputElement).checked;
    const ask_above_mib = isOn ? Number(sizeLimitText) || DEFAULT_SIZE_LIMIT_MIB : null;
    void save({ auto_accept: { ...rule, ask_above_mib } satisfies AutoAcceptRule });
  }

  function saveSizeLimit(): void {
    const mib = Math.floor(Number(sizeLimitText));
    if (!(mib >= 1)) return void toasts.show('error', 'Choose a size of at least 1 MB.');
    void save({ auto_accept: { ...rule, ask_above_mib: mib } });
  }

  async function choosePhoneFolder(): Promise<void> {
    try {
      phoneFolder = (await phone?.pickSaveFolder()) ?? null;
    } catch (error) {
      toasts.show('error', String(error));
    }
  }

  async function stopUsingPhoneFolder(): Promise<void> {
    await phone?.forgetSaveFolder();
    phoneFolder = null;
  }

  async function chooseFolder(): Promise<void> {
    const folder = await desktop?.pickFolder('Where should received files be saved?');
    if (folder) await save({ downloads_dir: folder });
  }
</script>

<section class="card">
  <h2>Receiving files</h2>

  {#if phone}
    <div class="field">
      <span class="label" id="downloads-label">Save received files in</span>
      <div class="row">
        <code class="path truncate" aria-labelledby="downloads-label">{phoneFolder ?? 'LADEX’s own storage'}</code>
        <button class="btn" onclick={choosePhoneFolder}>{phoneFolder ? 'Change…' : 'Choose folder…'}</button>
        {#if phoneFolder}<button class="btn btn-ghost" onclick={stopUsingPhoneFolder}>Stop using it</button>{/if}
      </div>
      {#if !phoneFolder}
        <span class="small faint">Only LADEX can see files in its own storage. Choose a folder to find them in the Files app.</span>
      {/if}
    </div>
  {:else if desktop}
    <div class="field">
      <span class="label" id="downloads-label">Save received files in</span>
      <div class="row">
        <code class="path truncate" aria-labelledby="downloads-label">{downloadsFolder ?? 'the system downloads folder'}</code>
        <button class="btn" onclick={chooseFolder}>Change…</button>
        {#if view.preferences.downloads_dir}
          <button class="btn btn-ghost" onclick={() => save({ downloads_dir: '' })}>Use the default</button>
        {/if}
      </div>
    </div>
  {/if}

  <fieldset>
    <legend class="label">When someone sends a file to this device</legend>
    {#each rules as option (option.id)}
      <label class="choice">
        <input type="radio" name="auto-accept" value={option.id} checked={rule.from === option.id} onchange={() => chooseRule(option.id)} />
        <span><strong>{option.label}</strong><span class="small faint">{option.hint}</span></span>
      </label>
    {/each}

    {#if rule.from !== 'nobody'}
      <div class="size-limit">
        <label class="choice">
          <input type="checkbox" checked={hasSizeLimit} onchange={toggleSizeLimit} />
          <span>Still ask first for files larger than</span>
        </label>
        <input
          class="input number"
          type="number"
          min="1"
          aria-label="Size in megabytes"
          bind:value={sizeLimitText}
          disabled={!hasSizeLimit}
          onchange={saveSizeLimit}
        />
        <span class="muted">MB</span>
      </div>
    {/if}
  </fieldset>
</section>

<style>
  section {
    display: flex;
    flex-direction: column;
    gap: 16px;
    padding: 20px;
  }

  fieldset {
    display: flex;
    flex-direction: column;
    gap: 10px;
    margin: 0;
    padding: 0;
    border: 0;
  }

  .label {
    margin-bottom: 4px;
    font-weight: 500;
  }

  .field,
  .row {
    display: flex;
    gap: 8px;
  }

  .field {
    flex-direction: column;
  }

  .row {
    flex-wrap: wrap;
    align-items: center;
  }

  .path {
    flex: 1;
    min-width: 160px;
    padding: 8px 12px;
    border-radius: var(--radius-sm);
    background: var(--surface-low);
  }

  .choice {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    cursor: pointer;
  }

  .choice span {
    display: flex;
    flex-direction: column;
  }

  .choice input {
    margin-top: 4px;
  }

  .size-limit {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
    padding-left: 22px;
  }

  .number {
    width: 96px;
  }
</style>
