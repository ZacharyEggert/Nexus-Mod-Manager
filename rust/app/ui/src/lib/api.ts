// Typed wrappers for every Tauri command in app/src/main.rs, and the types they exchange.
// ponytail: hand-mirrored from the Rust structs; generate them (ts-rs / tauri-specta) if they drift.

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

export type LinkMode = 'auto' | 'hardlink' | 'symlink' | 'copy';
export type NexusApi = 'graphql' | 'rest';

export interface GameInfo {
  mode_id: string;
  name: string;
  game_path: string | null;
  mods_dir: string | null;
  /** Custom launch command (e.g. SMAPI); null = default_launch. */
  launch: string | null;
  default_launch: string | null;
  detected: string | null;
}

export interface ModEntry {
  id: string;
  name: string;
  archive: string;
  files: string[];
  /** Omitted when empty. */
  root_files?: string[];
  /** Omitted when false. */
  game_root?: boolean;
  active: boolean;
  /** Omitted until known (download, or md5 match). */
  nexus?: NexusMeta;
}

export interface NexusMeta {
  domain: string;
  mod_id: number;
  file_id: number;
  mod_name: string;
  file_name: string;
  version: string;
  author: string;
  category_id: number | null;
  /** Newer file in the update chain, from the last check. */
  update?: { file_id: number; name: string; version: string };
}

export interface CheckReport {
  identified: number;
  checked: number;
  /** Names of mods with an update available. */
  updates: string[];
  errors: string[];
}

export interface Library {
  mods: ModEntry[];
  /** Mod id -> ids of mods it shares files with. */
  conflicts: Record<string, string[]>;
  profiles: string[];
  install_root: string;
  link_mode: LinkMode;
  same_volume: boolean;
  in_sync: boolean;
  /** Stored mod folders missing from the list; `adopt` re-adds them. */
  strays: string[];
  /** Nexus category id -> name. */
  categories: Record<string, string>;
  /** Unix seconds of the last Nexus update check. */
  nexus_checked: number | null;
}

export interface RepairReport {
  /** Stale backups set aside, relative to the mods folder. */
  orphaned_backups: string[];
  /** Game paths holding an unrecorded copy of a mod file (or a game file with the same bytes). */
  untracked: string[];
}

export interface Settings {
  mods_root: string | null;
  link_mode: LinkMode;
  nexus_api: NexusApi;
  close_on_launch: boolean;
  max_downloads: number;
}

export interface SettingsView extends Settings {
  log_dir: string | null;
}

export interface NexusUser {
  user_id: number;
  name: string;
  premium: boolean;
  supporter: boolean;
}

export interface Account {
  user: NexusUser | null;
  /** Browser login available in this build. */
  sso: boolean;
  /** A key is stored but couldn't be checked. */
  error: string | null;
}

export interface NxmKey {
  key: string;
  expires: number;
}

export interface NexusLink {
  domain: string;
  mod_id: number;
  file_id: number | null;
  key: NxmKey | null;
  user_id: number | null;
  source: string;
}

export interface LinkEvent {
  link: NexusLink;
  /** Game modes the link's domain belongs to. */
  modes: string[];
}

export type DownloadState = 'queued' | 'running' | 'paused' | 'done' | 'failed';

export interface Download {
  id: string;
  mode: string;
  domain: string;
  mod_id: number;
  file_id: number;
  mod_name: string;
  name: string;
  version: string;
  path: string;
  /** Bytes; 0 = unknown. */
  size: number;
  key: NxmKey | null;
  state: DownloadState;
  done: number;
  /** Failure reason, or a retry note while running. */
  error: string | null;
  /** md5 matched Nexus; null = couldn't check. */
  verified: boolean | null;
}

export type GroupKind = 'SelectAtLeastOne' | 'SelectAtMostOne' | 'SelectExactlyOne' | 'SelectAll' | 'SelectAny';
export type PluginType = 'Required' | 'Optional' | 'Recommended' | 'NotUsable' | 'CouldBeUsable';

export interface FomodPlugin {
  name: string;
  description: string;
  image: string | null;
}

export interface FomodGroup {
  name: string;
  kind: GroupKind;
  plugins: FomodPlugin[];
}

export interface ModuleConfig {
  name: string;
  steps: { name: string; groups: FomodGroup[] }[];
}

/** (step, group, plugin) indices of checked options. */
export type Selection = [number, number, number][];

export interface Eval {
  visible: boolean[];
  /** Per step / group / plugin. */
  types: PluginType[][][];
}

export interface StageResult {
  /** Set when the archive is a FOMod: pass to the fomod_* commands. */
  token: string | null;
  fomod: ModuleConfig | null;
}

export type Op =
  | { op: 'set_active'; id: string; active: boolean }
  | { op: 'set_game_root'; id: string; on: boolean }
  | { op: 'set_link_mode'; mode: LinkMode }
  | { op: 'remove'; id: string }
  | { op: 'reorder'; id: string; delta: number }
  | { op: 'save_profile'; name: string }
  | { op: 'load_profile'; name: string }
  | { op: 'delete_profile'; name: string }
  | { op: 'redeploy' };

