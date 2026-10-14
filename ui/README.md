# LADEX web UI

Svelte 5 + TypeScript + Vite. One build, two pages:

- `index.html`: the node's own page (device list, catalog, upload, download, chat, nicknames, guests).
- `join.html`: the mobile guest page (join with an invitation, browse, download, upload).

Both talk to the node through the `LadexClient` interface in `src/client/types.ts`.

```sh
npm install
npm run dev      # proxies to a node on http://localhost:8081 (override with LADEX_NODE)
npm run check    # svelte-check
npm test         # vitest
npm run build    # writes dist/
```

Start a node with `ladex --no-keychain --port 8080` and its plain-HTTP local listener is
`http://localhost:8081`.
