//! What a vault's sync setup is: `SyncConfig` and its profiles, secrets,
//! selection and targets, the keychain names, and the folder checks.

use super::{
    Arc, Deserialize, DeviceId, Instant, KdfParams, ObjectStore, PARTIAL_DEVICE, Path, PathBuf, REMOTE_DEVICE, Serialize, SpaceId,
    VaultKey, folder, remote, secrets, sync_list,
};

/// What's stored in `.quire/sync/config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncConfig {
    /// The server's address; empty when syncing through a folder.
    #[serde(default)]
    pub server: String,
    /// The `Quire Sync` folder inside a synced folder, when syncing that way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    pub space: SpaceId,
    pub device: DeviceId,
    pub device_name: String,
    /// Also sync `.quire/shared/` (vault settings, agent policy, review
    /// logs). On a server, turning it on raises the space's minimum so 0.8
    /// clients stop. In a sync folder it travels in a lane of
    /// its own that 0.8 never reads, so 0.8 devices keep syncing notes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shared_state: bool,
    /// Selective sync: only these folders (`Work/`); empty means all.
    /// Notes outside the selection are never written here, and never sent
    /// as deleted (see [`Selection`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub only: Vec<String>,
    /// Selective sync: never these folders.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// 0.12: S3-compatible storage or a WebDAV folder, when syncing that way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteProfile>,
}

/// Where in S3-compatible storage or WebDAV a vault syncs (no secrets: the
/// credentials are in the keychain, see [`RemoteSecret`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum RemoteProfile {
    S3 {
        endpoint: String,
        region: String,
        bucket: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        prefix: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        virtual_hosted: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        allow_http: bool,
    },
    Webdav {
        url: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        allow_http: bool,
    },
}

/// The storage's credentials, as kept in the keychain (JSON).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum RemoteSecret {
    S3 {
        access_key: String,
        secret_key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_token: Option<String>,
    },
    Webdav {
        user: String,
        password: String,
    },
}

impl std::fmt::Debug for RemoteSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::S3 { access_key, .. } => f.debug_struct("S3").field("access_key", access_key).finish_non_exhaustive(),
            Self::Webdav { user, .. } => f.debug_struct("Webdav").field("user", user).finish_non_exhaustive(),
        }
    }
}

/// `url` without a `user:password@` part, for status lines. (Such an
/// address is refused when sync is set up; this covers a hand-edited
/// `config.json`.)
fn without_userinfo(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..end].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => url.to_string(),
    }
}

impl RemoteProfile {
    /// "S3 bucket notes at https://…", "WebDAV folder https://…".
    pub fn describe(&self) -> String {
        match self {
            Self::S3 { endpoint, bucket, prefix, .. } => {
                let p = if prefix.trim_matches('/').is_empty() { String::new() } else { format!("/{}", prefix.trim_matches('/')) };
                format!("S3 bucket {bucket}{p} at {endpoint}")
            }
            Self::Webdav { url, .. } => format!("WebDAV folder {}", without_userinfo(url)),
        }
    }

    /// A client for the storage, checking the address and credentials' shape.
    pub fn open(&self, secret: &RemoteSecret) -> Result<Arc<dyn ObjectStore>, String> {
        use quire_sync::remote::Zeroizing;
        match (self, secret) {
            (
                Self::S3 { endpoint, region, bucket, prefix, virtual_hosted, allow_http },
                RemoteSecret::S3 { access_key, secret_key, session_token },
            ) => {
                let cfg = remote::S3Config {
                    endpoint: endpoint.clone(),
                    region: region.clone(),
                    bucket: bucket.clone(),
                    prefix: prefix.clone(),
                    virtual_hosted: *virtual_hosted,
                    allow_http: *allow_http,
                };
                let creds = remote::S3Credentials {
                    access_key: access_key.trim().to_string(),
                    secret_key: Zeroizing::new(secret_key.trim().to_string()),
                    session_token: session_token.as_ref().filter(|t| !t.trim().is_empty()).map(|t| Zeroizing::new(t.trim().to_string())),
                };
                Ok(Arc::new(remote::S3Store::new(cfg, creds).map_err(|e| e.to_string())?))
            }
            (Self::Webdav { url, allow_http }, RemoteSecret::Webdav { user, password }) => {
                let cfg = remote::WebDavConfig { url: url.clone(), allow_http: *allow_http };
                let creds = remote::WebDavCredentials { user: user.trim().to_string(), password: Zeroizing::new(password.clone()) };
                Ok(Arc::new(remote::WebDavStore::new(&cfg, &creds).map_err(|e| e.to_string())?))
            }
            _ => Err("The saved credentials are for another kind of storage; enter them again.".into()),
        }
    }
}

