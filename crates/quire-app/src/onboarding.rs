// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! First-run AI choice (design §7.1).
//!
//! * A new install shows the two-step dialog once, right after the tour.
//! * Someone upgrading from 0.8 (the tour was done before this version
//!   started) gets a dismissible card above the note instead, never a dialog.
//!
//! Choosing does what it says and nothing more: "No AI" changes nothing,
//! "Use a provider" opens the provider form, "On this computer" turns the
//! chosen capabilities on and starts their downloads only on "Download now".
//!
//! Owned by WS-D; kept in `ws.features.onboarding`.

use crate::Command;
use crate::workspace::{UiRequest, Workspace};

/// State for this feature.
#[derive(Debug, Default)]
pub struct OnboardingState {
    /// Whether this run started with the tour already done (an upgrade or a
    /// later start), decided on the first tick.
    started_after_tour: Option<bool>,
    /// The dialog was asked for in this run.
    asked: bool,
}

/// The on-device capabilities a person can pick (step 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeviceChoice {
    pub embeddings: bool,
    /// The multilingual embedder instead of the English one.
    pub multilingual: bool,
    pub llm: bool,
    /// Catalog id of the language model size.
    pub llm_size: LlmSize,
    pub ocr: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmSize {
    Small,
    Balanced,
    Better,
}

/// The suggested size: Balanced with 8 GB of memory or more, otherwise (or
/// when the memory can't be read) Small. 0.10 measured about 1.3 GB in use
/// for Balanced after a 256-token prompt and 1.65 GB after 1,024 (5.1 GB and
/// out of memory with candle's own model), and 1.35 and 1.86 GB for Small,
/// whose weights the engine still repacks for speed
/// (docs/formats/model-catalog.md).
impl Default for LlmSize {
    fn default() -> Self {
        Self::for_memory_gb(memory_gb())
    }
}

impl LlmSize {
    pub fn for_memory_gb(gb: Option<u64>) -> Self {
        if gb.is_some_and(|g| g >= 7) { Self::Balanced } else { Self::Small }
    }

    pub const fn id(self) -> &'static str {
        match self {
            Self::Small => "qwen3-0.6b-q8",
            Self::Balanced => "qwen3-1.7b-q4km",
            Self::Better => "qwen3-4b-q4km",
        }
    }
}

/// This computer's memory in GB, rounded; `None` when it can't be read.
/// Read once per run (on Windows it asks another program).
pub fn memory_gb() -> Option<u64> {
    static GB: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *GB.get_or_init(|| memory_bytes().map(|b| (b + (1 << 29)) >> 30))
}

fn memory_bytes() -> Option<u64> {
    if cfg!(target_os = "linux") {
        parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
    } else if cfg!(target_os = "macos") {
        let out = std::process::Command::new("/usr/sbin/sysctl").args(["-n", "hw.memsize"]).output().ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    } else if cfg!(windows) {
        windows_memory()
    } else {
        None
    }
}

/// `MemTotal:` from `/proc/meminfo`, in bytes.
fn parse_meminfo(info: &str) -> Option<u64> {
    let kb: u64 = info.lines().find_map(|l| l.strip_prefix("MemTotal:"))?.trim().trim_end_matches("kB").trim().parse().ok()?;
    Some(kb * 1024)
}

/// Windows without unsafe code: ask PowerShell (CIM), then `wmic` (older
/// systems; removed from the newest), with no console window.
fn windows_memory() -> Option<u64> {
    let run = |program: &str, args: &[&str]| -> Option<String> {
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
    };
    run(
        "powershell",
        &["-NoProfile", "-NonInteractive", "-Command", "(Get-CimInstance -ClassName Win32_ComputerSystem).TotalPhysicalMemory"],
    )
    .as_deref()
    .and_then(parse_powershell_bytes)
    .or_else(|| run("wmic", &["ComputerSystem", "get", "TotalPhysicalMemory", "/value"]).as_deref().and_then(parse_wmic_bytes))
}

/// PowerShell prints the number alone (`17079209984`).
fn parse_powershell_bytes(out: &str) -> Option<u64> {
    out.lines().map(str::trim).find(|l| !l.is_empty())?.parse().ok().filter(|&b| b > 0)
}

