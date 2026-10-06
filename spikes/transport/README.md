# Transport spike (ADR 0001)

Throwaway code behind [`docs/adr/0001-transport.md`](../../docs/adr/0001-transport.md): the same 1 → 1
transfer of one large file, built once on plain `quinn` and once on `iroh` + `iroh-blobs`.

It is a standalone crate (its own `[workspace]`), so `cargo test --workspace` and CI never build it.

| | quinn | iroh |
|---|---|---|
| Transport | quinn 0.11, rustls/ring, TLS 1.3 | iroh 1.3 (noq), relay, address lookup and portmapper all off |
| Identity | self-signed Ed25519 certs, receiver pins the sender's cert, sender requires a client cert (mTLS) | Ed25519 endpoint ids via RFC 7250 raw public keys |
| Protocol | NEXT_PHASE §5.2 cut down: `GetManifest`, then `GetChunks{start,count}` on 16 parallel streams | iroh-blobs `GET` of one BLAKE3 hash |
| Integrity | BLAKE3 per 1 MiB chunk, checked before the positional write | BLAKE3/bao, checked per 16 KiB group |
| Storage | plain file, `set_len` up front, then `fsync` | `FsStore` (redb); import and export by reference, then `fsync` |

## Run it

```sh
# Both transfers alternate on one machine; every output file is hash-checked.
spikes/transport/bench.sh /path/with/9GB/free 5 4GiB
```

On two machines, start the sender on one and paste its ticket into the receiver on the other:

```sh
transport-spike gen big.bin --size 4GiB
transport-spike quinn-send big.bin --listen 192.168.1.10:0          # prints: ticket: ...
transport-spike quinn-recv '<ticket>' out.bin --listen 192.168.1.20:0

transport-spike iroh-send big.bin ./send-store --listen 192.168.1.10:0
transport-spike iroh-recv '<ticket>' out.bin ./recv-store --listen 192.168.1.20:0

transport-spike hash out.bin   # compare with: transport-spike hash big.bin
```

Each process prints one JSON line with its phase timings, CPU time and peak RSS. Use the receiver's
`transfer_mib_per_sec`; the sender's clock starts at accept, so it includes the handshake.
