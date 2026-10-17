<script lang="ts">
  import { useHost } from '../host-context';

  const { desktop, toasts } = useHost();

  let launches = $state(false);

  $effect(() => {
    void desktop?.launchesAtLogin().then((is) => (launches = is));
  });

  async function change(event: Event): Promise<void> {
    const wanted = (event.target as HTMLInputElement).checked;
    try {
      launches = (await desktop?.setLaunchesAtLogin(wanted)) ?? false;
    } catch {
      launches = !wanted;
      toasts.show('error', 'Could not change what happens at login.');
    }
  }
</script>

<section class="card">
  <h2>Startup</h2>
  <label class="choice">
    <input type="checkbox" checked={launches} onchange={change} />
    <span>
      <strong>Start LADEX when I log in</strong>
      <span class="small faint">It starts in the tray without a window, so files can still reach you.</span>
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