/// `wmic … /value` prints `TotalPhysicalMemory=17079209984` among blank lines
/// (UTF-16 turned into text by the caller, so stray NULs are dropped).
fn parse_wmic_bytes(out: &str) -> Option<u64> {
    out.lines()
        .find_map(|l| l.trim().trim_matches('\0').strip_prefix("TotalPhysicalMemory="))
        .map(|v| v.chars().filter(char::is_ascii_digit).collect::<String>())?
        .parse()
        .ok()
        .filter(|&b| b > 0)
}

impl DeviceChoice {
    /// Catalog ids to download for this choice.
    pub fn models(self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.embeddings {
            v.push(if self.multilingual { "multilingual-e5-small" } else { "bge-small-en-v1.5" });
        }
        if self.llm {
            v.push(self.llm_size.id());
        }
        if self.ocr {
            v.push("ocrs");
        }
        v
    }
}

/// Once per run: after the tour of a new install, ask for the AI choice.
pub(crate) fn tick(ws: &mut Workspace) {
    let tour_done = ws.settings.tour_done;
    let has_vault = ws.vault().is_some();
    let st = &mut ws.features.onboarding;
    let first = *st.started_after_tour.get_or_insert(tour_done);
    if first || st.asked || ws.settings.onboarding.ai_done || !tour_done || !has_vault {
        return;
    }
    st.asked = true;
    ws.request_ui(UiRequest::Feature(Command::AiSetup));
}

/// The upgrade card shows: 0.8 users who haven't chosen or dismissed it.
pub fn show_upgrade_card(ws: &Workspace) -> bool {
    let o = &ws.settings.onboarding;
    ws.features.onboarding.started_after_tour == Some(true) && !o.ai_done && !o.upgrade_card_done && ws.vault().is_some()
}

impl Workspace {
    /// Step 1: "No AI". Nothing changes.
    pub fn onboarding_no_ai(&mut self) {
        self.finish_onboarding("off");
    }

    /// Step 1: "Use a provider" (the provider form follows).
    pub fn onboarding_provider(&mut self) {
        let device = matches!(self.settings.onboarding.ai_choice.as_str(), "device" | "both");
        self.finish_onboarding(if device { "both" } else { "cloud" });
    }

    /// Step 2: turn on what was ticked; `download` starts the downloads now
    /// ("Download now"), otherwise they wait for Settings → AI ("Later").
    pub fn onboarding_device(&mut self, choice: DeviceChoice, download: bool) {
        let a = &mut self.settings.ai_local;
        a.engine |= choice.embeddings || choice.llm || choice.ocr;
        if choice.embeddings {
            a.embeddings = true;
            a.embed_model = (if choice.multilingual { "multilingual-e5-small" } else { "bge-small-en-v1.5" }).into();
            self.settings.embeddings = "local".into();
        }
        if choice.llm {
            a.llm = true;
            a.llm_model = choice.llm_size.id().into();
        }
        if choice.ocr {
            a.ocr = true;
        }
        let cloud = matches!(self.settings.onboarding.ai_choice.as_str(), "cloud" | "both");
        self.finish_onboarding(if cloud { "both" } else { "device" });
        if download {
            for id in choice.models() {
                self.ai_download(id);
            }
        }
        self.apply_ai_settings();
    }

    /// The upgrade card's ✕.
    pub fn dismiss_upgrade_card(&mut self) {
        self.settings.onboarding.upgrade_card_done = true;
        let _ = self.settings.save();
    }