impl RemoteSecret {
    pub(super) fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub(super) fn from_json(s: &str) -> Result<Self, String> {
        serde_json::from_str(s).map_err(|_| "The storage credentials on this device are unreadable; enter them again.".to_string())
    }
}

/// Which folders a device syncs (`quire-cli sync --only/--exclude`), so a
/// server can hold just the folders it needs.
///
/// A note outside the selection is never written, read or sent from this
/// device: its absence here is never taken for a delete. A note moved out
/// of the selection on another device leaves this one (to its trash); one
/// moved into an unselected folder here is sent as a rename. Widening the
/// selection fetches the notes it now includes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    /// Folder prefixes with a trailing `/`; empty: every folder.
    pub only: Vec<String>,
    pub exclude: Vec<String>,
}

impl Selection {
    /// Folder names as typed (`Work`, `Work/`, `./Work`, `Projects/2026`).
    /// Names are compared exactly, case included.
    pub fn new(only: &[String], exclude: &[String]) -> Result<Self, String> {
        let norm = |f: &String| -> Result<String, String> {
            let t = f.trim().trim_start_matches("./").trim_matches('/');
            if !quire_base::path::Rule::VaultFolder.accepts(t) {
                return Err(format!("“{f}” isn't a folder in the vault (a relative path, not hidden, without `..`)"));
            }
            Ok(format!("{t}/"))
        };
        let list = |v: &[String]| -> Result<Vec<String>, String> {
            let mut out = v.iter().map(norm).collect::<Result<Vec<_>, _>>()?;
            out.sort();
            out.dedup();
            Ok(out)
        };
        Ok(Self { only: list(only)?, exclude: list(exclude)? })
    }

    pub const fn is_everything(&self) -> bool {
        self.only.is_empty() && self.exclude.is_empty()
    }

    /// Whether this device syncs `path`. Vault settings (`.quire/shared/`)
    /// follow their own switch, not the selection.
    pub fn includes(&self, path: &str) -> bool {
        if path.starts_with(SHARED_PREFIX) {
            return true;
        }
        (self.only.is_empty() || self.only.iter().any(|o| path.starts_with(o.as_str())))
            && !self.exclude.iter().any(|e| path.starts_with(e.as_str()))
    }

    /// "only Work/, Projects/; not Work/Old/", or "everything".
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.only.is_empty() {
            parts.push(format!("only {}", self.only.join(", ")));
        }
        if !self.exclude.is_empty() {
            parts.push(format!("not {}", self.exclude.join(", ")));
        }
        if parts.is_empty() { "everything".into() } else { parts.join("; ") }
    }
}

/// The vault-settings folder that travels with Quire Sync when allowed.
pub const SHARED_PREFIX: &str = ".quire/shared/";
/// The largest file synced from it.
pub(super) const MAX_SHARED_BYTES: u64 = 10 * 1024 * 1024;
/// Review history shared between devices (0.10), inside the settings.
pub(super) const SHARED_REVIEW_PREFIX: &str = ".quire/shared/agent/";

/// Shown in the status bar and Settings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SyncStatus {
    /// Sync isn't set up for this vault.
    #[default]
    Off,
    Syncing,
    /// Up to date as of this moment.
    Synced(Instant),
    /// Can't reach the server or the sync folder; will keep trying.
    Offline(String),
    /// Synced, but the sync folder has files that haven't downloaded yet.
    Waiting(String),
    /// Something needs the user (wrong token, missing key…).
    Error(String),
    /// Another Quire process syncs this vault on this device (the app,
    /// `quire-cli serve --sync` or `sync watch`); this one waits and takes
    /// over when it stops. Says so in words.
    Elsewhere(String),
}

impl SyncStatus {
    pub fn label(&self) -> String {
        match self {
            Self::Off => "Sync off".into(),
            Self::Syncing => "Syncing…".into(),
            Self::Synced(_) => "Synced".into(),
            Self::Offline(_) => "Offline".into(),
            Self::Waiting(_) => "Waiting for files".into(),
            Self::Error(_) => "Sync needs attention".into(),
            Self::Elsewhere(_) => "Synced by another Quire".into(),
        }
    }
}

