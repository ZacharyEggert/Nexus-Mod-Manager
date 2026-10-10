# NMM (Rust port)

Rust + Tauri port of the core of Nexus Mod Manager.

- `core/`: game definitions (embedded from `../Game Modes/Definitions/*/game.json`), Steam detection, zip/7z/rar extraction, FOMod XML installer, virtual install (hardlink deploy, conflict resolution by load order, backup/restore of overwritten game files), plugins.txt, profiles.
- `app/`: Tauri 2 desktop UI (plain HTML/JS in `app/ui`, so no npm needed).

```sh
cargo test -p nmm-core
cargo run -p nmm-app
# bundle: cargo install tauri-cli && cd app && cargo tauri build
```

Not ported yet: Nexus API/downloads (nxm links), C# and ModScript installers, OMod, the C#-only game modes, per-game path adjustments, game-root installs (SKSE etc.), GOG/Epic detection, plugin load-order UI, importing an existing NMM VirtualModConfig.xml.
