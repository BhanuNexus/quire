// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `[ai_local]` in settings.toml: on-device AI (`docs/design/0.9.md` §D.1–§D.3).
//!
//! Nothing runs or downloads unless turned on.
//!
//! 0.14 (`docs/design/0.14-local-ai.md` §3.8): the on-device AI pack. Each
//! capability (`llm`, `ocr`, `embeddings`) works only when `pack` is set (the
//! person turned the pack on, or ran `quire-cli ai download`), or behind the
//! older `experimental` switch or `QUIRE_EXPERIMENTAL_ON_DEVICE=1`. Keys set
//! under 0.9–0.13 stay inert until then. Voice transcription on this computer
//! still needs `experimental`.
//!
//! Every field defaults to off or unchanged; keys this
//! version doesn't know are kept in `other`, so older and newer Quire
//! versions can share the file.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiLocalSettings {
    /// Show and use on-device AI (Settings → AI → Advanced). Off in 0.9.0.
    pub experimental: bool,
    /// The on-device AI pack was turned on (0.14): written only when the
    /// person turns it on. 0.13 keeps it in `other`.
    pub pack: bool,
    /// The engine may run at all.
    pub engine: bool,
    /// Meaning search on this computer.
    pub embeddings: bool,
    /// Writing and agent model on this computer.
    pub llm: bool,
    /// Read text in images and scanned PDFs.
    pub ocr: bool,
    /// Catalog id of the embedder.
    pub embed_model: String,
    /// Catalog id of the language model.
    pub llm_model: String,
    /// 0 = automatic.
    pub threads: u32,
    /// `"auto"`, `"on"` or `"off"` (Metal on macOS).
    pub gpu: String,
    /// Unload an idle model after this many seconds.
    pub idle_unload_s: u64,
    /// Replaces the download host; empty for the catalog URLs.
    pub mirror_url: String,
    /// 0.10: expand every model weight into the engine's faster x86 tiles
    /// (prompts read about twice as fast; the Balanced model then uses about
    /// 4 GB instead of 1.7 GB). Off: only the Small model's are.
    pub fast_prompts: bool,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Default for AiLocalSettings {
    fn default() -> Self {
        Self {
            experimental: false,
            pack: false,
            engine: false,
            embeddings: false,
            llm: false,
            ocr: false,
            embed_model: "bge-small-en-v1.5".into(),
            // No model chosen: the pack dialog chooses one.
            llm_model: String::new(),
            threads: 0,
            gpu: "auto".into(),
            idle_unload_s: 300,
            mirror_url: String::new(),
            fast_prompts: false,
            other: toml::Table::new(),
        }
    }
}

/// Set to `1` to turn on on-device AI for this process (testing).
pub const EXPERIMENTAL_ENV: &str = "QUIRE_EXPERIMENTAL_ON_DEVICE";

fn env_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os(EXPERIMENTAL_ENV).is_some_and(|v| v == "1"))
}

