// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Ask your vault: state and workspace operations.

use crossbeam_channel::Receiver;
use quire_intel::llm::{Assist, LlmError, spawn};
use quire_intel::{Answer, Source, build_ask_request, extractive_answer, retrieve};
use quire_text::{EditKind, Selection, Transaction};

use crate::workspace::Workspace;

type PendingAnswer = (Vec<Source>, Receiver<Result<String, LlmError>>);

#[derive(Default)]
pub struct AskState {
    pub open: bool,
    pub question: String,
    pub answer: Option<Answer>,
    pub error: Option<String>,
    pending: Option<PendingAnswer>,
}

impl AskState {
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }
}

impl Workspace {
    /// Answer `self.ask.question` from the vault. Offline answers are instant;
    /// model answers arrive via [`Workspace::tick`].
    pub fn ask(&mut self) {
        let q = self.ask.question.trim().to_string();
        if q.is_empty() {
            return;
        }
        self.ask.error = None;
        self.ask.answer = None;
        let sem = self.index.semantic();
        let sources = {
            let ix = self.index.read();
            retrieve(&ix, sem.as_deref(), &q, 6)
        };
        if self.assistant.is_offline() || sources.is_empty() {
            let markdown = extractive_answer(&q, &sources, sem.as_deref());
            self.ask.answer = Some(Answer { markdown, sources, by: "extractive (offline)".into() });
            return;
        }
        let task = Assist::Ask { question: q.clone() };
        let rx = spawn(std::sync::Arc::clone(&self.assistant), task, build_ask_request(&q, &sources));
        self.ask.pending = Some((sources, rx));
    }

    pub(crate) fn poll_ask(&mut self) -> bool {
        let Some((_, rx)) = &self.ask.pending else { return false };
        let Ok(res) = rx.try_recv() else { return false };
        let (sources, _) = self.ask.pending.take().expect("checked above");
        match res {
            Ok(markdown) => self.ask.answer = Some(Answer { markdown, sources, by: self.assistant.name().to_string() }),
            Err(e) => {
                // Fall back to an extractive answer so the user still gets something.
                let sem = self.index.semantic();
                let markdown = extractive_answer(&self.ask.question, &sources, sem.as_deref());
                self.ask.error = Some(e.to_string());
                self.ask.answer = Some(Answer { markdown, sources, by: "extractive (offline)".into() });
            }
        }
        true
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

    #[test]
    fn offline_answer_cites_sources() {
        let mut w = Workspace::new(Settings::default());
        w.attach_vault(Vault::in_memory(&[
            ("Rope Notes.md", "# Rope Notes\n## Undo\nRopes make undo cheap because unchanged subtrees are shared.\n"),
            ("Bread.md", "# Bread\nSourdough needs a slow rise."),
        ]));
        w.index().flush_semantic();
        w.ask.question = "why is undo cheap?".into();
        w.ask();
        let a = w.ask.answer.clone().unwrap();
        assert!(a.markdown.contains("[[Rope Notes#Undo]]"), "{}", a.markdown);
        assert_eq!(a.sources[0].path, NotePath::new("Rope Notes.md"));
        w.open_note(&NotePath::new("Bread.md")).unwrap();
        assert!(w.insert_answer());
        assert!(w.active_doc().unwrap().text().contains("> **Q:** why is undo cheap?"));
    }
}