// ---------- plain commands ----------

export const games = () => invoke<GameInfo[]>('games');
export const configure = (mode: string, gamePath: string, modsDir: string | null, launch: string | null) =>
  invoke<void>('configure', { mode, gamePath, modsDir, launch });
export const library = (mode: string) => invoke<Library>('library', { mode });
export const pickArchive = () => invoke<string | null>('pick_archive');
export const pickFolder = () => invoke<string | null>('pick_folder');
export const fomodEval = (token: string, sel: Selection) => invoke<Eval>('fomod_eval', { token, sel });
export const fomodImage = (token: string, path: string) => invoke<ArrayBuffer>('fomod_image', { token, path });
export const fomodCancel = (token: string) => invoke<void>('fomod_cancel', { token });
export const launch = (mode: string) => invoke<void>('launch', { mode });
export const settings = () => invoke<SettingsView>('settings');
export const saveSettings = (settings: Settings) => invoke<void>('save_settings', { settings });
export const nexusAccount = () => invoke<Account>('nexus_account');
export const nexusSetKey = (key: string) => invoke<NexusUser>('nexus_set_key', { key });
export const nexusLogout = () => invoke<void>('nexus_logout');
export const openNexus = (domain: string, modId: number, fileId: number | null = null) =>
  invoke<void>('open_nexus', { domain, modId, fileId });
/** A background update check changed a game's library. */
export const onLibraryChanged = (fn: (mode: string) => void) => listen<string>('library-changed', (e) => fn(e.payload));
export const takeLinks = () => invoke<LinkEvent[]>('take_links');
export const downloads = () => invoke<Download[]>('downloads');
export const downloadAdd = (mode: string, url: string) => invoke<string>('download_add', { mode, url });
export const downloadPause = (id: string) => invoke<void>('download_pause', { id });
export const downloadResume = (id: string) => invoke<void>('download_resume', { id });
export const downloadRemove = (id: string, deleteFile: boolean) => invoke<void>('download_remove', { id, deleteFile });
/** A download changed state or made progress. */
export const onDownload = (fn: (d: Download) => void) => listen<Download>('download', (e) => fn(e.payload));

// ---------- background tasks ----------

export interface Progress {
  id: string;
  step: string;
  done: number;
  /** 0 when unknown. */
  total: number;
}

interface Outcome<T> {
  id: string;
  result?: T;
  error?: string;
}

/** A task command plus its args; `T` is what it resolves with. Run it with `start`. */
export interface TaskCall<T> {
  cmd: string;
  args: Record<string, unknown>;
  /** Type-only. */
  _result?: T;
}
const call = <T>(cmd: string, args: Record<string, unknown>): TaskCall<T> => ({ cmd, args });

/** `download`: id of the finished download the archive came from (records its Nexus origin). */
export const stage = (mode: string, archive: string, download: string | null = null) =>
  call<StageResult>('stage', { mode, archive, download });
export const nexusCheck = (mode: string) => call<CheckReport>('nexus_check', { mode });
export const fomodInstall = (token: string, sel: Selection) => call<void>('fomod_install', { token, sel });
export const apply = (mode: string, op: Op) => call<void>('apply', { mode, op });
export const repair = (mode: string) => call<RepairReport>('repair', { mode });
export const adopt = (mode: string) => call<RepairReport>('adopt', { mode });
export const nexusLogin = () => call<NexusUser>('nexus_login', {});

/** What a task rejects with when cancelled. */
export const CANCELLED = 'cancelled';

type Handlers = { progress: (p: Progress) => void; done: (o: Outcome<unknown>) => void; error: (o: Outcome<unknown>) => void };
const pending = new Map<string, Handlers>();
for (const ev of ['progress', 'done', 'error'] as const) {
  listen<Progress & Outcome<unknown>>(ev, ({ payload }) => pending.get(payload.id)?.[ev](payload));
}
let seq = 0;

/** Start a backend task under an id we pick, so we're listening before it can finish.
 *  `done` rejects with the error text, or `CANCELLED`. */
export function start<T>(c: TaskCall<T>, onProgress: (p: Progress) => void): { id: string; done: Promise<T> } {
  const id = `t${++seq}`;
  const done = new Promise<T>((resolve, reject) => {
    const end = <A>(fn: (a: A) => void) => (a: A) => {
      pending.delete(id);
      fn(a);
    };
    pending.set(id, {
      progress: onProgress,
      done: end((o) => resolve(o.result as T)),
      error: end((o) => reject(o.error)),
    });
    invoke(c.cmd, { ...c.args, id }).catch(end(reject));
  });
  return { id, done };
}

export const cancel = (id: string) => invoke<void>('cancel', { id });

/** Fired when Nexus links arrive; fetch them with `takeLinks`. */
export const onLinks = (fn: () => void) => listen('nxm', fn);

/** Fired when a quit is waiting for running tasks to roll back. */
export const onQuitting = (fn: () => void) => listen('quitting', fn);
