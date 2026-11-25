// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The on-device AI pack (`docs/design/0.14-local-ai.md` §3.8): what the
//! "Turn on on-device AI…" dialog offers, what it costs in disk, and turning
//! it on and off.
//!
//! The pack is the writing model (Qwen 3.5 4B or 9B) and OCR, nothing else.
//! Its rows come from [`PACK_ROWS`], so a row can be removed in one line; a
//! row whose files this build hasn't pinned is not offered at all (if Qwen 3.5
//! fails its checks, only OCR is). Nothing downloads until the person presses
//! "Download and turn on", and then only pinned files.

use std::path::Path;

use quire_ai::catalog::{Catalog, ENGINE_ID};

use crate::settings::PackPick;
use crate::workspace::{NoticeLevel, Workspace};

/// What a row of the dialog turns on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackKind {
    /// The writing and agent model (one of these rows, a choice).
    Writing,
    /// Text in images and scanned PDFs (a tick box).
    Ocr,
}

/// One row of the pack dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackRow {
    pub id: &'static str,
    pub kind: PackKind,
    pub label: &'static str,
}

/// The pack's rows, in the dialog's order. 4B is the default everywhere.
pub const PACK_ROWS: &[PackRow] = &[
    PackRow { id: "qwen3.5-4b-q4km", kind: PackKind::Writing, label: "Writing and the agent: Qwen 3.5 4B" },
    PackRow { id: "qwen3.5-9b-q4km", kind: PackKind::Writing, label: "Writing and the agent: Qwen 3.5 9B" },
    PackRow { id: "ocrs", kind: PackKind::Ocr, label: "Text in images and scanned PDFs (Latin script)" },
];

/// A row as offered on this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offered {
    pub row: PackRow,
    /// Bytes to download (the pinned sizes of its files).
    pub size: u64,
    /// Why it can't be chosen here (greyed out with this text), if so.
    pub blocked: Option<String>,
    /// The model's memory in MB while writing (`ram_mb`).
    pub ram_mb: u32,
}

/// What the dialog offers on this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackOffer {
    pub rows: Vec<Offered>,
    /// The writing model ticked at first: 4B when it can be chosen.
    pub default_llm: Option<&'static str>,
    /// The engine's size when it must be downloaded too (Quire Lite).
    pub engine_size: Option<u64>,
}

/// What the person ticked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackChoice {
    pub llm: Option<String>,
    pub ocr: bool,
}

/// The offer for a computer with `memory_gb` of memory (`None`: unknown).
///
/// With or without the engine installed. Rows this build hasn't pinned are
/// left out; writing needs 8 GB, and a row its own `min_ram_gb`. With the
/// memory unknown only the 4B row is offered ("4B when in doubt").
pub fn offer(catalog: &Catalog, memory_gb: Option<u64>, engine_installed: bool) -> PackOffer {
    let mut rows = Vec::new();
    for row in PACK_ROWS {
        let Some(entry) = catalog.get(row.id).filter(|e| e.is_pinned()) else { continue };
        let blocked = match (row.kind, memory_gb) {
            (PackKind::Writing, Some(gb)) if gb < 8 => Some("needs a computer with 8 GB of memory".to_string()),
            (PackKind::Writing, Some(gb)) if gb < u64::from(entry.min_ram_gb) => Some(format!("needs {} GB", entry.min_ram_gb)),
            (PackKind::Writing, None) if entry.min_ram_gb > 8 => Some(format!("needs {} GB", entry.min_ram_gb)),
            _ => None,
        };
        rows.push(Offered { row: *row, size: entry.size(), blocked, ram_mb: entry.ram_mb });
    }
    let default_llm = rows.iter().find(|o| o.row.kind == PackKind::Writing && o.blocked.is_none()).map(|o| o.row.id);
    let engine_size = (!engine_installed).then(|| catalog.size_of(ENGINE_ID)).filter(|s| *s > 0);
    PackOffer { rows, default_llm, engine_size }
}

impl PackOffer {
    /// The first choice: the default writing model and OCR, when offered.
    pub fn default_choice(&self) -> PackChoice {
        PackChoice {
            llm: self.default_llm.map(str::to_string),
            ocr: self.rows.iter().any(|o| o.row.kind == PackKind::Ocr && o.blocked.is_none()),
        }
    }

