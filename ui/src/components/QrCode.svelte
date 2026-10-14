<script lang="ts">
  import qrcode from 'qrcode-generator';

  let { text, label = 'QR code' }: { text: string; label?: string } = $props();

  const svg = $derived.by(() => {
    const code = qrcode(0, 'M');
    code.addData(text);
    code.make();
    return code.createSvgTag({ cellSize: 4, margin: 0, scalable: true });
  });
</script>

<div class="qr" role="img" aria-label={label}>{@html svg}</div>

<style>
  .qr {
    padding: 12px;
    border-radius: var(--radius-md);
    background: #ffffff;
    line-height: 0;
  }

  .qr :global(svg) {
    width: 100%;
    height: auto;
    display: block;
  }
</style>