pub(super) fn sync_dir(root: &Path) -> PathBuf {
    root.join(".quire").join("sync")
}

/// How hard a passphrase is stretched (cheap in tests).
pub(super) const KDF: KdfParams = if cfg!(test) { KdfParams::TEST } else { KdfParams::DEFAULT };

pub(super) fn token_name(space: &SpaceId) -> String {
    format!("sync-token-{space}")
}

pub(super) fn key_name(space: &SpaceId) -> String {
    format!("sync-key-{space}")
}

/// The vault key in the keychain and its key epoch: the recovery key, or
/// (0.13, after a device was removed) `e<epoch>:<recovery key>`.
pub(super) fn parse_key(s: &str) -> Option<(u32, VaultKey)> {
    let (epoch, rk) = match s.strip_prefix('e').and_then(|r| r.split_once(':')) {
        Some((e, rk)) => (e.parse().ok()?, rk),
        None => (0, s),
    };
    VaultKey::from_recovery_key(rk).ok().map(|k| (epoch, k))
}

/// The newest vault key this device has for `space`.
pub(super) fn read_key(space: &SpaceId) -> Option<VaultKey> {
    secrets::store().get(&key_name(space)).and_then(|k| parse_key(&k)).map(|(_, k)| k)
}

/// Where a vault syncs, as chosen in Settings.
#[derive(Clone, PartialEq, Eq)]
pub enum SyncTarget {
    /// A folder another tool keeps the same on every device (the one the
    /// user picked; Quire works in a `Quire Sync` folder inside it).
    Folder(PathBuf),
    Server {
        url: String,
        token: String,
    },
    /// 0.12: S3-compatible storage or a WebDAV folder.
    Remote {
        profile: RemoteProfile,
        secret: RemoteSecret,
    },
}

/// Without the server token (the storage secret hides itself).
impl std::fmt::Debug for SyncTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Folder(p) => f.debug_tuple("Folder").field(p).finish(),
            Self::Server { url, .. } => f.debug_struct("Server").field("url", url).finish_non_exhaustive(),
            Self::Remote { profile, secret } => f.debug_struct("Remote").field("profile", profile).field("secret", secret).finish(),
        }
    }
}

impl SyncTarget {
    pub(super) fn store(&self) -> Result<Option<Arc<dyn ObjectStore>>, String> {
        match self {
            Self::Remote { profile, secret } => profile.open(secret).map(Some),
            _ => Ok(None),
        }
    }
}

/// Whether an S3 endpoint wants `bucket.host` URLs (AWS) rather than paths.
pub fn s3_virtual_hosted_default(endpoint: &str, bucket: &str) -> bool {
    remote::S3Config::default_virtual_hosted(endpoint, bucket)
}

/// The place a vault syncs with, secrets included (for "Test connection"
/// on a vault that's set up).
pub fn sync_target_of(cfg: &SyncConfig) -> Result<SyncTarget, String> {
    let token = secrets::store().get(&token_name(&cfg.space));
    match (&cfg.remote, &cfg.folder) {
        (Some(profile), _) => Ok(SyncTarget::Remote {
            profile: profile.clone(),
            secret: RemoteSecret::from_json(&token.ok_or("This device is missing the storage credentials.")?)?,
        }),
        (None, Some(dir)) => Ok(SyncTarget::Folder(dir.clone())),
        (None, None) => Ok(SyncTarget::Server { url: cfg.server.clone(), token: token.ok_or("This device is missing the sync token.")? }),
    }
}

/// "Test connection": can Quire list, write, read and delete there, and
/// does the storage support conditional writes. For S3 and WebDAV; folders
/// and servers say whether they're reachable.
pub fn sync_test(target: &SyncTarget) -> Result<String, String> {
    match target {
        SyncTarget::Remote { profile, secret } => {
            let store = profile.open(secret)?;
            remote::probe(&*store).map(|p| p.summary()).map_err(|e| e.to_string())
        }
        SyncTarget::Folder(_) | SyncTarget::Server { .. } => {
            let n = sync_list(target)?.len();
            Ok(format!("Connected: {n} Quire vault(s) there."))
        }
    }
}

/// A vault that a new device can join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncVaultInfo {
    pub space: SpaceId,
    /// Unix seconds.
    pub created: i64,
    /// "3 devices", "120 changes".
    pub detail: String,
}

