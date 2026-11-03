// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Downloading models (design §D.2).
//!
//! One file at a time into `<models>/<id>/<file>.part`, hashed while it streams, resumed with
//! `Range` after a pause or a dropped connection, checked against the
//! catalog's SHA-256, synced and renamed into place. A mirror keeps the path
//! and the hash. "Install from file…" copies a file someone downloaded
//! elsewhere, after the same check.
//!
//! `<models>/installed.toml` records what was verified and when
//! (docs/formats/model-catalog.md). Nothing here starts on its own: the app
//! calls [`Downloader::fetch`] when the person presses Download.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::catalog::{BINARY_SHA256, Catalog, CatalogEntry, CatalogFile, ENGINE_ID, Purpose};
use crate::engine::ENGINE_BIN;

/// Pause, resume or cancel a download from another thread.
#[derive(Debug, Clone, Default)]
pub struct Control(Arc<AtomicU8>);

const PAUSE: u8 = 1;
const CANCEL: u8 = 2;

impl Control {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stop, keeping what arrived so far (Download resumes it).
    pub fn pause(&self) {
        self.0.store(PAUSE, Ordering::Release);
    }

    /// Stop and delete what arrived.
    pub fn cancel(&self) {
        self.0.store(CANCEL, Ordering::Release);
    }

    fn state(&self) -> u8 {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    /// Bytes of the whole model so far, its total, and the recent rate.
    Bytes { done: u64, total: u64, bytes_per_s: f64 },
    /// Checking the last file.
    Verifying,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadError {
    /// The catalog has no hash for a file, so it can't be checked.
    NotPinned,
    Paused,
    Cancelled,
    /// The file arrived twice and was wrong both times.
    Checksum {
        file: String,
    },
    Http(String),
    Io(String),
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPinned => f.write_str("this model isn't available in this build of Quire"),
            Self::Paused => f.write_str("paused"),
            Self::Cancelled => f.write_str("cancelled"),
            Self::Checksum { file } => write!(f, "checksum mismatch for {file}: the download was damaged or changed"),
            Self::Http(e) => write!(f, "download failed: {e}"),
            Self::Io(e) => write!(f, "couldn't save the download: {e}"),
        }
    }
}

impl std::error::Error for DownloadError {}

impl From<std::io::Error> for DownloadError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

// ------------------------------------------------------------------ manifest

pub const MANIFEST_FORMAT: u32 = 1;

