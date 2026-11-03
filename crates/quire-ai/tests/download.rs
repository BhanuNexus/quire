// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The downloader against a loopback server that speaks `Range` (design
//! §6.2 WS-D acceptance): a download resumes after a forced disconnect,
//! a wrong hash is rejected (after one retry), Pause keeps the part and
//! Cancel deletes it, a mirror keeps the path, and "Install from file…"
//! matches files by hash.
#![cfg(feature = "client")]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use quire_ai::catalog::{Catalog, CatalogEntry, Purpose};
use quire_ai::download::{Control, DownloadError, Downloader, Installed1, Manifest, install_from_files, partial_bytes, sha256_file};

#[derive(Clone, Copy)]
enum Behaviour {
    /// Serve the file (honouring Range).
    Ok,
    /// Send this many bytes of the body, then drop the connection.
    DropAfter(usize),
    /// Serve different bytes of the same length.
    Corrupt,
    /// Send slowly, 4 KB every 50 ms.
    Slow,
    /// Ignore Range: always 200 and the whole file.
    NoRange,
}

/// Serves `body` at any path, one behaviour per request; sends each
/// request's path and Range header back.
#[expect(clippy::unwrap_used, reason = "a test helper: any failure should fail the test")]
fn server(body: Vec<u8>, plan: Vec<Behaviour>) -> (String, mpsc::Receiver<(String, Option<String>)>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("http://{}", l.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for (stream, b) in l.incoming().zip(plan) {
            let mut s = stream.unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let path = line.split_whitespace().nth(1).unwrap_or_default().to_string();
            let mut range = None;
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h == "\r\n" || h.is_empty() {
                    break;
                }
                if let Some(v) = h.to_ascii_lowercase().strip_prefix("range:") {
                    range = Some(v.trim().to_string());
                }
            }
            tx.send((path, range.clone())).unwrap();
            let data: Vec<u8> = match b {
                Behaviour::Corrupt => body.iter().map(|x| x ^ 0xff).collect(),
                _ => body.clone(),
            };
            let start = match (b, &range) {
                (Behaviour::NoRange, _) | (_, None) => None,
                (_, Some(r)) => r.trim_start_matches("bytes=").trim_end_matches('-').parse::<usize>().ok(),
            };
            let (status, slice) = match start {
                Some(st) if st >= data.len() => {
                    let _ = write!(s, "HTTP/1.1 416 Range Not Satisfiable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
                    continue;
                }
                Some(st) => ("206 Partial Content", &data[st..]),
                None => ("200 OK", &data[..]),
            };
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                slice.len()
            );
            match b {
                Behaviour::DropAfter(n) => {
                    let _ = s.write_all(&slice[..n.min(slice.len())]);
                    let _ = s.flush();
                    drop(s);
                }
                Behaviour::Slow => {
                    for chunk in slice.chunks(4096) {
                        if s.write_all(chunk).is_err() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                _ => {
                    let _ = s.write_all(slice);
                }
            }
        }
    });
    (addr, rx)
}

#[expect(clippy::unwrap_used, reason = "a test helper: any failure should fail the test")]
fn sha(bytes: &[u8]) -> String {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x");
    std::fs::write(&p, bytes).unwrap();
    sha256_file(&p).unwrap()
}

#[expect(clippy::unwrap_used, reason = "a test helper: any failure should fail the test")]
fn entry(id: &str, url: &str, body: &[u8]) -> CatalogEntry {
    let mut c = Catalog::parse(&format!(
        "format = 1\n[[model]]\nid = \"{id}\"\npurpose = \"embed\"\nlabel = \"Test\"\nlicence = \"MIT\"\nram_mb = 1\n[[model.files]]\nname = \"model.bin\"\nurl = \"{url}\"\nsha256 = \"{}\"\nsize = {}\n",
        sha(body),
        body.len()
    ))
    .unwrap();
    c.models.remove(0)
}

#[expect(clippy::unwrap_used, reason = "a test helper: any failure should fail the test")]
fn data(n: usize) -> Vec<u8> {
    (0..n).map(|i| u8::try_from(i * 7 % 251).unwrap()).collect()
}