/// Folders that a sync tool on this computer keeps in step, for one-click setup.
pub fn suggested_sync_folders() -> Vec<(String, PathBuf)> {
    let Some(home) = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()) else { return Vec::new() };
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let mut add = |label: &str, p: PathBuf| {
        if p.is_dir() && !out.iter().any(|(l, q)| l == label || *q == p) {
            out.push((label.to_string(), p));
        }
    };
    add("iCloud Drive", home.join("Library/Mobile Documents/com~apple~CloudDocs"));
    // macOS puts Dropbox, Google Drive and OneDrive under CloudStorage.
    if let Ok(entries) = std::fs::read_dir(home.join("Library/CloudStorage")) {
        let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if name.starts_with("Dropbox") {
                add("Dropbox", p);
            } else if name.starts_with("GoogleDrive") {
                add("Google Drive", p.join("My Drive"));
            } else if name.starts_with("OneDrive") {
                add("OneDrive", p);
            }
        }
    }
    add("Dropbox", home.join("Dropbox"));
    add("iCloud Drive", home.join("iCloudDrive"));
    add("OneDrive", home.join("OneDrive"));
    add("Google Drive", home.join("Google Drive"));
    add("Syncthing", home.join("Sync"));
    out
}

/// `p` with symlinks resolved (macOS's `/var` is `/private/var`), even when
/// its last parts don't exist yet: the deepest existing folder is resolved
/// and the rest appended, so two paths compare the same way.
fn canonical(p: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(c) = std::fs::canonicalize(cur) {
            return rest.iter().rev().fold(c, |acc, part| acc.join(part));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                cur = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// The `Quire Sync` folder for a picked folder, refusing ones that would
/// sync the vault into itself.
pub(super) fn sync_folder_root(chosen: &Path, vault_root: &Path) -> Result<PathBuf, String> {
    if !chosen.is_dir() {
        return Err(format!("{} isn't a folder on this computer", chosen.display()));
    }
    let root = folder::sync_root(chosen);
    let (r, v) = (canonical(&root), canonical(vault_root));
    if r.starts_with(&v) {
        return Err("Pick a folder outside this notes folder".into());
    }
    if v.starts_with(&r) {
        return Err("This notes folder is inside that sync folder; pick another one".into());
    }
    Ok(root)
}

impl SyncConfig {
    /// Where this vault syncs, in words ("folder …", "server …", "S3 bucket …").
    pub fn transport_label(&self) -> String {
        match (&self.remote, &self.folder) {
            (Some(r), _) => r.describe(),
            (None, Some(f)) => format!("folder {}", f.display()),
            (None, None) => format!("server {}", self.server),
        }
    }

    /// The folders this device syncs.
    pub fn selection(&self) -> Selection {
        Selection::new(&self.only, &self.exclude).unwrap_or_else(|_| Selection {
            // Written by hand and unreadable: hold nothing rather than guess.
            only: vec!["\u{0}/".into()],
            exclude: Vec::new(),
        })
    }

    pub fn load(root: &Path) -> Option<Self> {
        let s = std::fs::read_to_string(sync_dir(root).join("config.json")).ok()?;
        let mut v: serde_json::Value = serde_json::from_str(&s).ok()?;
        for alias in [PARTIAL_DEVICE, REMOTE_DEVICE] {
            if let Some(o) = v.as_object_mut()
                && !o.contains_key("device")
                && let Some(d) = o.remove(alias)
            {
                o.insert("device".into(), d);
            }
        }
        serde_json::from_value(v).ok()
    }

    pub(super) fn save(&self, root: &Path) -> Result<(), String> {
        let mut v = serde_json::to_value(self).map_err(|e| e.to_string())?;
        if !self.selection().is_everything()
            && let Some(o) = v.as_object_mut()
            && let Some(d) = o.remove("device")
        {
            // Quire 0.9 can't read this file then, so it shows sync as off
            // for this vault rather than sync it from an empty state.
            o.insert(PARTIAL_DEVICE.into(), d);
        }
        if self.remote.is_some()
            && let Some(o) = v.as_object_mut()
            && let Some(d) = o.remove("device").or_else(|| o.remove(PARTIAL_DEVICE))
        {
            // Quire 0.11 and older can't read this file then (they'd take
            // it for a server with no address), so they show sync as off.
            o.insert(REMOTE_DEVICE.into(), d);
        }
        let text = serde_json::to_string_pretty(&v).map_err(|e| e.to_string())?;
        crate::atomic::write(&sync_dir(root).join("config.json"), text.as_bytes(), false).map_err(|e| e.to_string())
    }
}
