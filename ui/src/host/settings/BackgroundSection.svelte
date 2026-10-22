<script lang="ts">
  import { TransferError } from '../../client/types';
  import { useHost } from '../host-context';
  import type { PreferencesView } from '../preferences-api';

  let { view, onchange }: { view: PreferencesView; onchange: (view: PreferencesView) => void } = $props();

  const { preferencesApi, toasts } = useHost();

  async function change(event: Event): Promise<void> {
    const input = event.target as HTMLInputElement;
    try {
      onchange(await preferencesApi.update({ keep_sharing_in_background: input.checked }));
    } catch (error) {
      input.checked = !input.checked;
      toasts.show('error', error instanceof TransferError ? error.message : 'Could not change sharing in the background.');
    }
  }
</script>

<section class="card">
  <h2>Background</h2>
  <label class="choice">
    <input type="checkbox" checked={view.preferences.keep_sharing_in_background} onchange={change} />
    <span>
      <strong>Keep sharing in background</strong>
      <span class="small faint">
        LADEX stays on after you close the app, with a notification to stop it. Otherwise it shares while the app is open or a
        transfer runs. Either way it pauses when this phone leaves Wi-Fi.
      </span>
    </span>
  </label>
</section>

<style>
  section {
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 20px;
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
</style>
