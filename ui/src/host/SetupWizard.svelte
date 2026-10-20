<script lang="ts">
  import { untrack } from 'svelte';
  import { saveNickname } from '../client/local-node-client';
  import { formatBytes } from '../client/format';
  import Icon from '../components/Icon.svelte';
  import type { DesktopSettings, DesktopSetup, SetupState } from '../desktop/desktop';
  import {
    SETUP_STEPS,
    chosenPassphrase,
    nextStep,
    previousStep,
    problemAt,
    suggestedStorageGib,
    type SetupAnswers,
    type SetupStep,
  } from './setup-flow';

  let {
    setup,
    pickFolder,
    state: initial,
    isMobile,
  }: { setup: DesktopSetup; pickFolder: DesktopSettings['pickFolder']; state: SetupState; isMobile: boolean } = $props();

  const TITLES: Record<SetupStep, string> = {
    welcome: 'Welcome to LADEX',
    mesh: 'Your mesh',
    files: 'Saving files',
    finish: 'Ready to go',
  };

  let step = $state<SetupStep>('welcome');
  let isWorking = $state(false);
  let failure = $state<string | null>(null);
  let shouldLaunchAtLogin = $state(false);
  let copyState = $state<'idle' | 'copied' | 'failed'>('idle');
  // What the machine offered when the wizard opened is only the starting point of the answers.
  let answers = $state<SetupAnswers>(
    untrack(() => ({
      deviceName: '',
      meshChoice: 'create',
      createdPassphrase: initial.suggestedPassphrase,
      joinedPassphrase: '',
      downloadsDir: null,
      storageLimitGib: suggestedStorageGib(initial.defaultStorageGib, initial.freeBytes),
    })),
  );

  const stepNumber = $derived(SETUP_STEPS.indexOf(step) + 1);
  const problem = $derived(problemAt(step, answers, initial.freeBytes));
  const folder = $derived(answers.downloadsDir ?? initial.defaultDownloads ?? 'your Downloads folder');

  async function remakePassphrase(): Promise<void> {
    answers.createdPassphrase = await setup.suggestPassphrase();
    copyState = 'idle';
  }

  async function copyPassphrase(): Promise<void> {
    try {
      await navigator.clipboard.writeText(answers.createdPassphrase);
      copyState = 'copied';
    } catch {
      copyState = 'failed';
    }
  }

  async function chooseFolder(): Promise<void> {
    const picked = await pickFolder('Where should received files be saved?');
    if (picked) answers.downloadsDir = picked;
  }

  async function finish(): Promise<void> {
    isWorking = true;
    failure = null;
    if (answers.deviceName.trim()) saveNickname(answers.deviceName.trim());
    try {
      // The window reloads onto the running app when this succeeds.
      await setup.finish({
        passphrase: chosenPassphrase(answers),
        downloadsDir: answers.downloadsDir,
        storageLimitGib: answers.storageLimitGib,
        launchAtLogin: shouldLaunchAtLogin,
      });
    } catch (error) {
      failure = String(error);
      isWorking = false;
    }
  }

  function advance(): void {
    if (problem) return;
    if (step === 'finish') void finish();
    else step = nextStep(step);
  }
</script>

