// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Ask your vault: state and workspace operations (design §C.4).
//!
//! A question is answered from the vault's passages: by the AI model when
//! one is set up (citing `[n]`, or `NOT_FOUND`), else by quoting the best
//! sentences. When the notes don't hold the answer, the answer says so and
//! lists the nearest passages. With ask-before-tell, the person first says
//! what they expect, then sees the evidence grouped by topic, then the
//! answer (which, from a model, says where the evidence agrees with them).
//! An answer can be saved as a note with its citations as footnotes.

use std::sync::Arc;

use crossbeam_channel::Receiver;
use quire_agent::policy::ZoneRole;
use quire_agent::{Actor, Change, ChangeSet};
use quire_intel::ask::{self, AnswerMeta};
use quire_intel::cluster::{Cluster, evidence_clusters};
use quire_intel::llm::{Assist, LlmError, LlmRequest, spawn};
use quire_intel::{Answer, Source, retrieve};
use quire_text::{EditKind, Selection, Transaction};
use quire_vault::NotePath;

use crate::workspace::{NoticeLevel, Workspace};

type PendingAnswer = (Vec<Source>, Receiver<Result<String, LlmError>>);

/// Where an ask is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AskStep {
    /// Typing the question.
    #[default]
    Question,
    /// Ask-before-tell 1: "What do you expect the answer to be?"
    Expect,
    /// Ask-before-tell 2: the evidence, grouped.
    Evidence,
    /// The answer (or waiting for it).
    Answer,
}

#[derive(Default)]
pub struct AskState {
    pub open: bool,
    pub question: String,
    /// Ask before telling, for this question (starts from the setting).
    pub before_tell: bool,
    /// What the person expected (ask-before-tell; may be empty).
    pub hypothesis: String,
    pub step: AskStep,
    /// The passages found for the question.
    pub sources: Vec<Source>,
    pub clusters: Vec<Cluster>,
    pub answer: Option<Answer>,
    pub error: Option<String>,
    /// The note the answer was saved to.
    pub saved: Option<NotePath>,
    pending: Option<PendingAnswer>,
}

impl AskState {
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Back to an empty question (keeps the ask-before-tell choice).
    pub fn reset(&mut self, before_tell: bool) {
        *self = Self { open: self.open, before_tell, ..Self::default() };
    }
}

impl Workspace {
    /// Start answering `self.ask.question`: with ask-before-tell, go to the
    /// "what do you expect" step; otherwise answer now.
    pub fn ask(&mut self) {
        let q = self.ask.question.trim().to_string();
        if q.is_empty() {
            return;
        }
        self.ask.error = None;
        self.ask.answer = None;
        self.ask.saved = None;
        self.ask.clusters.clear();
        let sem = self.index.semantic();
        self.ask.sources = {
            let ix = self.index.read();
            retrieve(&ix, sem.as_deref(), &q, 6)
        };
        if self.ask.before_tell && !self.ask.sources.is_empty() {
            self.ask.step = AskStep::Expect;
            return;
        }
        self.ask_answer();
    }

    /// Ask-before-tell: show the evidence (after the expectation, or Skip).
    pub fn ask_evidence(&mut self) {
        let sem = self.index.semantic();
        let q = self.ask.question.clone();
        self.ask.clusters = {
            let ix = self.index.read();
            evidence_clusters(&q, &self.ask.sources, sem.as_deref(), &ix)
        };
        self.ask.step = AskStep::Evidence;
    }

    /// Answer from the sources already found. Offline answers are instant;
    /// model answers arrive via [`Workspace::tick`].
    pub fn ask_answer(&mut self) {
        self.ask.step = AskStep::Answer;
        let q = self.ask.question.trim().to_string();
        let sem = self.index.semantic();
        let sources = self.ask.sources.clone();
        if self.assistant.is_offline() || sources.is_empty() {
            let abstain = ask::should_abstain(&sources, sem.as_deref());
            let markdown = if abstain { None } else { ask::extractive_markdown(&q, &sources, sem.as_deref()) };
            self.ask.answer = Some(self.finish_answer(markdown, sources, "extractive (offline)".into()));
            return;
        }
        let hyp = Some(self.ask.hypothesis.trim()).filter(|h| !h.is_empty() && self.ask.before_tell);
        let req = ask::build_chat_request(&q, &sources, hyp);
        let llm = LlmRequest { system: req.system.clone(), prompt: req.messages[0].text(), max_tokens: req.max_tokens };
        let rx = spawn(Arc::clone(&self.assistant), Assist::Ask { question: q }, llm);
        self.ask.pending = Some((sources, rx));
    }

