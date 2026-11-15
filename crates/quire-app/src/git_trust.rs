// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Which git repositories Quire may run git in, on this computer.
//!
//! A repository's own settings (`.git/config`) and hooks can make git run any
//! program, and they travel with a copied, unpacked or synced folder. No list
//! of "dangerous" settings is complete, so Quire runs no git at all in a
//! repository until you have trusted it here; once trusted, git runs as your
//! own git would, hooks and all.
//!
//! - Finding the repository runs no git: [`find_repo`] walks up from the vault
//!   for `.git` (a folder, or a file saying `gitdir: …`) and reads
//!   `commondir` for a linked worktree, with git's own rules for what a git
//!   folder is. It fails closed: a `.git` of any kind that isn't a git folder
//!   it fully accepts, or a folder on the way that looks like a bare
//!   repository, ends the search with nothing found. Quire then runs git with
//!   that exact git folder and working tree (`--git-dir`, `--work-tree`), so
//!   git does no search of its own.
//! - Trust is kept in this computer's Quire settings folder
//!   (`git-trust.toml`), never in the vault, keyed by the canonical paths of
//!   the working tree, its git folder and the common git folder, and by a
//!   random id that trusting writes into the git folder (`quire-trust`). A
//!   copy of a trusted repository at another path (or on another computer)
//!   is not trusted, nor is another worktree of it, nor a different
//!   repository that later appears at the same path.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file trust is kept in, in Quire's settings folder.
pub const FILE: &str = "git-trust.toml";
/// The file in a trusted repository's git folder that holds its trust id.
pub const ID_FILE: &str = "quire-trust";

/// A repository found without running git. Every path is canonical.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoDirs {
    /// The working tree's top folder.
    root: PathBuf,
    /// This working tree's git folder (`.git`, or where a `.git` file points).
    git_dir: PathBuf,
    /// The folder with the repository's config and hooks: `git_dir`, or the
    /// main repository's for a linked worktree.
    common_dir: PathBuf,
}

impl RepoDirs {
    /// The working tree's top folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    /// The git folder and common folder that lie outside the working tree
    /// (a `.git` file pointing elsewhere), to show before trusting.
    pub fn outside_root(&self) -> Vec<&Path> {
        let mut v: Vec<&Path> = Vec::new();
        for d in [self.git_dir(), self.common_dir()] {
            if !d.starts_with(&self.root) && !v.contains(&d) {
                v.push(d);
            }
        }
        v
    }
}

/// Read at most this much of a `.git`, `HEAD`, `commondir` or id file.
const SMALL: u64 = 4096;

/// The whole of a small file; a larger one is an error.
fn read_capped(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut b = Vec::new();
    std::fs::File::open(path)?.take(SMALL + 1).read_to_end(&mut b)?;
    if b.len() as u64 > SMALL {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "too large"));
    }
    Ok(b)
}

/// A path from a file's bytes, as git uses them (on Windows they must be
/// UTF-8).
#[cfg_attr(unix, expect(clippy::unnecessary_wraps, reason = "None on Windows, for bytes that aren't UTF-8"))]
fn path_from_bytes(b: &[u8]) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(b)))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(b).ok().map(PathBuf::from)
    }
}

/// The path in a one-value file (`commondir`, or a `.git` file after its
/// `gitdir: `): only trailing CR and LF are removed, as git does; empty or
/// any other line break or NUL is refused.
fn clean_value(mut b: &[u8]) -> Option<PathBuf> {
    while let [rest @ .., b'\n' | b'\r'] = b {
        b = rest;
    }
    if b.is_empty() || b.iter().any(|c| matches!(c, b'\n' | b'\r' | 0)) {
        return None;
    }
    path_from_bytes(b)
}