/// `<models>/installed.toml`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub format: u32,
    #[serde(default)]
    pub models: BTreeMap<String, Installed>,
    #[serde(flatten)]
    pub other: toml::Table,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Installed {
    /// File name → SHA-256 it was verified against.
    pub files: BTreeMap<String, String>,
    /// Unix ms of the last check.
    pub verified: i64,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Manifest {
    pub fn path(models: &Path) -> PathBuf {
        models.join("installed.toml")
    }

    pub fn load(models: &Path) -> Self {
        std::fs::read_to_string(Self::path(models)).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
    }

    /// Written whole to a temporary file and renamed. A manifest from a newer
    /// Quire is left alone.
    pub fn save(&mut self, models: &Path) -> std::io::Result<()> {
        if self.format > MANIFEST_FORMAT {
            return Ok(());
        }
        self.format = MANIFEST_FORMAT;
        let text = toml::to_string(self).map_err(std::io::Error::other)?;
        std::fs::create_dir_all(models)?;
        let tmp = models.join("installed.toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, Self::path(models))
    }

    /// Every file of `entry` is on disk and was verified against the hash
    /// this build pins.
    pub fn is_installed(&self, models: &Path, entry: &CatalogEntry) -> bool {
        let Some(m) = self.models.get(&entry.id) else { return false };
        entry
            .files
            .iter()
            .all(|f| m.files.get(&f.name).is_some_and(|h| f.is_pinned() && *h == f.sha256) && installed_path(models, entry, f).is_file())
    }
}

/// Where a catalog file lives once installed. The engine is unpacked to
/// `<models>/engine/quire-ai-engine`.
pub fn installed_path(models: &Path, entry: &CatalogEntry, file: &CatalogFile) -> PathBuf {
    if entry.purpose == Purpose::Engine { models.join(ENGINE_ID).join(ENGINE_BIN) } else { models.join(&entry.id).join(&file.name) }
}

/// Lowercase hex SHA-256 of a file.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// `url` with its scheme and host replaced by `mirror`'s (`""`: unchanged).
pub fn mirrored(url: &str, mirror: &str) -> String {
    let mirror = mirror.trim().trim_end_matches('/');
    if mirror.is_empty() {
        return url.to_string();
    }
    let path = url.split_once("://").and_then(|(_, rest)| rest.find('/').map(|i| &rest[i..])).unwrap_or("/");
    format!("{mirror}{path}")
}

// ------------------------------------------------------------------ downloader

pub struct Downloader {
    agent: ureq::Agent,
    mirror: String,
}

impl Downloader {
    pub fn new(mirror: &str) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(20)))
            .timeout_recv_response(Some(Duration::from_mins(1)))
            .timeout_per_read(Some(Duration::from_secs(45)))
            .http_status_as_error(false)
            .user_agent(concat!("Quire/", env!("CARGO_PKG_VERSION"), " (+models)"))
            .tls_config(ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::PlatformVerifier).build())
            .build()
            .into();
        Self { agent, mirror: mirror.to_string() }
    }

    /// Download every file of `entry` that isn't installed yet, then record
    /// it in the manifest. Resumes `.part` files left by a pause or a
    /// dropped connection.
    pub fn fetch(
        &self,
        entry: &CatalogEntry,
        models: &Path,
        ctl: &Control,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<(), DownloadError> {
        if !entry.is_pinned() {
            return Err(DownloadError::NotPinned);
        }
        let mut manifest = Manifest::load(models);
        let total = entry.size();
        let mut base = 0;
        let mut meter = Meter::new();
        for f in &entry.files {
            let done_already = manifest.models.get(&entry.id).and_then(|m| m.files.get(&f.name)).is_some_and(|h| *h == f.sha256)
                && installed_path(models, entry, f).is_file();
            if !done_already {
                self.fetch_file(entry, f, models, ctl, &mut |n| {
                    let rate = meter.add(base + n);
                    progress(Progress::Bytes { done: base + n, total, bytes_per_s: rate });
                })?;
                progress(Progress::Verifying);
                let installed = manifest.models.entry(entry.id.clone()).or_default();
                installed.files.insert(f.name.clone(), f.sha256.clone());
                installed.verified = now_ms();
                manifest.save(models)?;
            }
            base += f.size;
            progress(Progress::Bytes { done: base, total, bytes_per_s: meter.rate });
        }
        Ok(())
    }

    fn fetch_file(
        &self,
        entry: &CatalogEntry,
        f: &CatalogFile,
        models: &Path,
        ctl: &Control,
        progress: &mut dyn FnMut(u64),
    ) -> Result<(), DownloadError> {
        let dir = models.join(&entry.id);
        std::fs::create_dir_all(&dir)?;
        let part = dir.join(format!("{}.part", f.name));
        for attempt in 0..2 {
            match self.stream_to(&part, f, ctl, progress) {
                Ok(()) => {}
                Err(DownloadError::Cancelled) => {
                    let _ = std::fs::remove_file(&part);
                    return Err(DownloadError::Cancelled);
                }
                Err(e) => return Err(e),
            }
            if sha256_file(&part)? == f.sha256 {
                return place(entry, f, &part, models);
            }
            // Wrong bytes (a bad mirror, a changed upstream file): start over once.
            let _ = std::fs::remove_file(&part);
            if attempt == 1 {
                return Err(DownloadError::Checksum { file: f.name.clone() });
            }
        }
        Err(DownloadError::Checksum { file: f.name.clone() })
    }

    /// Bring `part` up to `f.size` bytes, resuming when the server allows.
    fn stream_to(&self, part: &Path, f: &CatalogFile, ctl: &Control, progress: &mut dyn FnMut(u64)) -> Result<(), DownloadError> {
        let mut have = std::fs::metadata(part).map_or(0, |m| m.len());
        if have > f.size {
            std::fs::remove_file(part)?;
            have = 0;
        }
        if have == f.size {
            return Ok(());
        }
        let url = mirrored(&f.url, &self.mirror);
        let mut req = self.agent.get(&url);
        if have > 0 {
            req = req.header("range", &format!("bytes={have}-"));
        }
        let resp = req.call().map_err(|e| DownloadError::Http(e.to_string()))?;
        let status = resp.status().as_u16();
        let mut out = match status {
            206 => OpenOptions::new().append(true).open(part)?,
            200 => {
                have = 0;
                File::create(part)?
            }
            416 => {
                // The server thinks we have it all; the hash check decides.
                return Ok(());
            }
            _ => return Err(DownloadError::Http(format!("HTTP {status} from {}", host_of(&url)))),
        };
        progress(have);
        let mut body = resp.into_body().into_with_config().limit(f.size.saturating_sub(have) + 1).reader();
        let mut buf = vec![0; 1 << 16];
        loop {
            match ctl.state() {
                PAUSE => {
                    out.flush()?;
                    return Err(DownloadError::Paused);
                }
                CANCEL => return Err(DownloadError::Cancelled),
                _ => {}
            }
            let n = match body.read(&mut buf) {
                Ok(n) => n,
                Err(e) => {
                    // Keep what arrived; Download resumes from here.
                    out.flush()?;
                    return Err(DownloadError::Http(format!("the connection dropped ({e}); what arrived is kept")));
                }
            };
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            have += n as u64;
            progress(have);
        }
        out.sync_all()?;
        if have < f.size {
            return Err(DownloadError::Http("the connection dropped; what arrived is kept".into()));
        }
        Ok(())
    }
}

