<script lang="ts">
  import { TransferError } from '../../client/types';
  import Icon from '../../components/Icon.svelte';
  import { useHost } from '../host-context';
  import type { TrustedNode } from '../pairing-api';

  let { nodes, onchange }: { nodes: TrustedNode[]; onchange: () => void } = $props();

  const { pairingApi, toasts } = useHost();

  // Revoking cannot be undone, so the button asks once more before it acts.
  let confirming = $state<string | null>(null);

  async function revoke(node: TrustedNode): Promise<void> {
    if (confirming !== node.node_id) {
      confirming = node.node_id;
      return;
    }
    confirming = null;
    try {
      await pairingApi.revoke(node.node_id);
      toasts.show('success', `Revoked ${node.name}. The rest of the mesh drops it too.`);
      onchange();
    } catch (error) {
      toasts.show('error', error instanceof TransferError ? error.message : 'Could not revoke that node.');
    }
  }

  function standing(node: TrustedNode): string {
    if (node.revoked) return `Revoked by ${node.revoked_by ?? 'this device'}`;
    return node.connected ? 'Connected' : 'Not connected';
  }
</script>

<section class="card trust">
  <h2><Icon name="shield" size={18} /> Trusted nodes</h2>
  {#if nodes.length === 0}
    <p class="muted small">No other node yet. Nodes are trusted when they join with the passphrase or pair with this one.</p>
  {:else}
    <ul>
      {#each nodes as node (node.node_id)}
        <li class:revoked={node.revoked}>
          <span class="device"><Icon name="laptop" size={20} /></span>
          <div class="who">
            <div class="truncate"><strong>{node.name}</strong></div>
            <div class="small faint">
              <span class="badge {node.trusted_via === 'pairing' ? 'badge-success' : 'badge-accent'}">
                {node.trusted_via === 'pairing' ? 'Paired' : 'Same passphrase'}
              </span>
              {standing(node)}
            </div>
          </div>
          {#if !node.revoked}
            <button class="btn btn-small" class:danger={confirming === node.node_id} onclick={() => revoke(node)}>
              <Icon name="x" size={15} /> {confirming === node.node_id ? 'Revoke for the whole mesh?' : 'Revoke'}
            </button>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
</section>

<style>
  .trust {
    padding: 20px;
  }

  h2 {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  ul {
    margin: 12px 0 0;
    padding: 0;
    list-style: none;
  }

  li {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 12px 0;
  }

  li + li {
    border-top: 1px solid var(--border);
  }

  li.revoked {
    opacity: 0.6;
  }

  .who {
    flex: 1;
    min-width: 0;
  }

  .device {
    display: grid;
    place-items: center;
    width: 36px;
    height: 36px;
    border-radius: var(--radius-sm);
    background: var(--surface-low);
    color: var(--text-muted);
  }

  .danger {
    color: var(--danger);
  }
</style>