/// Whether something (of any kind, even a dangling symlink) is at `path`.
fn exists_any(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// `HEAD` as git's `validate_headref` accepts it: a symlink into `refs/`,
/// `ref: refs/…`, or an object id.
fn head_ok(dir: &Path) -> bool {
    let head = dir.join("HEAD");
    let Ok(meta) = std::fs::symlink_metadata(&head) else { return false };
    if meta.file_type().is_symlink() && std::fs::read_link(&head).is_ok_and(|t| t.starts_with("refs")) {
        return true;
    }
    let Ok(b) = read_capped(&head) else { return false };
    if let Some(rest) = b.strip_prefix(b"ref:") {
        return rest.trim_ascii_start().starts_with(b"refs/");
    }
    let hex = b.iter().take_while(|c| c.is_ascii_hexdigit()).count();
    hex == 40 || hex == 64
}

/// `dir` as a git folder, as git's `is_git_directory` defines it (a valid
/// `HEAD`, and `objects` and `refs` in its common folder): its canonical
/// path and its common folder's. `None` for anything else, including a
/// `commondir` that can't be read cleanly.
fn git_folder(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    if !head_ok(dir) {
        return None;
    }
    let common = match read_capped(&dir.join("commondir")) {
        Ok(b) => dir.join(clean_value(&b)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => dir.to_path_buf(),
        Err(_) => return None,
    };
    if !common.join("objects").is_dir() || !common.join("refs").is_dir() {
        return None;
    }
    Some((dir.canonicalize().ok()?, common.canonicalize().ok()?))
}

/// A `.git` file's target, parsed as git's `read_gitfile` does: the whole
/// file is `gitdir: <path>`, with only trailing CR and LF removed.
fn gitfile_target(dot: &Path) -> Option<PathBuf> {
    let b = read_capped(dot).ok()?;
    let path = clean_value(b.strip_prefix(b"gitdir: ")?)?;
    Some(dot.parent()?.join(path))
}

/// The repository `dir` is in (or is the top of), found without running git.
///
/// Fails closed: `None` when there is none, and also when what is on
/// the way isn't exactly a git folder this accepts (a `.git` that is broken,
/// a dangling symlink, or a git folder git would read differently), or a
/// folder looks like a bare repository.
pub fn find_repo(dir: &Path) -> Option<RepoDirs> {
    let start = dir.canonicalize().ok()?;
    for d in start.ancestors() {
        let dot = d.join(".git");
        let (git_dir, common_dir) = match std::fs::symlink_metadata(&dot) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A folder that looks like a git folder itself (a bare
                // repository): git would use it before any repository
                // above, and files placed in a vault can make one.
                if exists_any(&d.join("HEAD")) && (exists_any(&d.join("objects")) || exists_any(&d.join("refs"))) {
                    return None;
                }
                continue;
            }
            Err(_) => return None,
            Ok(_) => {
                // Follow a symlink; a dangling one ends the search.
                let meta = std::fs::metadata(&dot).ok()?;
                if meta.is_dir() {
                    git_folder(&dot)?
                } else if meta.is_file() {
                    git_folder(&gitfile_target(&dot)?)?
                } else {
                    return None;
                }
            }
        };
        return Some(RepoDirs { root: d.to_path_buf(), git_dir, common_dir });
    }
    None
}

/// One trusted repository in `git-trust.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
    /// The id written to `<git_dir>/quire-trust` when it was trusted.
    id: String,
}

impl Entry {
    fn is(&self, r: &RepoDirs) -> bool {
        self.root == r.root && self.git_dir == r.git_dir && self.common_dir == r.common_dir
    }
}

/// `git-trust.toml`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    #[serde(default)]
    trusted: Vec<Entry>,
    /// Keys a newer Quire wrote, kept.
    #[serde(flatten)]
    other: toml::Table,
}

const HEADER: &str = "# Git repositories trusted on this computer: Quire runs git only in these.\n\
                      # Remove an entry (or use \"Stop trusting this repository\" in Quire's Git tab) to stop.\n\n";

/// The trust id in `repo`'s git folder, if any.
fn read_id(repo: &RepoDirs) -> Option<String> {
    let b = read_capped(&repo.git_dir.join(ID_FILE)).ok()?;
    let id = std::str::from_utf8(&b).ok()?.trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// Where trust is kept. `Clone` is cheap: it is a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustStore {
    /// `None`: nowhere to keep it (no settings folder, or a unit test), so
    /// nothing is trusted.
    path: Option<PathBuf>,
}

