// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end encrypted sync for Quire vaults.
//!
//! * [`crypto`]: vault keys, sealing, passphrase wrapping, recovery keys
//! * [`protocol`]: revisions and what's inside them
//! * [`transport`]: how the engine reaches a server, and an in-memory one
//! * [`engine`]: pull, merge three-way, push; never drops text
//! * `http` (feature `http`): a transport for `quire-sync-server`
//!
//! The design is in `docs/design/sync.md`.

pub mod crypto;
pub mod engine;
#[cfg(feature = "http")]
pub mod http;
pub mod protocol;
pub mod transport;

pub use crypto::{CryptoError, KdfParams, Keys, RecoveryKey, VaultKey, WrappedKey};
pub use engine::{Engine, LocalStore, MemoryStore, Report, SyncError, SyncState, Tracked};
#[cfg(feature = "http")]
pub use http::{HttpTransport, SpaceInfo};
pub use protocol::{Content, DeviceId, FileId, PushOutcome, RevId, Revision, SpaceId};
pub use transport::{MemoryServer, Transport, TransportError};