    /// The answer with its span citations, or "not found" (`markdown: None`).
    fn finish_answer(&self, markdown: Option<String>, sources: Vec<Source>, by: String) -> Answer {
        let Some(markdown) = markdown.filter(|m| !ask::model_abstained(m)) else {
            return Answer { markdown: ask::abstain_markdown(&sources), sources, by, abstained: true, citations: Vec::new() };
        };
        let note = |p: &NotePath| {
            self.docs
                .iter()
                .find(|d| d.path.as_ref() == Some(p))
                .map(|d| d.text().to_string())
                .or_else(|| self.index.read().source(p).map(str::to_string))
        };
        let citations = ask::cite_spans(&markdown, &sources, &note);
        Answer { markdown, sources, by, abstained: false, citations }
    }

    pub(crate) fn poll_ask(&mut self) -> bool {
        let Some((_, rx)) = &self.ask.pending else { return false };
        let Ok(res) = rx.try_recv() else { return false };
        let (sources, _) = self.ask.pending.take().expect("checked above");
        match res {
            Ok(markdown) => self.ask.answer = Some(self.finish_answer(Some(markdown), sources, self.assistant.name().to_string())),
            Err(e) => {
                // Fall back to an extractive answer so the user still gets something.
                let sem = self.index.semantic();
                let q = self.ask.question.clone();
                let abstain = ask::should_abstain(&sources, sem.as_deref());
                let markdown = if abstain { None } else { ask::extractive_markdown(&q, &sources, sem.as_deref()) };
                self.ask.error = Some(e.to_string());
                self.ask.answer = Some(self.finish_answer(markdown, sources, "extractive (offline)".into()));
            }
        }
        true
    }