fn installed(models: &Path, e: &CatalogEntry) -> bool {
    Manifest::load(models).is_installed(models, e)
}

#[test]
fn resumes_with_range_after_a_dropped_connection() {
    let body = data(300_000);
    let (addr, seen) = server(body.clone(), vec![Behaviour::DropAfter(120_000), Behaviour::Ok]);
    let e = entry("m", &format!("{addr}/org/model/resolve/abc/model.bin"), &body);
    let models = tempfile::tempdir().unwrap();
    let d = Downloader::new("");
    let mut last = 0;
    let r = d.fetch(&e, models.path(), &Control::new(), &mut |p| {
        if let quire_ai::download::Progress::Bytes { done, .. } = p {
            last = done;
        }
    });
    assert!(matches!(r, Err(DownloadError::Http(ref m)) if m.contains("kept")), "{r:?}");
    assert_eq!(partial_bytes(models.path(), &e), 120_000, "what arrived is kept");
    assert!(!installed(models.path(), &e));
    d.fetch(&e, models.path(), &Control::new(), &mut |p| {
        if let quire_ai::download::Progress::Bytes { done, .. } = p {
            last = done;
        }
    })
    .unwrap();
    assert_eq!(last, 300_000);
    assert_eq!(seen.recv().unwrap().1, None);
    assert_eq!(seen.recv().unwrap().1.as_deref(), Some("bytes=120000-"), "the second request resumes");
    assert!(installed(models.path(), &e));
    assert_eq!(std::fs::read(models.path().join("m/model.bin")).unwrap(), body);
    assert!(!models.path().join("m/model.bin.part").exists());
    // Already installed: nothing is fetched again (the server has no more answers).
    d.fetch(&e, models.path(), &Control::new(), &mut |_| {}).unwrap();
}

#[test]
fn a_server_that_ignores_range_restarts_cleanly() {
    let body = data(100_000);
    let (addr, _seen) = server(body.clone(), vec![Behaviour::DropAfter(30_000), Behaviour::NoRange]);
    let e = entry("m", &format!("{addr}/model.bin"), &body);
    let models = tempfile::tempdir().unwrap();
    let d = Downloader::new("");
    assert!(d.fetch(&e, models.path(), &Control::new(), &mut |_| {}).is_err());
    d.fetch(&e, models.path(), &Control::new(), &mut |_| {}).unwrap();
    assert_eq!(std::fs::read(models.path().join("m/model.bin")).unwrap(), body);
}

#[test]
fn a_wrong_hash_is_retried_once_then_rejected() {
    let body = data(50_000);
    let (addr, seen) = server(body.clone(), vec![Behaviour::Corrupt, Behaviour::Corrupt]);
    let e = entry("m", &format!("{addr}/model.bin"), &body);
    let models = tempfile::tempdir().unwrap();
    let r = Downloader::new("").fetch(&e, models.path(), &Control::new(), &mut |_| {});
    assert_eq!(r, Err(DownloadError::Checksum { file: "model.bin".into() }));
    assert_eq!(seen.try_iter().count(), 2, "one retry");
    assert!(!models.path().join("m/model.bin").exists() && !models.path().join("m/model.bin.part").exists());
    assert!(!installed(models.path(), &e));
}