/// A model the person chose for the pack: what it turns on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackPick<'a> {
    /// The writing and agent model, by catalog id.
    Llm(&'a str),
    Ocr,
    /// The embedder for meaning search, by catalog id.
    Embed(&'a str),
}

impl AiLocalSettings {
    /// Turns the pack on with `picks` (the dialog's ticked rows, or the model
    /// `quire-cli ai download` fetches). When the pack was off (and the
    /// experimental switch too), capabilities ticked under 0.9–0.13 are reset
    /// first, so only what was picked runs: `llm`, `ocr` and `embeddings` are
    /// exactly the picks and an old `llm_model` is cleared. When it was on
    /// already, the picks are added to what is on.
    pub fn turn_pack_on(&mut self, picks: &[PackPick<'_>]) {
        if !self.pack_on() {
            self.llm = false;
            self.ocr = false;
            self.embeddings = false;
            self.llm_model.clear();
        }
        self.pack = true;
        self.engine = true;
        for pick in picks {
            match *pick {
                PackPick::Llm(id) => {
                    self.llm = true;
                    self.llm_model = id.to_string();
                }
                PackPick::Ocr => self.ocr = true,
                PackPick::Embed(id) => {
                    self.embeddings = true;
                    self.embed_model = id.to_string();
                }
            }
        }
    }

    /// On-device AI is offered at all: the experimental switch or
    /// [`EXPERIMENTAL_ENV`]. When false nothing on-device is shown, spawned
    /// or downloaded, whatever the other keys say.
    pub fn on_device(&self) -> bool {
        self.experimental || env_on()
    }

    /// The pack is on, or the experimental switch: capabilities may run.
    pub fn pack_on(&self) -> bool {
        self.pack || self.on_device()
    }

    /// Meaning search on this computer is on (and the pack is).
    pub fn embeddings_on(&self) -> bool {
        self.embeddings && self.pack_on()
    }

    /// The on-device language model is on (and the pack is).
    pub fn llm_on(&self) -> bool {
        self.llm && self.pack_on()
    }

    /// OCR is on (and the pack is).
    pub fn ocr_on(&self) -> bool {
        self.ocr && self.pack_on()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Consent (`docs/design/0.14-local-ai.md` §3.8): capability keys set
    /// under 0.9–0.13 stay inert until the pack is turned on; the pack turns
    /// on only what was ticked; voice still needs the experimental switch.
    #[test]
    fn capabilities_need_the_pack_and_only_ticked_ones_turn_on() {
        if std::env::var_os(EXPERIMENTAL_ENV).is_some() {
            return; // the switch for testing overrides everything here
        }
        // A 0.13 settings file: capabilities ticked long ago, experimental off.
        let old: AiLocalSettings =
            toml::from_str("experimental = false\nllm = true\nocr = true\nembeddings = true\nllm_model = \"qwen3-1.7b-q4km\"\n").unwrap();
        assert!(!old.pack && !old.llm_on() && !old.ocr_on() && !old.embeddings_on() && !old.on_device(), "nothing starts");
        // The pack with only OCR ticked.
        let ocr_only = AiLocalSettings { pack: true, ocr: true, ..AiLocalSettings::default() };
        assert!(ocr_only.ocr_on() && !ocr_only.llm_on() && !ocr_only.embeddings_on());
        assert!(!ocr_only.on_device(), "voice on this computer still needs the experimental switch");
        // The older switch still works.
        let exp = AiLocalSettings { experimental: true, llm: true, ..AiLocalSettings::default() };
        assert!(exp.llm_on());
        // No model is chosen by default, and `pack` round-trips; 0.13 keeps it in `other`.
        assert_eq!(AiLocalSettings::default().llm_model, "");
        let text = toml::to_string(&AiLocalSettings { pack: true, ..AiLocalSettings::default() }).unwrap();
        assert!(text.contains("pack = true"));
        let back: AiLocalSettings = toml::from_str(&text).unwrap();
        assert!(back.pack);
    }

    /// A 0.13 file with capabilities ticked long ago (experimental off).
    fn old() -> AiLocalSettings {
        toml::from_str("experimental = false\nllm = true\nocr = false\nembeddings = true\nllm_model = \"qwen3-1.7b-q4km\"\n").unwrap()
    }

    /// Turning the pack on turns on what was chosen and nothing ticked in
    /// 0.9–0.13: the old model and embedder stay off.
    #[test]
    fn turning_the_pack_on_wakes_only_what_was_chosen() {
        if std::env::var_os(EXPERIMENTAL_ENV).is_some() {
            return;
        }
        let mut a = old();
        a.turn_pack_on(&[PackPick::Ocr]);
        assert!(a.pack && a.engine && a.ocr_on());
        assert!(!a.llm_on() && !a.embeddings_on(), "the 0.13 choices stay asleep");
        assert_eq!(a.llm_model, "", "the old model is not the chosen one");
        let mut a = old();
        a.turn_pack_on(&[PackPick::Llm("qwen3.5-4b-q4km")]);
        assert!(a.llm_on() && !a.ocr_on() && !a.embeddings_on());
        assert_eq!(a.llm_model, "qwen3.5-4b-q4km");
        // Already on: a second model adds its capability and keeps the rest.
        a.turn_pack_on(&[PackPick::Ocr]);
        assert!(a.llm_on() && a.ocr_on() && a.llm_model == "qwen3.5-4b-q4km");
        a.turn_pack_on(&[PackPick::Embed("bge-small-en-v1.5")]);
        assert!(a.embeddings_on() && a.embed_model == "bge-small-en-v1.5");
        // With the experimental switch the old keys were live already: kept.
        let mut a = AiLocalSettings { experimental: true, ..old() };
        a.turn_pack_on(&[PackPick::Ocr]);
        assert!(a.llm_on() && a.embeddings_on() && a.llm_model == "qwen3-1.7b-q4km");
        // Nothing chosen: the pack is on with nothing to run.
        let mut a = old();
        a.turn_pack_on(&[]);
        assert!(a.pack && !a.llm_on() && !a.ocr_on() && !a.embeddings_on());
    }
}
