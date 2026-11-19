// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Quire Sync for an open vault: setup, a background worker, and status.
//!
//! The worker runs the [`quire_sync::Engine`] against the vault folder. It
//! writes notes like any other program would, so the editor's existing
//! outside-change handling (merge into unsaved edits, or reload) applies.
//!
//! A vault syncs through a folder that Dropbox, iCloud Drive and the like
//! already sync (free, nothing to run), through a Quire Sync server, or
//! (0.12) straight to S3-compatible storage or a WebDAV folder.
//!
//! Per vault, `.quire/sync/config.json` holds where it syncs and its ids, and
//! `.quire/sync/state.d/` the engine's state (`journal`; 0.13 kept it in
//! `state.json`). The vault key (and a server's
//! token, or the storage's credentials) live in the OS keychain (or the
//! private secrets file), never in the vault.
//!
//! The code is split by part, each a child module that re-exports its items
//! here, so paths such as `vault_sync::SyncConfig` do not change:
//! `config` (what a setup is), `state` (the lock and the engine's files),
//! `journal` (the engine state on disk: checkpoint, journal, base packs),
//! `store` (the vault as the engine sees it), `session` (one pass),
//! `worker` (the background thread), `deletes` (held mass deletes), `setup`
//! (create, join, move and stop), `workspace` (the `impl Workspace` methods)
//! and `harden` (devices, key trust, compaction, upgrades).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
/// What one sync pass did.
pub use quire_sync::Report as SyncReport;
pub use quire_sync::SpaceInfo;
/// One version of a note from the sync log, for the history dialog.
pub use quire_sync::Version as SyncVersion;
use quire_sync::folder::{self, FolderTransport};
pub use quire_sync::protocol::RevId as SyncRevId;
use quire_sync::remote::{self, ObjectStore, RemoteTransport};
use quire_sync::{
    DeviceId, Engine, HttpTransport, KdfParams, LanePos, LocalStore, RecoveryKey, Report, SpaceId, SyncState, Transport, VaultKey,
    WrappedKey,
};
use quire_vault::{NotePath, Vault};
use serde::{Deserialize, Serialize};

use crate::secrets;
use crate::workspace::{NoticeLevel, Workspace};

mod harden;
pub use harden::{
    DeviceRemoved, Hardening, SyncDevice, SyncDevices, UpgradeInfo, sync_compact_at, sync_devices_at, sync_remove_device_at,
    sync_trust_key_at, sync_upgrade_at, sync_upgrade_info_at,
};
use quire_sync::admin::SpaceAdmin;
use quire_sync::keyring::{DeviceSecret, KeySet, Pins};

mod config;
pub use config::*;
mod state;
pub use state::*;
mod journal;
mod store;
pub use store::*;
mod session;
pub use session::*;
mod worker;
pub use worker::*;
mod deletes;
pub use deletes::*;
mod setup;
pub use setup::*;
mod workspace;

#[cfg(test)]
mod tests;