    /// Bytes `choice` downloads: the ticked rows and, in Lite, the engine.
    pub fn download_bytes(&self, choice: &PackChoice) -> u64 {
        let rows: u64 = self
            .rows
            .iter()
            .filter(|o| o.blocked.is_none())
            .filter(|o| match o.row.kind {
                PackKind::Writing => choice.llm.as_deref() == Some(o.row.id),
                PackKind::Ocr => choice.ocr,
            })
            .map(|o| o.size)
            .sum();
        let any = rows > 0;
        rows + if any { self.engine_size.unwrap_or(0) } else { 0 }
    }

    /// The memory line under the rows, for the chosen writing model.
    pub fn memory_line(&self, choice: &PackChoice, macos: bool) -> Option<String> {
        let o = self.rows.iter().find(|o| choice.llm.as_deref() == Some(o.row.id))?;
        let gb = quire_ai::memory::gb(u64::from(o.ram_mb) * 1_000_000);
        Some(if macos {
            format!("Writing uses about {gb} of memory; close other programs if your Mac is short of memory.")
        } else {
            format!("Writing uses about {gb} of memory.")
        })
    }
}

/// What Settings → AI says while the pack is off, from what this build and
/// this computer offer, and whether "Turn on on-device AI…" is offered.
pub fn off_text(offer: &PackOffer) -> (String, bool) {
    const WAIT: &str = " Nothing is downloaded until you turn it on.";
    let writing = offer.rows.iter().any(|o| o.row.kind == PackKind::Writing && o.blocked.is_none());
    let writing_listed = offer.rows.iter().any(|o| o.row.kind == PackKind::Writing);
    let ocr = offer.rows.iter().any(|o| o.row.kind == PackKind::Ocr && o.blocked.is_none());
    match (writing, ocr) {
        (true, true) => {
            (format!("Qwen 3.5 for writing and the agent, and text in images and scanned PDFs, all on this computer.{WAIT}"), true)
        }
        (true, false) => (format!("Qwen 3.5 for writing and the agent, on this computer.{WAIT}"), true),
        (false, true) if writing_listed => {
            (format!("Text in images and scanned PDFs, read on this computer. Writing on this computer needs 8 GB of memory.{WAIT}"), true)
        }
        (false, true) => (format!("Text in images and scanned PDFs, read on this computer.{WAIT}"), true),
        (false, false) => ("This build of Quire can't download on-device models. A release build can.".to_string(), false),
    }
}

/// Free disk wanted for a download of `bytes`: the files, 10% and 500 MB.
pub const fn space_needed(bytes: u64) -> u64 {
    bytes + bytes / 10 + 500 * 1_000_000
}

/// `Err` with the message for the dialog when `free` is short of a download.
///
/// A download of `bytes` needs [`space_needed`]. An unknown `free` passes: the download starts,
/// and a full disk ends it with its message and the part deleted.
pub fn check_space(bytes: u64, free: Option<u64>) -> Result<(), String> {
    let need = space_needed(bytes);
    match free {
        Some(f) if f < need => {
            Err(format!("Not enough free space: needs {}, {} free.", quire_ai::memory::gb(need), quire_ai::memory::gb(f)))
        }
        _ => Ok(()),
    }
}

/// Free bytes on the disk that holds `dir` (or its nearest existing parent):
/// `df -Pk` on Linux and macOS, PowerShell's `Get-PSDrive` on Windows.
/// `None` when it can't be read.
pub fn free_disk_bytes(dir: &Path) -> Option<u64> {
    let dir = dir.ancestors().find(|d| d.exists())?;
    if cfg!(windows) {
        let drive = dir.to_string_lossy().chars().next().filter(char::is_ascii_alphabetic)?;
        let out = run("powershell", &["-NoProfile", "-NonInteractive", "-Command", &format!("(Get-PSDrive -Name {drive}).Free")])?;
        parse_number(&out)
    } else {
        parse_df(&run("df", &["-Pk", &dir.to_string_lossy()])?)
    }
}

