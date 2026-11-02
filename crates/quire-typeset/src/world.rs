// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A sandboxed Typst [`World`]: embedded fonts, an in-memory file system and
//! nothing else (no packages, no disk, no network).

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use typst::diag::{FileError, FileResult, Severity, SourceDiagnostic};
use typst::foundations::{Bytes, Datetime, Duration};
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};
use typst_layout::PagedDocument;

use crate::TypesetError;
use crate::fonts::{INTER_ITALIC, INTER_REGULAR, INTER_SEMIBOLD, JETBRAINS_MONO_NL, STIX_TWO_MATH};

/// The math font's family name as Typst sees it. (The `.typ` files name
/// `Inter` and `JetBrains Mono NL` directly; a test checks all three.)
pub(crate) const MATH_FONT: &str = "STIX Two Math";

/// The `MiTeX` support scope: Typst definitions for what `mitex` emits.
const MITEX_DEFS: &str = include_str!("mitex-defs.typ");
const MITEX_LIB: &str = include_str!("mitex.typ");

/// Everything that is parsed once and shared by every compilation.
struct Shared {
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    fonts: Vec<Font>,
    mitex_defs: Source,
    mitex_lib: Source,
}

/// Script-coverage fallbacks, the same the desktop app uses: the first file
/// found in each group is loaded (skipped when absent or huge).
const FALLBACK_FONTS: &[&[&str]] = &[
    &["/usr/share/fonts/truetype/noto/NotoSansDevanagari-Regular.ttf", "C:\\Windows\\Fonts\\Nirmala.ttf"],
    &["/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", "C:\\Windows\\Fonts\\msyh.ttc", "/System/Library/Fonts/PingFang.ttc"],
];

fn system_fallbacks() -> Vec<Bytes> {
    FALLBACK_FONTS
        .iter()
        .filter_map(|group| group.iter().find_map(|path| std::fs::read(path).ok()))
        .filter(|data| data.len() < 24 * 1024 * 1024)
        .map(Bytes::new)
        .collect()
}

fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(|| {
        let embedded = [INTER_REGULAR, INTER_SEMIBOLD, INTER_ITALIC, JETBRAINS_MONO_NL, STIX_TWO_MATH].map(Bytes::new);
        let fonts: Vec<Font> = embedded.into_iter().chain(system_fallbacks()).flat_map(Font::iter).collect();
        Shared {
            library: LazyHash::new(Library::default()),
            book: LazyHash::new(FontBook::from_fonts(&fonts)),
            fonts,
            mitex_defs: Source::new(file_id("/mitex-defs.typ"), MITEX_DEFS.to_owned()),
            mitex_lib: Source::new(file_id("/mitex.typ"), MITEX_LIB.to_owned()),
        }
    })
}

/// The interned id of a project-relative virtual path.
pub(crate) fn file_id(path: &str) -> FileId {
    let vpath = VirtualPath::new(path).expect("internal virtual paths are well-formed");
    RootedPath::new(VirtualRoot::Project, vpath).intern()
}

/// One compilation: a main source plus in-memory binary files (images).
pub(crate) struct TypesetWorld {
    shared: &'static Shared,
    main: Source,
    files: HashMap<FileId, Bytes>,
}

impl TypesetWorld {
    pub(crate) fn new(main: String) -> Self {
        Self { shared: shared(), main: Source::new(file_id("/main.typ"), main), files: HashMap::new() }
    }

    /// Makes `data` readable at `path` (e.g. `/img/0.png`).
    pub(crate) fn add_file(&mut self, path: &str, data: Bytes) {
        self.files.insert(file_id(path), data);
    }

    /// Compiles to paged frames. Never panics: a panic inside Typst becomes an error.
    pub(crate) fn compile(&self) -> Result<PagedDocument, TypesetError> {
        let result = catch_unwind(AssertUnwindSafe(|| typst::compile::<PagedDocument>(self).output));
        // Keep Typst's global memoization caches bounded across many compiles.
        typst::comemo::evict(30);
        match result {
            Ok(Ok(doc)) => Ok(doc),
            Ok(Err(errors)) => Err(TypesetError::Typst(describe(&errors))),
            Err(_) => Err(TypesetError::Internal("the typesetter crashed on this input".into())),
        }
    }
}

/// The first error, with its first hint, as one short line.
pub(crate) fn describe(diagnostics: &[SourceDiagnostic]) -> String {
    let Some(diag) = diagnostics.iter().find(|d| d.severity == Severity::Error).or_else(|| diagnostics.first()) else {
        return "unknown error".into();
    };
    let mut message = diag.message.to_string();
    if let Some(hint) = diag.hints.first() {
        message.push_str(" (");
        message.push_str(&hint.v);
        message.push(')');
    }
    message
}

impl World for TypesetWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.shared.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.shared.book
    }

    fn main(&self) -> FileId {
        self.main.id()
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.main.id() {
            Ok(self.main.clone())
        } else if id == self.shared.mitex_defs.id() {
            Ok(self.shared.mitex_defs.clone())
        } else if id == self.shared.mitex_lib.id() {
            Ok(self.shared.mitex_lib.clone())
        } else {
            Err(not_found(id))
        }
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        self.files.get(&id).cloned().ok_or_else(|| not_found(id))
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.shared.fonts.get(index).cloned()
    }

    fn today(&self, offset: Option<Duration>) -> Option<Datetime> {
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs_f64();
        let shifted = secs + offset.map_or(0.0, |d| d.seconds());
        let (year, month, day) = civil_from_days((shifted / 86_400.0).floor() as i64);
        Datetime::from_ymd(i32::try_from(year).ok()?, month, day)
    }
}

fn not_found(id: FileId) -> FileError {
    FileError::NotFound(id.vpath().get_without_slash().into())
}

/// Days since 1970-01-01 to a proleptic Gregorian (year, month, day).
/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u8, u8) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month as u8, day as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn embedded_font_families() {
        let book = &shared().book;
        for family in ["Inter", "JetBrains Mono NL", MATH_FONT] {
            assert!(book.contains_family(&family.to_lowercase()), "missing font family {family}");
        }
    }
}