fn host_of(url: &str) -> &str {
    url.split_once("://").map_or(url, |(_, r)| r.split('/').next().unwrap_or(r))
}

/// Move a verified file into place (the engine is unpacked and made
/// executable, and checked again against its binary hash when one is pinned).
fn place(entry: &CatalogEntry, f: &CatalogFile, part: &Path, models: &Path) -> Result<(), DownloadError> {
    let dest = installed_path(models, entry, f);
    if let Some(d) = dest.parent() {
        std::fs::create_dir_all(d)?;
    }
    if entry.purpose == Purpose::Engine && Path::new(&f.name).extension().is_some_and(|e| e.eq_ignore_ascii_case("gz")) {
        let tmp = dest.with_extension("tmp");
        {
            let mut gz = flate2::read::GzDecoder::new(File::open(part)?);
            let mut out = File::create(&tmp)?;
            std::io::copy(&mut gz, &mut out)?;
            out.sync_all()?;
        }
        if let Some(want) = entry.other.get(BINARY_SHA256).and_then(toml::Value::as_str)
            && sha256_file(&tmp)? != want
        {
            let _ = std::fs::remove_file(&tmp);
            return Err(DownloadError::Checksum { file: ENGINE_BIN.into() });
        }
        make_executable(&tmp)?;
        std::fs::rename(&tmp, &dest)?;
        let _ = std::fs::remove_file(part);
    } else {
        File::open(part)?.sync_all()?;
        std::fs::rename(part, &dest)?;
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn make_executable(_p: &Path) -> std::io::Result<()> {
    Ok(())
}

/// A transfer rate over the last few seconds.
struct Meter {
    start: Instant,
    last: (Instant, u64),
    rate: f64,
}

impl Meter {
    fn new() -> Self {
        let now = Instant::now();
        Self { start: now, last: (now, 0), rate: 0.0 }
    }

    fn add(&mut self, done: u64) -> f64 {
        let now = Instant::now();
        let dt = now.duration_since(self.last.0).as_secs_f64();
        if dt >= 0.5 {
            let r = done.saturating_sub(self.last.1) as f64 / dt;
            // Smooth, but follow changes within a few seconds.
            self.rate =
                if now.duration_since(self.start).as_secs_f64() < 1.5 || self.rate == 0.0 { r } else { 0.6f64.mul_add(self.rate, 0.4 * r) };
            self.last = (now, done);
        }
        self.rate
    }
}

// ------------------------------------------------------------------ install from file, delete

/// What happened to one picked file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installed1 {
    /// It is file `file` of model `id`.
    Ok {
        id: String,
        file: String,
    },
    /// Its hash matches nothing in the catalog.
    Unknown {
        path: PathBuf,
    },
    Failed {
        path: PathBuf,
        error: String,
    },
}

/// Copy files someone downloaded elsewhere (an offline computer): each is
/// matched to a catalog file by its SHA-256, copied in and recorded.
pub fn install_from_files(catalog: &Catalog, paths: &[PathBuf], models: &Path) -> Vec<Installed1> {
    let mut manifest = Manifest::load(models);
    let mut out = Vec::new();
    for p in paths {
        let r = (|| -> Result<Option<(String, String)>, DownloadError> {
            let hash = sha256_file(p)?;
            let found = catalog.models.iter().find_map(|e| {
                e.files.iter().find(|f| f.is_pinned() && f.sha256 == hash).map(|f| (e, f)).or_else(|| {
                    // The unpacked engine binary is accepted too.
                    (e.purpose == Purpose::Engine && e.other.get(BINARY_SHA256).and_then(toml::Value::as_str) == Some(hash.as_str()))
                        .then(|| e.files.first().map(|f| (e, f)))
                        .flatten()
                })
            });
            let Some((entry, f)) = found else { return Ok(None) };
            let dir = models.join(&entry.id);
            std::fs::create_dir_all(&dir)?;
            if entry.purpose == Purpose::Engine && !hash.eq_ignore_ascii_case(&f.sha256) {
                let dest = installed_path(models, entry, f);
                if let Some(d) = dest.parent() {
                    std::fs::create_dir_all(d)?;
                }
                let tmp = dest.with_extension("tmp");
                std::fs::copy(p, &tmp)?;
                make_executable(&tmp)?;
                std::fs::rename(tmp, dest)?;
            } else {
                let part = dir.join(format!("{}.part", f.name));
                std::fs::copy(p, &part)?;
                place(entry, f, &part, models)?;
            }
            let m = manifest.models.entry(entry.id.clone()).or_default();
            m.files.insert(f.name.clone(), f.sha256.clone());
            m.verified = now_ms();
            Ok(Some((entry.id.clone(), f.name.clone())))
        })();
        out.push(match r {
            Ok(Some((id, file))) => Installed1::Ok { id, file },
            Ok(None) => Installed1::Unknown { path: p.clone() },
            Err(e) => Installed1::Failed { path: p.clone(), error: e.to_string() },
        });
    }
    let _ = manifest.save(models);
    out
}

/// Delete a model's files (and any partial download) and forget it.
pub fn delete(models: &Path, id: &str) -> std::io::Result<()> {
    let dir = models.join(id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    let mut m = Manifest::load(models);
    if m.models.remove(id).is_some() {
        m.save(models)?;
    }
    Ok(())
}

/// Bytes of a partial download of `entry`, for "Paused at 42%".
pub fn partial_bytes(models: &Path, entry: &CatalogEntry) -> u64 {
    let m = Manifest::load(models);
    entry
        .files
        .iter()
        .map(|f| {
            let done = m.models.get(&entry.id).is_some_and(|i| i.files.get(&f.name) == Some(&f.sha256));
            if done { f.size } else { std::fs::metadata(models.join(&entry.id).join(format!("{}.part", f.name))).map_or(0, |x| x.len()) }
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrors_keep_the_path() {
        let u = "https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/abc/model.safetensors";
        assert_eq!(mirrored(u, ""), u);
        assert_eq!(
            mirrored(u, "https://models.example.org/"),
            "https://models.example.org/BAAI/bge-small-en-v1.5/resolve/abc/model.safetensors"
        );
        assert_eq!(host_of(u), "huggingface.co");
    }

    #[test]
    fn manifest_round_trips_and_keeps_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            Manifest::path(dir.path()),
            "format = 1\nfuture = true\n[models.x]\nverified = 5\nnote = \"kept\"\n[models.x.files]\na = \"00\"\n",
        )
        .unwrap();
        let mut m = Manifest::load(dir.path());
        assert_eq!(m.models["x"].verified, 5);
        m.save(dir.path()).unwrap();
        let text = std::fs::read_to_string(Manifest::path(dir.path())).unwrap();
        assert!(text.contains("future = true") && text.contains("note = \"kept\""), "{text}");
        std::fs::write(Manifest::path(dir.path()), "format = 9\n").unwrap();
        let mut newer = Manifest::load(dir.path());
        newer.models.insert("y".into(), Installed::default());
        newer.save(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(Manifest::path(dir.path())).unwrap(), "format = 9\n", "a newer manifest is never rewritten");
    }
}