#[test]
fn pause_keeps_the_part_and_cancel_deletes_it() {
    let body = data(400_000);
    let (addr, _seen) = server(body.clone(), vec![Behaviour::Slow, Behaviour::Slow]);
    let e = entry("m", &format!("{addr}/model.bin"), &body);
    let models = tempfile::tempdir().unwrap();
    let d = Downloader::new("");
    let ctl = Control::new();
    let r = d.fetch(&e, models.path(), &ctl, &mut |p| {
        if matches!(p, quire_ai::download::Progress::Bytes { done, .. } if done > 20_000) {
            ctl.pause();
        }
    });
    assert_eq!(r, Err(DownloadError::Paused));
    assert!(partial_bytes(models.path(), &e) > 20_000);
    let ctl = Control::new();
    let started = std::time::Instant::now();
    let r = d.fetch(&e, models.path(), &ctl, &mut |p| {
        if matches!(p, quire_ai::download::Progress::Bytes { done, .. } if done > 60_000) {
            ctl.cancel();
        }
    });
    assert_eq!(r, Err(DownloadError::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(5), "cancel is prompt");
    assert_eq!(partial_bytes(models.path(), &e), 0, "cancel deletes what arrived");
}

#[test]
fn mirrors_keep_path_and_hash_and_unpinned_models_never_download() {
    let body = data(10_000);
    let (mirror, seen) = server(body.clone(), vec![Behaviour::Ok]);
    let e = entry("m", "https://huggingface.co/org/repo/resolve/abc/model.bin", &body);
    let models = tempfile::tempdir().unwrap();
    Downloader::new(&format!("{mirror}/")).fetch(&e, models.path(), &Control::new(), &mut |_| {}).unwrap();
    assert_eq!(seen.recv().unwrap().0, "/org/repo/resolve/abc/model.bin");
    let mut unpinned = e;
    unpinned.files[0].sha256.clear();
    assert_eq!(Downloader::new("").fetch(&unpinned, models.path(), &Control::new(), &mut |_| {}), Err(DownloadError::NotPinned));
}

#[test]
fn install_from_file_matches_by_hash() {
    let body = data(20_000);
    let e = entry("bge", "https://example.org/model.bin", &body);
    let catalog = Catalog { format: 1, models: vec![e.clone()] };
    let tmp = tempfile::tempdir().unwrap();
    let good = tmp.path().join("downloaded.bin");
    let other = tmp.path().join("other.bin");
    std::fs::write(&good, &body).unwrap();
    std::fs::write(&other, b"nope").unwrap();
    let models = tmp.path().join("models");
    let r = install_from_files(&catalog, &[good, other.clone()], &models);
    assert_eq!(r[0], Installed1::Ok { id: "bge".into(), file: "model.bin".into() });
    assert_eq!(r[1], Installed1::Unknown { path: other });
    assert!(installed(&models, &e));
    quire_ai::download::delete(&models, "bge").unwrap();
    assert!(!installed(&models, &e) && !models.join("bge").exists());
}

#[test]
fn the_engine_is_unpacked_and_checked_against_its_binary_hash() {
    let binary = b"#!/bin/sh\necho engine\n".to_vec();
    let gz_bytes = gzip(&binary);
    let (addr, _seen) = server(gz_bytes.clone(), vec![Behaviour::Ok, Behaviour::Ok]);
    let mut e = quire_ai::catalog::engine_entry("0.9.0", "test-target", &format!("{} {} {}", sha(&gz_bytes), gz_bytes.len(), sha(&binary)));
    e.files[0].url = format!("{addr}/quire-ai-engine-test-target.gz");
    assert_eq!(e.purpose, Purpose::Engine);
    let models = tempfile::tempdir().unwrap();
    Downloader::new("").fetch(&e, models.path(), &Control::new(), &mut |_| {}).unwrap();
    let bin = models.path().join("engine").join(quire_ai::engine::ENGINE_BIN);
    assert_eq!(std::fs::read(&bin).unwrap(), binary);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&bin).unwrap().permissions().mode() & 0o111, 0o111, "executable");
    }
    // A .gz whose content isn't the pinned binary is refused.
    let models2 = tempfile::tempdir().unwrap();
    let mut bad = e.clone();
    bad.other.insert(quire_ai::catalog::BINARY_SHA256.into(), "0".repeat(64).into());
    let r = Downloader::new("").fetch(&bad, models2.path(), &Control::new(), &mut |_| {});
    assert!(matches!(r, Err(DownloadError::Checksum { .. })), "{r:?}");
    assert!(!models2.path().join("engine").join(quire_ai::engine::ENGINE_BIN).exists());
}

/// The release asset is gzip (flate2 is already a dependency of quire-ai).
#[expect(clippy::unwrap_used, reason = "a test helper: any failure should fail the test")]
fn gzip(data: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}