fn run(program: &str, args: &[&str]) -> Option<String> {
    #[expect(clippy::disallowed_methods, reason = "reads free disk space (df or PowerShell), with fixed arguments and the models folder")]
    let mut cmd = std::process::Command::new(program);
    cmd.args(args).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `df -Pk`'s second line, fourth column (available KB), in bytes.
fn parse_df(out: &str) -> Option<u64> {
    let kb: u64 = out.lines().nth(1)?.split_whitespace().nth(3)?.parse().ok()?;
    kb.checked_mul(1024)
}

fn parse_number(out: &str) -> Option<u64> {
    out.lines().map(str::trim).find(|l| !l.is_empty())?.parse().ok()
}

/// Folders under the models folder that no pinned row of this build names
/// (a test build's files, an older model), with their size in bytes. Never
/// deleted without the person's click (Settings → AI).
pub fn stray_folders(models: &Path, catalog: &Catalog) -> Vec<(String, u64)> {
    let Ok(entries) = std::fs::read_dir(models) else { return Vec::new() };
    let mut out: Vec<(String, u64)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|name| name != ENGINE_ID && catalog.get(name).is_none_or(|e| !e.is_pinned()))
        .map(|name| {
            let bytes = dir_bytes(&models.join(&name));
            (name, bytes)
        })
        .collect();
    out.sort();
    out
}

fn dir_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir).map_or(0, |entries| {
        entries
            .filter_map(Result::ok)
            .map(|e| match e.file_type() {
                Ok(t) if t.is_dir() => dir_bytes(&e.path()),
                Ok(_) => e.metadata().map_or(0, |m| m.len()),
                Err(_) => 0,
            })
            .sum()
    })
}

impl Workspace {
    /// "Download and turn on": writes `pack` and the ticked capabilities
    /// (one atomic settings write), then queues the downloads one at a time.
    /// If the settings can't be saved nothing downloads. Crash after the
    /// write: the pack is on and Settings shows each model as not downloaded.
    pub fn pack_turn_on(&mut self, choice: &PackChoice) {
        let offer = offer(crate::ai::catalog(), crate::onboarding::memory_gb(), crate::ai::engine_installed());
        let allowed = |id: &str| offer.rows.iter().any(|o| o.row.id == id && o.blocked.is_none());
        let llm = choice.llm.clone().filter(|id| allowed(id));
        let ocr = choice.ocr && offer.rows.iter().any(|o| o.row.kind == PackKind::Ocr);
        let mut picks: Vec<PackPick<'_>> = llm.iter().map(|id| PackPick::Llm(id)).collect();
        if ocr {
            picks.push(PackPick::Ocr);
        }
        self.settings.ai_local.turn_pack_on(&picks);
        // Meaning search on this computer was reset with the rest: the built-in index takes over.
        if !self.settings.ai_local.embeddings_on() && self.settings.embeddings == "local" {
            self.settings.embeddings = "lsa".into();
        }
        if let Err(e) = self.settings.save() {
            self.notify(NoticeLevel::Error, format!("Couldn't save the settings, so nothing was downloaded: {e}"));
            return;
        }
        for id in llm.iter().map(String::as_str).chain(ocr.then_some("ocrs")) {
            self.ai_download(id);
        }
        self.apply_ai_settings();
    }

    /// Turning the pack off: nothing on this computer runs any more; the
    /// files stay until "Delete downloaded models".
    pub fn pack_turn_off(&mut self) {
        self.settings.ai_local.pack = false;
        if let Err(e) = self.settings.save() {
            self.notify(NoticeLevel::Error, format!("Couldn't save the settings: {e}"));
        }
        crate::ai::engine(&self.settings).stop();
        self.apply_ai_settings();
    }