<main>
  <form class="card wizard" onsubmit={(event) => (event.preventDefault(), advance())}>
    <header>
      <span class="mark"><Icon name="hub" size={22} /></span>
      <div>
        <h1>{TITLES[step]}</h1>
        <p class="small faint">Step {stepNumber} of {SETUP_STEPS.length}</p>
      </div>
    </header>

    <div class="progress" aria-hidden="true"><span style:width="{(stepNumber / SETUP_STEPS.length) * 100}%"></span></div>

    {#if step === 'welcome'}
      <p>LADEX shares files between devices on the same Wi-Fi or network, directly and without the internet. Setting up takes a minute.</p>
      <label class="field">
        <span class="label">What should other people see this device as?</span>
        <input class="input" maxlength="40" placeholder="For example, Sam’s laptop" bind:value={answers.deviceName} />
        <span class="small faint">You can change it later in Settings.</span>
      </label>
    {:else if step === 'mesh'}
      <p>A mesh is the group of devices that share with each other. A passphrase keeps everyone else on the network out.</p>
      <div class="choices" role="radiogroup" aria-label="Start a mesh or join one">
        <button type="button" role="radio" aria-checked={answers.meshChoice === 'create'} class:active={answers.meshChoice === 'create'} onclick={() => (answers.meshChoice = 'create')}>
          <strong>Start a new mesh</strong>
          <span class="small">This is the first device</span>
        </button>
        <button type="button" role="radio" aria-checked={answers.meshChoice === 'join'} class:active={answers.meshChoice === 'join'} onclick={() => (answers.meshChoice = 'join')}>
          <strong>Join an existing mesh</strong>
          <span class="small">Another device is already set up</span>
        </button>
      </div>

      {#if answers.meshChoice === 'create'}
        <div class="passphrase">
          <span class="small faint">YOUR PASSPHRASE</span>
          <strong class="code">{answers.createdPassphrase}</strong>
          <div class="row">
            <button type="button" class="btn btn-small" onclick={copyPassphrase}>
              <Icon name={copyState === 'copied' ? 'check' : 'copy'} size={15} /> {copyState === 'copied' ? 'Copied' : 'Copy'}
            </button>
            <button type="button" class="btn btn-small" onclick={remakePassphrase}><Icon name="refresh" size={15} /> Make another</button>
          </div>
          {#if copyState === 'failed'}<span class="small faint">Could not copy. Select it and copy it by hand.</span>{/if}
        </div>
        <p class="small muted">You type it on each computer you add to the mesh; you can see it again later in Settings. Phones don’t need it: they join by scanning a QR code. Anyone who has it can join, so share it only with people you trust.</p>
      {:else}
        <label class="field">
          <span class="label">Passphrase of the mesh</span>
          <input class="input code-input" bind:value={answers.joinedPassphrase} autocomplete="off" spellcheck="false" placeholder="Shown on the device that started it" />
          <span class="small faint">On a computer already in the mesh, open Settings and choose Show passphrase.</span>
        </label>
      {/if}
    {:else if step === 'files'}
      <div class="field">
        <span class="label">Save received files in</span>
        <div class="row">
          <code class="path truncate">{folder}</code>
          {#if !isMobile}<button type="button" class="btn" onclick={chooseFolder}>Change…</button>{/if}
        </div>
      </div>
      <label class="field">
        <span class="label">Most space LADEX may use for shared files</span>
        <div class="row">
          <input class="input number" type="number" min="1" bind:value={answers.storageLimitGib} />
          <span class="muted">GB</span>
          {#if initial.freeBytes !== null}<span class="small faint">{formatBytes(initial.freeBytes)} free on this disk</span>{/if}
        </div>
      </label>
    {:else}
      <ul class="summary">
        <li><Icon name="check-circle" size={18} /> {answers.meshChoice === 'create' ? 'A new mesh, protected by your passphrase' : 'Joining an existing mesh with its passphrase'}</li>
        <li><Icon name="check-circle" size={18} /> Received files go to <code class="truncate">{folder}</code></li>
        <li><Icon name="check-circle" size={18} /> Up to {answers.storageLimitGib} GB of shared files</li>
      </ul>
      {#if isMobile}
        <p class="small muted">Next you can invite another phone by QR code and share files.</p>
      {:else}
        <label class="choice">
          <input type="checkbox" bind:checked={shouldLaunchAtLogin} />
          <span>
            <strong>Start LADEX when I log in</strong>
            <span class="small faint">It waits in the tray, so files can reach you without opening a window.</span>
          </span>
        </label>
        <p class="small muted">Next you can invite a phone by QR code and share a folder. Closing the window keeps LADEX running in the tray.</p>
      {/if}
    {/if}

    {#if problem && step !== 'welcome'}<p class="problem small" role="alert">{problem}</p>{/if}
    {#if failure}<p class="problem" role="alert">LADEX could not start: {failure}</p>{/if}

    <footer>
      {#if step !== 'welcome'}
        <button type="button" class="btn btn-ghost" disabled={isWorking} onclick={() => (step = previousStep(step))}>Back</button>
      {/if}
      <button class="btn btn-primary" disabled={!!problem || isWorking}>
        {step === 'finish' ? (isWorking ? 'Starting…' : 'Start LADEX') : 'Continue'}
      </button>
    </footer>
  </form>
</main>

<style>
  main {
    display: grid;
    place-items: center;
    min-height: var(--viewport-height);
    padding: 24px 16px calc(24px + var(--safe-bottom));
  }

  .wizard {
    display: flex;
    flex-direction: column;
    gap: 16px;
    width: min(560px, 100%);
    padding: 28px;
  }

  header {
    display: flex;
    align-items: center;
    gap: 14px;
  }

  .mark {
    display: grid;
    place-items: center;
    width: 40px;
    height: 40px;
    border-radius: var(--radius-sm);
    background: var(--accent);
    color: var(--on-accent);
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .label {
    font-weight: 500;
  }

  .row {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .path {
    flex: 1;
    padding: 8px 12px;
    border-radius: var(--radius-sm);
    background: var(--surface-low);
  }

  .number {
    width: 110px;
  }

  .choices {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px;
  }

  .choices button {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 12px;
    border: 1.5px solid var(--border-strong);
    border-radius: var(--radius-md);
    background: var(--surface);
    text-align: left;
  }

  .choices button.active {
    border-color: var(--accent);
    background: var(--accent-tint);
    color: var(--on-accent-tint);
  }

  .passphrase {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
    padding: 16px;
    border-radius: var(--radius-md);
    background: var(--accent-tint);
  }

  .code {
    color: var(--on-accent-tint);
    font-family: ui-monospace, 'SFMono-Regular', Menlo, monospace;
    font-size: 24px;
    letter-spacing: 0.1em;
  }

  .code-input {
    font-family: ui-monospace, 'SFMono-Regular', Menlo, monospace;
  }

  .summary {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .summary li {
    display: flex;
    align-items: center;
    gap: 8px;
    color: var(--success);
  }

  .summary li :global(code) {
    color: var(--text);
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

  .problem {
    color: var(--danger);
  }

  footer {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 8px;
  }
</style>