    /// Where "Save answer as note" saves: `wiki/answers/` when the vault has
    /// an agent-owned `wiki` zone, else `Answers/`.
    pub fn answer_folder(&self) -> &'static str {
        let wiki = self.agent_store().is_some_and(|s| {
            s.policy().zones.iter().any(|z| z.role == ZoneRole::Agent && z.path.trim_end_matches('/').eq_ignore_ascii_case("wiki"))
        });
        ask::answer_folder(wiki)
    }

    /// Save the answer as a note (the person's own write, with history),
    /// then open it. Its citations are checked in the background, with the
    /// AI model when citation checks are on.
    pub fn save_answer_note(&mut self) -> Option<NotePath> {
        let a = self.ask.answer.clone().filter(|a| !a.abstained)?;
        let q = self.ask.question.trim().to_string();
        let (Some(vault), Some(store)) = (self.vault.clone(), self.agent_store().cloned()) else { return None };
        let folder = self.answer_folder();
        let mut name = quire_vault::sanitize_title(&q);
        name.truncate(name.char_indices().nth(80).map_or(name.len(), |(i, _)| i));
        if name.trim().is_empty() {
            name = "Answer".into();
        }
        let path = match vault.unique_path(folder, &name) {
            Ok(p) => p,
            Err(e) => {
                self.notify(NoticeLevel::Error, format!("Couldn't save the answer: {e}"));
                return None;
            }
        };
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let model = if a.by.starts_with("extractive") { "Quire (quoting your notes)".to_string() } else { a.by.clone() };
        let text = ask::answer_note(&AnswerMeta { question: &q, date: &date, model: &model, answered_by: "human" }, &a);
        let mut cs = ChangeSet::one(Change::Create { path: path.clone(), text: text.clone() });
        cs.title = format!("Save answer: {q}");
        match store.submit(&Actor::human(&store.device().id), cs) {
            Ok(_) => {
                self.index.upsert(path.clone(), text);
                self.index.flush();
                self.ask.saved = Some(path.clone());
                self.open_note(&path);
                // Check the citations: with the model when it answered and checks are on.
                if !a.citations.is_empty() {
                    let verify = self.settings.trust.verify_citations && !a.by.starts_with("extractive") && self.trust_chat().is_some();
                    let shared = &self.features.trust.shared;
                    let q = if verify { &shared.check_now } else { &shared.drift_changed };
                    if let Ok(mut q) = q.lock() {
                        q.push(path.clone());
                    }
                }
                self.notify(NoticeLevel::Info, format!("Saved the answer to {path}"));
                Some(path)
            }
            Err(e) => {
                self.notify(NoticeLevel::Error, format!("Couldn't save the answer: {e}"));
                None
            }
        }
    }

    /// Insert the answer and its sources at the end of the active note.
    pub fn insert_answer(&mut self) -> bool {
        let Some(a) = self.ask.answer.clone() else { return false };
        let q = self.ask.question.clone();
        let Some(doc) = self.active_doc_mut() else { return false };
        let sources: Vec<String> = a.sources.iter().map(|s| format!("{}. {}", s.n, s.citation())).collect();
        let block = format!("\n\n> **Q:** {q}\n\n{}\n\nSources: {}\n", a.markdown.trim(), sources.join(" · "));
        let end = doc.buffer().len_chars();
        let caret = end + block.chars().count();
        doc.apply(Transaction::insert(end, block).with_selection(Selection::cursor(caret)), EditKind::Other).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use crate::{Settings, Workspace};
    use quire_vault::{NotePath, Vault};

    use super::AskStep;

    fn ws() -> Workspace {
        let mut w = Workspace::new(Settings::default());
        w.attach_vault(Vault::in_memory(&[
            ("Rope Notes.md", "# Rope Notes\n## Undo\nRopes make undo cheap because unchanged subtrees are shared.\n"),
            ("Bread.md", "# Bread\nSourdough needs a slow rise."),
        ]));
        w.index().flush_semantic();
        w
    }

    #[test]
    fn offline_answer_cites_spans_and_saves_as_a_note() {
        let mut w = ws();
        w.ask.question = "why is undo cheap?".into();
        w.ask();
        let a = w.ask.answer.clone().unwrap();
        assert!(!a.abstained);
        assert!(a.markdown.contains("[1]"), "{}", a.markdown);
        assert_eq!(a.sources[0].path, NotePath::new("Rope Notes.md"));
        assert_eq!(a.citations[0].1.quote, "Ropes make undo cheap because unchanged subtrees are shared.");
        w.open_note(&NotePath::new("Bread.md")).unwrap();
        assert!(w.insert_answer());
        assert!(w.active_doc().unwrap().text().contains("> **Q:** why is undo cheap?"));
        let saved = w.save_answer_note().unwrap();
        assert_eq!(saved.as_str(), "wiki/answers/why is undo cheap.md", "the default policy has an agent-owned wiki/");
        let text = w.vault().unwrap().read(&saved).unwrap();
        assert!(text.contains("> [!ai] Written by Quire (quoting your notes) from 1 note on"));
        assert!(
            text.contains("[^1]: [[Rope Notes#Undo]] — “Ropes make undo cheap because unchanged subtrees are shared.” <!-- quire:cite v=1")
        );
        assert_eq!(w.active_doc().unwrap().path.as_ref(), Some(&saved));
    }

    #[test]
    fn abstains_and_asks_before_telling() {
        let mut w = ws();
        w.ask.question = "what is the annual budget of the rocket programme?".into();
        w.ask();
        let a = w.ask.answer.clone().unwrap();
        assert!(a.abstained);
        assert!(a.markdown.starts_with("Not found in your notes."));
        assert!(w.save_answer_note().is_none(), "nothing to save");
        w.ask.reset(true);
        w.ask.question = "why is undo cheap?".into();
        w.ask();
        assert_eq!(w.ask.step, AskStep::Expect);
        assert!(w.ask.answer.is_none());
        w.ask.hypothesis = "something about trees".into();
        w.ask_evidence();
        assert_eq!(w.ask.step, AskStep::Evidence);
        assert!(!w.ask.clusters.is_empty());
        w.ask_answer();
        assert_eq!(w.ask.step, AskStep::Answer);
        assert!(w.ask.answer.is_some());
    }
}
