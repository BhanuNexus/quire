// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The font files embedded in this crate.
//!
//! The text faces are the desktop app's own (`apps/quire-desktop/fonts/`);
//! the app can take them from here instead of embedding a second copy.

/// Inter Regular (SIL OFL 1.1).
pub const INTER_REGULAR: &[u8] = include_bytes!("../../../apps/quire-desktop/fonts/Inter-Regular.ttf");
/// Inter `SemiBold` (SIL OFL 1.1).
pub const INTER_SEMIBOLD: &[u8] = include_bytes!("../../../apps/quire-desktop/fonts/Inter-SemiBold.ttf");
/// Inter Italic (SIL OFL 1.1).
pub const INTER_ITALIC: &[u8] = include_bytes!("../../../apps/quire-desktop/fonts/Inter-Italic.ttf");
/// `JetBrains Mono NL` Regular (SIL OFL 1.1).
pub const JETBRAINS_MONO_NL: &[u8] = include_bytes!("../../../apps/quire-desktop/fonts/JetBrainsMonoNL-Regular.ttf");
/// STIX Two Math 2.13 (SIL OFL 1.1), an OpenType math font; see
/// `fonts/STIXTwoMath-OFL.txt`.
pub const STIX_TWO_MATH: &[u8] = include_bytes!("../fonts/STIXTwoMath-Regular.otf");
