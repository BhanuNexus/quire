// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end encrypted sync for Quire vaults.
//!
//! * [`crypto`]: vault keys, sealing, passphrase wrapping, recovery keys
//! * [`protocol`]: revisions and what's inside them
//! * [`transport`]: how the engine reaches a server, and an in-memory one
//! * [`engine`]: pull, merge three-way, push; never drops text
//! * [`keyring`]: key epochs and device keys, for removing a device (0.13)
//! * [`compact`]: snapshots that fold old revisions away (0.13)
//! * [`folder`]: sync through a folder Dropbox, iCloud Drive or similar already syncs
//! * [`names`]: which note names this computer can store (0.13.2)
//! * `http` (feature `http`): a transport for `quire-sync-server`
//! * `remote` (feature `remote`): S3-compatible storage and WebDAV, the
//!   folder transport's rules over object storage
//!
//! The design is in `docs/design/sync.md`.

#![cfg_attr(test, allow(clippy::disallowed_methods, reason = "tests build fixtures with std directly"))]

pub mod admin;
pub mod compact;
pub mod crypto;
pub mod engine;
pub mod folder;
#[cfg(feature = "http")]
pub mod http;
pub mod keyring;
pub mod names;
pub mod protocol;
#[cfg(feature = "remote")]
pub mod remote;
pub mod transport;

pub use crypto::{CryptoError, KdfParams, Keys, RecoveryKey, VaultKey, WrappedKey};
pub use engine::{
    DELETE_WINDOW_MS, DeleteMark, Engine, LocalStore, MANY_DELETES, MANY_SETTINGS_DELETES, MAX_DELETE_MARKS, MemoryStore, Report, Skipped,
    SyncError, SyncState, Tracked, Version,
};
pub use folder::{FolderSpaceInfo, FolderTransport, min_reader, raise_min_reader};
#[cfg(feature = "http")]
pub use http::{HttpTransport, Negotiated, SpaceInfo};
pub use keyring::{DeviceRecord, DeviceSecret, EpochRecord, KeySet, Pins};
pub use protocol::{ActorRef, Content, DeviceId, FileId, LanePos, PROTOCOL, PushOutcome, RevId, Revision, Sealing, ServerInfo, SpaceId};
pub use transport::{MemoryServer, Transport, TransportError};