impl Default for TrustStore {
    /// This computer's: `git-trust.toml` in Quire's settings folder. Unit
    /// tests get none, so they never read or write real trust.
    fn default() -> Self {
        Self { path: crate::settings::config_dir().filter(|_| !cfg!(test)).map(|d| d.join(FILE)) }
    }
}

impl TrustStore {
    /// Trust kept in `path` (tests, and a settings folder chosen elsewhere).
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: Some(path.into()) }
    }

    fn load(&self) -> Result<TrustFile, String> {
        let Some(path) = &self.path else { return Ok(TrustFile::default()) };
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{} can't be read: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TrustFile::default()),
            Err(e) => Err(format!("{} can't be read: {e}", path.display())),
        }
    }

    fn save(&self, file: &TrustFile) -> Result<(), String> {
        let path = self.path.as_ref().ok_or("Quire has no settings folder on this computer to remember that in")?;
        let body = toml::to_string(file).map_err(|e| format!("Couldn't write the list of trusted repositories: {e}"))?;
        crate::atomic::write(path, format!("{HEADER}{body}").as_bytes(), false)
            .map_err(|e| format!("Couldn't write {}: {e}", path.display()))
    }

    /// Whether `repo` is trusted now: its paths are listed, and its git
    /// folder still holds the id written when it was trusted (so a different
    /// repository at the same path isn't). Read fresh each time, so a change
    /// made in another window counts at once. An unreadable file trusts
    /// nothing.
    pub fn is_trusted(&self, repo: &RepoDirs) -> bool {
        let Some(id) = read_id(repo) else { return false };
        self.load().is_ok_and(|f| f.trusted.iter().any(|e| e.is(repo) && e.id == id))
    }

    /// Trust `repo` on this computer: a new random id goes into its git
    /// folder and into the list.
    pub fn trust(&self, repo: &RepoDirs) -> Result<(), String> {
        if self.path.is_none() {
            return Err("Quire has no settings folder on this computer to remember that in".into());
        }
        let mut f = self.load()?;
        if self.is_trusted(repo) {
            return Ok(());
        }
        let id = quire_sync::FileId::random().map_err(|e| format!("Couldn't make a random id: {e}"))?.0;
        let id_path = repo.git_dir.join(ID_FILE);
        crate::atomic::write(&id_path, format!("{id}\n").as_bytes(), false)
            .map_err(|e| format!("Couldn't write {}: {e}", id_path.display()))?;
        f.trusted.retain(|e| !e.is(repo));
        f.trusted.push(Entry { root: repo.root.clone(), git_dir: repo.git_dir.clone(), common_dir: repo.common_dir.clone(), id });
        self.save(&f)
    }

    /// Stop trusting `repo` (its id file goes too, when it is the one listed).
    pub fn untrust(&self, repo: &RepoDirs) -> Result<(), String> {
        let mut f = self.load()?;
        let (gone, kept): (Vec<Entry>, Vec<Entry>) = std::mem::take(&mut f.trusted).into_iter().partition(|e| e.is(repo));
        f.trusted = kept;
        if gone.is_empty() {
            return Ok(());
        }
        self.save(&f)?;
        if read_id(repo).is_some_and(|id| gone.iter().any(|e| e.id == id)) {
            let _ = std::fs::remove_file(repo.git_dir.join(ID_FILE));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_git_dir(dir: &Path) {
        std::fs::create_dir_all(dir.join("objects")).unwrap();
        std::fs::create_dir_all(dir.join("refs/heads")).unwrap();
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    #[test]
    fn repositories_are_found_without_git() {
        let t = tempfile::tempdir().unwrap();
        let top = t.path().canonicalize().unwrap();
        // Not a repository.
        std::fs::create_dir_all(top.join("plain/notes")).unwrap();
        assert_eq!(find_repo(&top.join("plain/notes")), None);
        // A `.git` folder, from a folder below the top.
        fake_git_dir(&top.join("r/.git"));
        std::fs::create_dir_all(top.join("r/notes/daily")).unwrap();
        let r = find_repo(&top.join("r/notes/daily")).unwrap();
        assert_eq!((r.root(), r.git_dir(), r.common_dir()), (&*top.join("r"), &*top.join("r/.git"), &*top.join("r/.git")));
        assert!(r.outside_root().is_empty());
        // A linked worktree: a `.git` file, and `commondir` in its git folder.
        let wt = top.join("r/.git/worktrees/w");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join("HEAD"), "ref: refs/heads/w\n").unwrap();
        std::fs::write(wt.join("commondir"), "../..\n").unwrap();
        std::fs::create_dir_all(top.join("w")).unwrap();
        std::fs::write(top.join("w/.git"), format!("gitdir: {}\n", wt.display())).unwrap();
        let w = find_repo(&top.join("w")).unwrap();
        assert_eq!((w.root(), w.git_dir(), w.common_dir()), (&*top.join("w"), &*wt, &*top.join("r/.git")));
        assert_eq!(w.outside_root(), [&*wt, &*top.join("r/.git")]);
        // A relative `gitdir:` (a submodule's).
        std::fs::create_dir_all(top.join("r/sub")).unwrap();
        fake_git_dir(&top.join("r/.git/modules/sub"));
        std::fs::write(top.join("r/sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
        assert_eq!(find_repo(&top.join("r/sub")).unwrap().git_dir(), top.join("r/.git/modules/sub"));
        // An object id in HEAD (a detached HEAD) is a valid HEAD.
        std::fs::write(top.join("r/.git/modules/sub/HEAD"), format!("{}\n", "a".repeat(40))).unwrap();
        assert!(find_repo(&top.join("r/sub")).is_some());
        // A folder that is itself a git folder, inside a repository: git would
        // use it and its settings, so Quire finds nothing to run git in.
        fake_git_dir(&top.join("r/notes/daily"));
        assert_eq!(find_repo(&top.join("r/notes/daily")), None);
        assert_eq!(find_repo(&top.join("r/notes")).unwrap().root(), top.join("r"));
    }

    /// Anything on the way that isn't exactly a git folder this accepts ends
    /// the search (findings 1, 4, 5 and 7 of the trust re-review): git would
    /// either use it or read it differently.
    #[test]
    fn find_repo_fails_closed() {
        let t = tempfile::tempdir().unwrap();
        let top = t.path().canonicalize().unwrap();
        fake_git_dir(&top.join("r/.git"));
        let case = |name: &str, make: &dyn Fn(&Path)| {
            let d = top.join("r").join(name);
            std::fs::create_dir_all(&d).unwrap();
            make(&d);
            assert_eq!(find_repo(&d), None, "{name}");
        };
        // 5: a `.git` folder without `refs`, or with junk in HEAD, or empty.
        case("no-refs", &|d| {
            std::fs::create_dir_all(d.join(".git/objects")).unwrap();
            std::fs::write(d.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        });
        case("junk-head", &|d| {
            fake_git_dir(&d.join(".git"));
            std::fs::write(d.join(".git/HEAD"), "junk\n").unwrap();
        });
        case("empty-dot-git", &|d| std::fs::create_dir_all(d.join(".git")).unwrap());
        // 4: a `.git` file git reads differently: extra spaces, no space, two lines.
        for (name, text) in [
            ("two-spaces", "gitdir:  ../../.git\n"),
            ("trailing-space", "gitdir: ../../.git \n"),
            ("no-space", "gitdir:../../.git\n"),
            ("two-lines", "gitdir: ../../.git\nmore\n"),
            ("empty", "gitdir: \n"),
        ] {
            case(name, &|d| std::fs::write(d.join(".git"), text).unwrap());
        }
        // 7: `commondir` that can't be read cleanly.
        case("commondir-dir", &|d| {
            fake_git_dir(&d.join(".git"));
            std::fs::create_dir_all(d.join(".git/commondir")).unwrap();
        });
        case("commondir-empty", &|d| {
            fake_git_dir(&d.join(".git"));
            std::fs::write(d.join(".git/commondir"), "\n").unwrap();
        });
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            // 1: a dangling `.git`, and HEAD as a symlink to a branch not born yet
            // (git accepts that HEAD; so must this, and the folder isn't trusted).
            case("dangling-dot-git", &|d| symlink(d.join("nowhere"), d.join(".git")).unwrap());
            let d = top.join("r/unborn");
            std::fs::create_dir_all(d.join(".git/objects")).unwrap();
            std::fs::create_dir_all(d.join(".git/refs/heads")).unwrap();
            symlink("refs/heads/nothere", d.join(".git/HEAD")).unwrap();
            assert_eq!(find_repo(&d).unwrap().root(), d, "found as its own repository, not the one above");
            // The bare variant: a dangling HEAD next to `objects` or `refs`.
            case("bare-dangling-head", &|d| {
                std::fs::create_dir_all(d.join("refs")).unwrap();
                symlink("refs/heads/nothere", d.join("HEAD")).unwrap();
            });
        }
        // 7: a commondir that isn't UTF-8 is used as bytes, as git does. (Linux
        // only: macOS file systems refuse a name that isn't UTF-8.)
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::ffi::OsStrExt as _;
            let d = top.join("r/bytes");
            fake_git_dir(&d.join(".git"));
            let raw = std::ffi::OsStr::from_bytes(b"other\xff");
            fake_git_dir(&top.join("r").join(raw));
            std::fs::write(d.join(".git/commondir"), b"../../other\xff\n").unwrap();
            assert_eq!(find_repo(&d).unwrap().common_dir(), top.join("r").join(raw));
        }
        // A plain `HEAD` file and `refs` folder in a notes folder: a bare repository to git.
        case("bare-head-file", &|d| {
            std::fs::write(d.join("HEAD"), "x").unwrap();
            std::fs::create_dir_all(d.join("objects")).unwrap();
        });
    }

    #[test]
    fn trust_is_kept_per_repository() {
        let t = tempfile::tempdir().unwrap();
        let top = t.path().canonicalize().unwrap();
        fake_git_dir(&top.join("a/.git"));
        fake_git_dir(&top.join("b/.git"));
        let (a, b) = (find_repo(&top.join("a")).unwrap(), find_repo(&top.join("b")).unwrap());
        let store = TrustStore::at(top.join("cfg").join(FILE));
        assert!(!store.is_trusted(&a));
        store.trust(&a).unwrap();
        store.trust(&a).unwrap();
        assert!(store.is_trusted(&a) && !store.is_trusted(&b));
        let text = std::fs::read_to_string(top.join("cfg").join(FILE)).unwrap();
        assert_eq!(text.matches("[[trusted]]").count(), 1, "{text}");
        let id = std::fs::read_to_string(top.join("a/.git").join(ID_FILE)).unwrap();
        assert!(text.contains(id.trim()));
        // Another store reading the same file agrees (another window).
        assert!(TrustStore::at(top.join("cfg").join(FILE)).is_trusted(&a));
        // 3: a different repository at the same path (its git folder has no
        // id, or another one) isn't trusted.
        std::fs::remove_dir_all(top.join("a/.git")).unwrap();
        fake_git_dir(&top.join("a/.git"));
        let a2 = find_repo(&top.join("a")).unwrap();
        assert_eq!(a2, a);
        assert!(!store.is_trusted(&a2));
        std::fs::write(top.join("a/.git").join(ID_FILE), "guessed\n").unwrap();
        assert!(!store.is_trusted(&a2));
        // Trusting it again replaces the entry.
        store.trust(&a2).unwrap();
        assert!(store.is_trusted(&a2));
        let text = std::fs::read_to_string(top.join("cfg").join(FILE)).unwrap();
        assert_eq!(text.matches("[[trusted]]").count(), 1, "{text}");
        store.untrust(&a).unwrap();
        assert!(!store.is_trusted(&a));
        assert!(!top.join("a/.git").join(ID_FILE).exists());
        // Nowhere to keep it: nothing is trusted, and trusting says why.
        let none = TrustStore::default();
        assert!(!none.is_trusted(&a));
        assert!(none.trust(&a).is_err());
        assert!(!top.join("a/.git").join(ID_FILE).exists());
        // An unreadable file trusts nothing and isn't overwritten.
        std::fs::write(top.join("cfg").join(FILE), "not = [toml").unwrap();
        assert!(!store.is_trusted(&a));
        assert!(store.trust(&a).is_err());
        assert_eq!(std::fs::read_to_string(top.join("cfg").join(FILE)).unwrap(), "not = [toml");
    }
}
