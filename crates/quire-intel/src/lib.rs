// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Intelligence that runs on-device, deterministically, in milliseconds.
//!
//! Nothing here needs a network or a model download. An optional
//! [`llm::LanguageModel`] port adds generative features when the user opts in.

pub mod anchor;
pub mod ask;
pub mod calc;
pub mod chat;
pub mod claims;
pub mod cluster;
pub mod contradict;
pub mod dates;
#[cfg(feature = "remote-llm")]
pub mod embed;
pub mod json;
pub mod llm;
pub mod mentions;
pub mod related;
pub mod stale;
pub mod summary;
pub mod tags;
pub mod timeline;

pub use ask::{Answer, Source, build_ask_request, extractive_answer, retrieve};
pub use calc::{Calculator, format_number};
pub use chat::{CancelFlag, ChatEvent, ChatMessage, ChatModel, ChatRequest, Locality, ScriptedChat};
pub use dates::{DateMatch, parse_natural_date, trailing_date_phrase};
pub use mentions::{LinkSuggestion, UnlinkedMention, suggest_links, unlinked_mentions};
pub use related::{RelatedNote, related_notes, related_notes_with};
pub use summary::summarize;
pub use tags::suggest_tags;
