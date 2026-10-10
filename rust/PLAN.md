# Rust port: remaining plan

Baseline (done, `3d87b25a` on `Zak/rust-port`): 17 data-driven games, Steam detection, zip/7z/rar, FOMod XML, hardlink deploy with conflicts + backup, plugins.txt, profiles, Tauri UI (vanilla JS).

## Decisions
- **Cross-platform is the point**: Windows, Linux (native + Proton), macOS. No Windows-only assumptions in core.
- **Nexus API**: implement both. GraphQL (v2) is the default; a setting switches to REST (v1). Where GraphQL lacks an endpoint (e.g. download links), fall back to REST transparently.
- **Start fresh**: no import of existing C# NMM installs.
- **UI framework**: light, not React/Next. Svelte 5 + Vite (or Solid, settled in the first UI issue).
- **Mod authoring**: keep it; port it.
- **Order**: Nexus comes before plugins.

Every phase ships on its own, keeps `cargo test` + clippy green, and adds one test per non-trivial path.

## Phase 1: Core hardening & UI foundation
Everything later builds on this.

### Crash-safe deploy journal
Write an intent journal (`deploy.journal`) before applying link changes. On `Manager::open`, replay or roll back an unfinished journal. Replaces the per-phase state save noted in `manager.rs`. Test: kill mid-deploy (simulated), reopen, state matches the filesystem.

### Game-root installs
Port `ModInstallRoot { Data, GameRoot }` (`ModManager.Interface/ModManagement/ModInstallRoot.cs`). Archive content outside the stop folders and Data dir (e.g. `skse64_loader.exe`, `*.dll`) deploys to the game root when `supportsGameRootInstall`. Unblocks SKSE/F4SE/OBSE.

### Path adjustment profiles
Port the `modInstall.pathAdjustmentProfile` behaviors (fallout4, cyberpunk2077, nomanssky, sims4, starfield, subnautica, stardewvalley, oblivionremastered) from `NexusClient/Games/DataDriven`.

### Case-insensitive deploy on case-sensitive filesystems
On Linux/macOS (case-sensitive volumes), mods ship `Textures/` vs `textures/`. Deploy must fold into the existing directory casing in the game folder so the game (expecting Windows semantics) sees one tree. Test on a case-sensitive tempdir.

### Link strategy: hardlink, symlink, copy, cross-volume
Detect when the mods dir and game dir are on different volumes, and fall back cleanly (port the `MultiHDMode`/`NMMLink` intent). Add a symlink tier where the platform allows it (Linux/macOS always; Windows with dev mode). Add a per-game link-mode setting.

### Background tasks with progress + cancel
Long operations (extract, install, deploy, download) run on worker threads, emit Tauri events (`progress`, `done`, `error`), and support cancellation. Remove the long-held global command lock.

### Errors, logging, settings
`thiserror` error type in core, a `tracing` log file in the app data dir, and an app settings file (mods dir defaults, link mode, Nexus API mode, close-on-launch).

### Frontend framework migration
Move `app/ui` from vanilla JS to Svelte 5 + Vite (decide Svelte vs Solid here). Use `app/ui` as the Vite root, with `beforeDevCommand`/`beforeBuildCommand` in `tauri.conf.json`. Keep feature parity with the current UI. Typed invoke wrappers for every command.

## Phase 2: Nexus integration

### Nexus API abstraction (GraphQL default, REST setting)
A `NexusApi` trait in core with `GraphqlApi` (v2, default) and `RestApi` (v1). A setting chooses one; unsupported GraphQL operations delegate to REST. Covers: mod info, file list, file info by md5, download links, update checks, endorse, validate key. Use `reqwest` with rustls. Respect the rate-limit headers.

### Authentication: SSO + manual API key
Port the SSO websocket flow (`NexusClient/SSO/SsoManager.cs`, `wss://sso.nexusmods.com`) and manual key entry. Store the key in the OS keychain (`keyring` crate: Windows Credential Manager, macOS Keychain, Secret Service on Linux), never in config files.

### nxm:// protocol handler + single instance
Register `nxm://` on all three OSes (`tauri-plugin-deep-link`), and route links to the running instance (`tauri-plugin-single-instance`). Parse links per `NexusClient/ModManagement/NexusUrl.cs` / `ModRepositories/NexusModLinkParser.cs`. Map Nexus game domains to mode ids.

### Download manager
Queue, concurrent limit, pause/resume via HTTP Range, retry with backoff, md5 verification, survives restarts. Port the intent of `DownloadManagement` + `DownloadMonitoring`. Downloads tab in the UI; a finished download offers install.

### Mod metadata, categories, update checks
Store Nexus mod id / file id / version / author / category on installed mods (matching by archive md5 where unknown). Categories from `Categories.xml` + API. Periodic update check with an "update available" badge (`ModUpdateCheckTask`, `AutoUpdater`).

## Phase 3: Plugins (Gamebryo/Creation games)

### Plugin load-order tab
Full plugin list independent of mod order: drag reorder, active toggle, filter. Persists through the game's strategy.

### Plugin header parsing (esplugin)
Use the `esplugin` crate for masters, ESL/light/medium flags, and descriptions. Warn on missing/misordered masters. Count Full/Light address spaces per `plugin.policy.addressSpaces`; enforce `maxAllowedActivePluginsCount`.