    fn finish_onboarding(&mut self, choice: &str) {
        let o = &mut self.settings.onboarding;
        o.ai_done = true;
        o.upgrade_card_done = true;
        o.ai_choice = choice.into();
        let _ = self.settings.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;
    use crate::vault::Vault;

    #[test]
    fn new_installs_get_the_dialog_after_the_tour_and_upgraders_a_card() {
        let mut ws = Workspace::new(Settings::default());
        ws.attach_vault(Vault::in_memory(&[("A.md", "# A\n")]));
        tick(&mut ws);
        assert!(ws.take_ui_requests().is_empty(), "not during the tour");
        ws.settings.tour_done = true;
        tick(&mut ws);
        assert_eq!(ws.take_ui_requests(), vec![UiRequest::Feature(Command::AiSetup)]);
        tick(&mut ws);
        assert!(ws.take_ui_requests().is_empty(), "once");
        assert!(!show_upgrade_card(&ws));

        let mut up = Workspace::new(Settings { tour_done: true, ..Settings::default() });
        up.attach_vault(Vault::in_memory(&[("A.md", "# A\n")]));
        tick(&mut up);
        assert!(up.take_ui_requests().is_empty(), "upgraders never get a dialog");
        assert!(show_upgrade_card(&up));
        up.settings.onboarding.upgrade_card_done = true;
        assert!(!show_upgrade_card(&up));
    }

    /// Someone who chose "On this computer" (or both) before on-device AI
    /// became experimental keeps that choice through a load and a save, and
    /// through choosing a provider.
    #[test]
    fn an_earlier_device_choice_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.toml");
        for choice in ["device", "both"] {
            std::fs::write(
                &p,
                format!(
                    "[onboarding]\nai_done = true\nai_choice = \"{choice}\"\n\n[ai_local]\nengine = true\nllm = true\nfuture_key = 1\n"
                ),
            )
            .unwrap();
            let s = Settings::load_from(&p);
            assert_eq!(s.onboarding.ai_choice, choice);
            assert!(!s.ai_local.experimental && s.ai_local.llm && s.ai_local.other.contains_key("future_key"));
            s.save_to(&p).unwrap();
            let back = Settings::load_from(&p);
            assert_eq!(back, s, "nothing lost in a save");
            assert!(std::fs::read_to_string(&p).unwrap().contains(&format!("ai_choice = \"{choice}\"")));
            let mut ws = Workspace::new(back);
            ws.onboarding_provider();
            assert_eq!(ws.settings.onboarding.ai_choice, "both", "a provider is added to the device choice");
            assert!(ws.settings.ai_local.llm, "and the capability stays as it was");
        }
    }

    #[test]
    fn choices_change_only_what_they_say() {
        let mut ws = Workspace::new(Settings::default());
        let before = ws.settings.clone();
        ws.onboarding_no_ai();
        assert_eq!(ws.settings.ai_provider, before.ai_provider);
        assert_eq!(ws.settings.ai_local, before.ai_local, "No AI turns nothing on");
        assert_eq!((ws.settings.onboarding.ai_done, ws.settings.onboarding.ai_choice.as_str()), (true, "off"));
        ws.onboarding_device(DeviceChoice { embeddings: true, ocr: true, ..DeviceChoice::default() }, false);
        assert!(ws.settings.ai_local.embeddings && ws.settings.ai_local.ocr && !ws.settings.ai_local.llm);
        assert_eq!(ws.settings.embeddings, "local");
        assert!(!ws.features.ai.busy(), "Later doesn't download");
        assert_eq!(DeviceChoice { llm: true, llm_size: LlmSize::Small, ..DeviceChoice::default() }.models(), vec!["qwen3-0.6b-q8"]);
        assert_eq!(LlmSize::for_memory_gb(Some(4)), LlmSize::Small);
        assert_eq!(LlmSize::for_memory_gb(Some(8)), LlmSize::Balanced);
        assert_eq!(LlmSize::for_memory_gb(Some(16)), LlmSize::Balanced);
        assert_eq!(LlmSize::for_memory_gb(None), LlmSize::Small);
    }
}

#[cfg(test)]
mod memory_tests {
    use super::*;

    #[test]
    fn reads_memory_from_each_systems_output() {
        assert_eq!(parse_meminfo("MemTotal:       16303516 kB\nMemFree: 1 kB\n"), Some(16_303_516 * 1024));
        assert_eq!(parse_powershell_bytes("17079209984\r\n"), Some(17_079_209_984));
        assert_eq!(parse_powershell_bytes("\r\n  8589934592  \r\n"), Some(8_589_934_592));
        assert_eq!(parse_powershell_bytes("Get-CimInstance : Access denied\r\n"), None);
        assert_eq!(parse_powershell_bytes(""), None);
        assert_eq!(parse_wmic_bytes("\r\r\n\r\r\nTotalPhysicalMemory=17079209984\r\r\n\r\r\n\r\r\n"), Some(17_079_209_984));
        assert_eq!(parse_wmic_bytes("TotalPhysicalMemory=34359738368\0\n"), Some(34_359_738_368));
        assert_eq!(parse_wmic_bytes("No Instance(s) Available.\r\n"), None);
        assert_eq!(parse_wmic_bytes("TotalPhysicalMemory=\r\n"), None);
    }
}