    /// Deletes one of [`stray_folders`] (only a folder that list names).
    pub fn delete_stray_models(&mut self, name: &str) {
        let Some(dir) = crate::ai::models_dir() else { return };
        if !stray_folders(&dir, crate::ai::catalog()).iter().any(|(n, _)| n == name) {
            return;
        }
        if let Err(e) = quire_ai::download::delete(&dir, name) {
            self.notify(NoticeLevel::Error, format!("Couldn't delete the files: {e}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The built-in catalog with the pack's rows pinned (as a release will
    /// have them) and the engine's size known.
    fn pinned() -> Catalog {
        let mut c = Catalog::builtin();
        for m in &mut c.models {
            for (i, f) in m.files.iter_mut().enumerate() {
                f.sha256 = format!("{:064x}", i + 1);
                f.size = match m.id.as_str() {
                    "qwen3.5-4b-q4km" => 2_740_000_000,
                    "qwen3.5-9b-q4km" => 5_680_000_000,
                    "ocrs" => 6_500_000,
                    _ => 20_000_000,
                };
            }
        }
        c
    }

    fn ids(o: &PackOffer) -> Vec<(&'static str, Option<String>)> {
        o.rows.iter().map(|r| (r.row.id, r.blocked.clone())).collect()
    }

    #[test]
    fn the_pack_offers_4b_by_default_and_9b_from_16_gb() {
        let c = pinned();
        let o = offer(&c, Some(16), true);
        assert_eq!(ids(&o), [("qwen3.5-4b-q4km", None), ("qwen3.5-9b-q4km", None), ("ocrs", None)]);
        assert_eq!(o.default_llm, Some("qwen3.5-4b-q4km"), "4B even with 16 GB");
        let o = offer(&c, Some(8), true);
        assert_eq!(o.rows[1].blocked.as_deref(), Some("needs 16 GB"));
        assert_eq!(o.default_llm, Some("qwen3.5-4b-q4km"));
        let o = offer(&c, Some(4), true);
        assert!(o.rows[..2].iter().all(|r| r.blocked.as_deref() == Some("needs a computer with 8 GB of memory")));
        assert_eq!((o.default_llm, o.default_choice().ocr), (None, true), "OCR still offered below 8 GB");
        let o = offer(&c, None, true);
        assert_eq!((o.rows[0].blocked.clone(), o.rows[1].blocked.as_deref()), (None, Some("needs 16 GB")), "4B when in doubt");
    }

    /// Rows this build hasn't pinned are not offered: with Qwen 3.5 unpinned
    /// the pack is OCR only; with nothing pinned it offers nothing.
    #[test]
    fn unpinned_rows_are_not_offered() {
        let mut c = pinned();
        for m in c.models.iter_mut().filter(|m| m.id.starts_with("qwen3.5")) {
            m.files[0].sha256.clear();
        }
        let o = offer(&c, Some(16), true);
        assert_eq!(ids(&o), [("ocrs", None)]);
        assert_eq!(o.default_choice(), PackChoice { llm: None, ocr: true });
        assert!(offer(&Catalog::builtin(), Some(16), true).rows.iter().all(|r| Catalog::builtin().get(r.row.id).unwrap().is_pinned()));
    }

    #[test]
    fn the_download_size_sums_the_ticked_rows_and_the_lite_engine() {
        let c = pinned();
        let o = offer(&c, Some(16), true);
        let both = PackChoice { llm: Some("qwen3.5-4b-q4km".into()), ocr: true };
        let files = |id: &str| c.get(id).unwrap().size();
        assert_eq!(o.download_bytes(&both), files("qwen3.5-4b-q4km") + files("ocrs"));
        assert_eq!(o.download_bytes(&PackChoice { llm: None, ocr: true }), files("ocrs"));
        assert_eq!(o.download_bytes(&PackChoice::default()), 0);
        // A blocked row adds nothing.
        let small = offer(&c, Some(8), true);
        assert_eq!(small.download_bytes(&PackChoice { llm: Some("qwen3.5-9b-q4km".into()), ocr: false }), 0);
        // Lite: the engine comes with the first download.
        let lite = offer(&c, Some(16), false);
        assert_eq!(lite.engine_size, Some(c.size_of(ENGINE_ID)));
        assert_eq!(lite.download_bytes(&both), o.download_bytes(&both) + c.size_of(ENGINE_ID));
        assert_eq!(o.memory_line(&both, false).as_deref(), Some("Writing uses about 3.3 GB of memory."));
        assert!(o.memory_line(&both, true).unwrap().contains("your Mac"));
        assert_eq!(o.memory_line(&PackChoice { llm: None, ocr: true }, false), None);
    }

    #[test]
    fn free_space_is_checked_with_a_margin() {
        // 2.76 GB + 10% + 0.5 GB = 3.5 GB.
        assert_eq!(space_needed(2_760_000_000), 3_536_000_000);
        assert!(check_space(2_760_000_000, Some(3_600_000_000)).is_ok());
        assert_eq!(check_space(2_760_000_000, Some(2_100_000_000)).unwrap_err(), "Not enough free space: needs 3.5 GB, 2.1 GB free.");
        assert!(check_space(2_760_000_000, None).is_ok(), "unknown: the download decides");
        let df = "Filesystem     1024-blocks      Used Available Capacity Mounted on\n/dev/vda        263174176  28123456  16000000      64% /\n";
        assert_eq!(parse_df(df), Some(16_000_000 * 1024));
        assert_eq!(parse_df("garbage"), None);
        assert_eq!(parse_number("\r\n 41000000000 \r\n"), Some(41_000_000_000));
        // This machine's own disk can be read (Linux and macOS have df).
        if cfg!(unix) {
            assert!(free_disk_bytes(&std::env::temp_dir().join("no-such-folder/deeper")).is_some());
        }
    }

    /// The dialog from a 0.13 settings file (llm, embeddings and an old
    /// model ticked, experimental off): with OCR ticked only, nothing old wakes.
    #[test]
    fn the_dialog_wakes_nothing_ticked_before_the_pack() {
        if std::env::var_os(crate::settings::EXPERIMENTAL_ENV).is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.toml");
        std::fs::write(
            &p,
            "[ai_local]\nexperimental = false\nengine = true\nllm = true\nembeddings = true\nllm_model = \"qwen3-1.7b-q4km\"\n",
        )
        .unwrap();
        let mut ws = Workspace::new(crate::Settings::load_from(&p));
        ws.pack_turn_on(&PackChoice { llm: None, ocr: true });
        let a = &ws.settings.ai_local;
        assert!(a.pack);
        assert!(!a.llm_on() && !a.embeddings_on(), "the old model and embedder stay off");
        assert_eq!(a.llm_model, "");
    }

    /// Settings → AI promises only what can be offered (fails on the old
    /// fixed sentence, which always promised Qwen 3.5).
    #[test]
    fn the_off_text_follows_the_offer() {
        let c = pinned();
        let (t, on) = off_text(&offer(&c, Some(16), true));
        assert!(on && t.starts_with("Qwen 3.5 for writing and the agent, and text in images"), "{t}");
        let (t, on) = off_text(&offer(&c, Some(4), true));
        assert!(on && t.contains("needs 8 GB") && !t.contains("Qwen 3.5"), "{t}");
        let mut ocr_only = pinned();
        for m in ocr_only.models.iter_mut().filter(|m| m.id.starts_with("qwen3.5")) {
            m.files[0].sha256.clear();
        }
        let (t, on) = off_text(&offer(&ocr_only, Some(16), true));
        assert!(on && t.starts_with("Text in images") && !t.contains("Qwen") && !t.contains("8 GB"), "{t}");
        let mut no_ocr = pinned();
        no_ocr.models.retain(|m| m.id != "ocrs");
        let (t, on) = off_text(&offer(&no_ocr, Some(16), true));
        assert!(on && t.starts_with("Qwen 3.5 for writing and the agent, on this computer"), "{t}");
        let (t, on) = off_text(&offer(&Catalog::builtin(), Some(16), true));
        assert!(!on && t.contains("can't download"), "this build pins nothing yet: {t}");
    }

    #[test]
    fn stray_model_folders_are_listed_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        for (name, bytes) in [("qwen3-1.7b-q4km", 1100), ("engine", 10), ("old-test-model", 20)] {
            std::fs::create_dir_all(dir.path().join(name)).unwrap();
            std::fs::write(dir.path().join(name).join("model.gguf"), vec![0u8; bytes]).unwrap();
        }
        std::fs::write(dir.path().join("installed.toml"), "format = 1\n").unwrap();
        let mut c = pinned();
        // An unpinned row's folder is stray too; a pinned one's is not.
        c.models.retain(|m| m.id != "qwen3-1.7b-q4km");
        std::fs::create_dir_all(dir.path().join("ocrs")).unwrap();
        assert_eq!(stray_folders(dir.path(), &c), [("old-test-model".to_string(), 20), ("qwen3-1.7b-q4km".to_string(), 1100)]);
        assert!(dir.path().join("old-test-model").exists(), "listing deletes nothing");
        assert!(stray_folders(&dir.path().join("missing"), &c).is_empty());
    }
}