### Load-order persistence strategies
`singleFile` plugins.txt with an active marker (SSE/FO4/Starfield), plugins.txt + timestamp order (Oblivion/FO3/NV/Skyrim LE), `.ccc` official/Creation Club plugins, `orderedCriticalPluginNames` pinned on top, `officialPluginsAreImplicitlyActive`. Must resolve correctly inside Proton prefixes.

### LOOT auto-sort
Integrate the `libloot` crate (replaces `AutoPluginSortingTask`), with masterlist download/update and a sort button + preview diff.

### Archive invalidation / BSA handling
Port whatever is still needed from `GamebryoBase/Tools/BSA` for archive invalidation on older games (Oblivion/FO3/NV).

## Phase 4: Game coverage & cross-platform detection

### game.json for remaining C#-only game modes
Write definitions for the 22 modes that exist only as C#: BaldursGate3, BreakingWheel, DarkSouls, DarkSouls2, DragonAge, DragonAge2, DragonsDogma, Enderal, EnderalSE, Fallout4VR, HogwartsLegacy, MonsterHunterWorld, Morrowind, MountAndBlade2Bannerlord, SkyrimVR, Starbound, StateOfDecay, TESO, WarThunder, Witcher2, Witcher3, WorldOfTanks, XRebirth. Reconcile the C# `Sims4`/`StardewValley` modes with the existing `TheSims4`/`stardewvalley` defs. Port each `*GameModeDescriptor.cs`/`*GameMode.cs` into data, adding schema fields only when a game needs one. Can be split per game family.

### Store detection: Windows
Registry (`winreg`) Steam path + uninstall keys, GOG Galaxy, Epic `.item` manifests, Xbox/MS Store (`gamebryo`-style `discovery.stores`).

### Store detection: Linux + Proton
Native Steam + Flatpak Steam library folders, Heroic (GOG/Epic), Lutris. For Windows games under Proton, resolve the compat prefix (`steamapps/compatdata/<appid>/pfx`) so `{LocalApplicationData}`, `{PersonalData}`, `{UserGameData}` and plugins.txt point inside the prefix.

### Store detection: macOS
Native mac builds (Steam on macOS), plus CrossOver/Whisky/Wine bottles for Windows games: locate the bottle and map the Windows user dirs inside it.

### Shared storage, base files, tools, script extenders
`storage.shareModsStorageWith` (SkyrimSE <-> SkyrimGOG), `baseFilesPath` purge protection, `supportedTools.json` tool discovery + launcher (LOOT, xEdit, BodySlide, FNIS/Nemesis/Pandora), script-extender launch, custom launch command. Launching under Proton/Wine runs through the right runner.

## Phase 5: Installer completeness

### Mod upgrade / reinstall
Replace a mod with a newer archive while keeping its load-order position, profile membership, and FOMod choices (offer replay). Port `ModUpgrader`/`ModFileUpgradeInstaller`.

### INI edits
Port `IniInstaller`/`IniUpgradeInstaller`/`IniEdits.xml`: installers can edit game INIs; edits are recorded and undone on uninstall. Cross-platform INI paths (Proton prefix).

### FOMod gaps
`Inactive` file state (installed but plugin inactive), game/script-extender/NMM version conditions, `fomod/info.xml` metadata (name, author, version, Nexus id), screenshots, and a "remember choices" replay.

### ModScript installer (Oblivion/Morrowind)
Port the OBMM ModScript language (`Script Types/ModScript`) with a hand-written parser in place of the ANTLR grammar.

### OMod format
Port `Mod Formats/OMod` (Oblivion .omod archives, including the compressed data/plugin streams).

### Readme manager
Extract and index readmes on install (`ReadMeManager`), and show them in the details pane.

### Unsupported installer messaging
C# script installers (`fomod/script.cs`) stay unsupported. Detect them and show a clear message, offering a manual/plain install.

## Phase 6: UI parity

### Mod list: categories, search, sort, bulk actions
Grouping by category, text filter, sortable columns, multi-select enable/disable/uninstall, install date, Nexus version/update badges.

### Mod details pane + per-file conflicts
Description, Nexus info, file tree, readme, and a per-file conflict view (which mod wins each file; jump to the other mod).

### Settings, tools, and activity log
Settings page (paths, link mode, Nexus API mode GraphQL/REST, auth, theme), Tools menu, and an activity/log panel for background tasks.

### Accessibility & keyboard pass
Full keyboard operation, focus management in dialogs, screen-reader labels, contrast in light/dark themes.

## Phase 7: Mod authoring

### Port mod packaging tool
Port `NexusClient/ModAuthoring`: build mod archives from a folder, author FOMod `ModuleConfig.xml`/`info.xml`, and validate them against the `XmlScript5.0.xsd` semantics. Lives in its own UI section.

## Phase 8: Release

### Cross-platform bundling + signing
`cargo tauri build` for Windows (NSIS/MSI), macOS (dmg, notarized), and Linux (AppImage/deb/rpm, Flatpak later). Code signing on Windows and macOS.

### Auto-updater
`tauri-plugin-updater` with signed update manifests, replacing `NexusClient/Updating`.

### CI
GitHub Actions matrix (windows/ubuntu/macos): test, clippy, fmt, frontend build, and bundle artifacts. Replaces `appveyor.yml`.

### Retire the C# tree
Once parity is confirmed, remove the C# solution and move the Rust workspace to the repo root.

## Not porting
C# script installers, Tips management, DockPanelSuite/WinForms UI, `GamebryoBase/Tools/{AI,Shader,TESsnip,SharpZipLib}` beyond what Phase 3 needs, and import of C# NMM data.
