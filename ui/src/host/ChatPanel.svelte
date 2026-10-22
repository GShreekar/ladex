<script lang="ts">
  import { tick } from 'svelte';
  import { formatAgo } from '../client/format';
  import Icon from '../components/Icon.svelte';
  import { clock } from '../state/clock.svelte';
  import { useApp } from '../state/context';

  const { room } = useApp();

  let draft = $state('');
  let list: HTMLElement;

  $effect(() => {
    void room.messages.length;
    void tick().then(() => list?.scrollTo({ top: list.scrollHeight }));
  });

  function send(event: SubmitEvent): void {
    event.preventDefault();
    if (!draft.trim()) return;
    room.client.sendMessage(draft);
    draft = '';
  }

  async function copy(text: string): Promise<void> {
    await navigator.clipboard?.writeText(text).catch(() => undefined);
  }
</script>

<section class="card panel">
  <div class="head">
    <h2><Icon name="chat" size={18} /> Room chat</h2>
    <span class="small faint">Visible to the room</span>
  </div>

  <ol bind:this={list}>
    {#each room.messages as message (message.id)}
      <li class:own={message.isOwn}>
        <div class="meta small">
          <span class="muted">{message.senderName}</span>
          <span class="faint">{formatAgo(message.sentAt, clock.now)}</span>
        </div>
        <button class="bubble" title="Copy this message" onclick={() => copy(message.text)}>{message.text}</button>
      </li>
    {:else}
      <li class="empty faint small">No messages yet. Say hello, or paste a link.</li>
    {/each}
  </ol>

  <form onsubmit={send}>
    <input class="input" placeholder="Message the room…" maxlength="2000" bind:value={draft} />
    <button class="btn btn-primary btn-icon" aria-label="Send" disabled={!draft.trim()}><Icon name="send" /></button>
  </form>
</section>

<style>
  .panel {
    padding: 16px;
  }

  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 12px;
  }

  h2 {
    display: flex;
    align-items: center;
    gap: 8px;
    color: var(--accent-text);
  }

  ol {
    display: flex;
    flex-direction: column;
    gap: 10px;
    max-height: 280px;
    margin: 0 0 12px;
    padding: 0;
    overflow-y: auto;
    list-style: none;
  }

  .meta {
    display: flex;
    justify-content: space-between;
    margin-bottom: 2px;
  }

  .bubble {
    display: block;
    width: 100%;
    padding: 8px 12px;
    border: 0;
    border-radius: var(--radius-sm);
    background: var(--surface-low);
    text-align: left;
    overflow-wrap: anywhere;
    white-space: pre-wrap;
  }

  .own .bubble {
    background: var(--accent-tint);
  }

  .empty {
    padding: 12px 0;
    text-align: center;
  }

  form {
    display: flex;
    gap: 8px;
  }
</style>
