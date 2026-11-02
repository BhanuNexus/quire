// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Git integration (design §F.5): detection, auto-commit timer, last result.
//!
//! Owned by WS-F; kept in `ws.features.git`.

/// State for this feature.
#[derive(Debug, Default)]
pub struct GitState {}
