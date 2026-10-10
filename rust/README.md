# NMM (Rust port)

Rust + Tauri port of the core of Nexus Mod Manager.

- `core/`: game definitions (embedded from `../Game Modes/Definitions/*/game.json`), Steam detection, zip/7z/rar extraction, FOMod XML installer, virtual install (hardlink deploy, conflict resolution by load order, backup/restore of overwritten game files), plugins.txt, profiles.
- `app/`: Tauri 2 desktop app; UI in `app/ui` (Svelte 5 + TypeScript + Vite, typed command wrappers in `src/lib/api.ts`).

```sh
cargo test                          # no Node needed
cd app/ui && npm install
npm run tauri dev                   # Vite dev server + app, hot reload
npm run check                       # svelte-check (types)
npm run tauri build                 # bundle
```

Plain `cargo run -p nmm-app` loads the UI from the dev server (`npm run dev` must be running).

Remaining work: `PLAN.md` and the GitHub issues it links.
