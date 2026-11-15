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
//!   for `.git` (a folder, or a file saying `gitdir: …`), as git does, and
//!   reads `commondir` for a linked worktree. A folder on the way that is
//!   itself a git folder (a bare repository, which files in a vault could
//!   make) ends the search with nothing found: git would use that one.
//! - Trust is kept in this computer's Quire settings folder
//!   (`git-trust.toml`), never in the vault, keyed by the canonical paths of
//!   the working tree, its git folder and the common git folder. A copy of a
//!   trusted repository at another path (or on another computer) is not
//!   trusted, and neither is another worktree of it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file trust is kept in, in Quire's settings folder.
pub const FILE: &str = "git-trust.toml";

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
}

/// Read at most this much of a `.git` or `commondir` file.
const SMALL: u64 = 4096;

fn read_small(path: &Path) -> Option<String> {
    use std::io::Read as _;
    let mut s = String::new();
    std::fs::File::open(path).ok()?.take(SMALL).read_to_string(&mut s).ok()?;
    Some(s)
}

/// A git folder: it has `HEAD`, and objects of its own or a `commondir`.
fn looks_like_git_dir(dir: &Path) -> bool {
    dir.join("HEAD").is_file() && (dir.join("objects").is_dir() || dir.join("commondir").is_file())
}

/// The repository `dir` is in (or is the top of), found the way git finds
/// it, without running git. `None` when there is none, or when the `.git`
/// it meets is a file git couldn't use either.
pub fn find_repo(dir: &Path) -> Option<RepoDirs> {
    let start = dir.canonicalize().ok()?;
    for d in start.ancestors() {
        let dot = d.join(".git");
        let git_dir = if dot.is_dir() && looks_like_git_dir(&dot) {
            dot
        } else if dot.is_file() {
            // `gitdir: <path>`; git stops with an error on any other content.
            let text = read_small(&dot)?;
            let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
            if target.is_empty() {
                return None;
            }
            let target = d.join(target);
            if !looks_like_git_dir(&target) {
                return None;
            }
            target
        } else if looks_like_git_dir(d) {
            // A folder that is itself a git folder (a bare repository): git
            // would use it, and its settings, before any repository above.
            // Quire has no use for one, and files placed in a vault can make
            // one, so there is no repository here that Quire will run git in.
            return None;
        } else {
            // No `.git`, or a `.git` folder that isn't a repository: git
            // looks further up.
            continue;
        };
        let git_dir = git_dir.canonicalize().ok()?;
        let common_dir = match read_small(&git_dir.join("commondir")) {
            Some(c) => git_dir.join(c.trim_end_matches(['\n', '\r'])).canonicalize().ok()?,
            None => git_dir.clone(),
        };
        return Some(RepoDirs { root: d.to_path_buf(), git_dir, common_dir });
    }
    None
}

/// `git-trust.toml`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    #[serde(default)]
    trusted: Vec<RepoDirs>,
    /// Keys a newer Quire wrote, kept.
    #[serde(flatten)]
    other: toml::Table,
}

const HEADER: &str = "# Git repositories trusted on this computer: Quire runs git only in these.\n\
                      # Remove an entry (or use \"Stop trusting this repository\" in Quire's Git tab) to stop.\n\n";

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

    /// Whether `repo` is trusted now (read fresh each time, so a change
    /// made in another window counts at once). An unreadable file trusts
    /// nothing.
    pub fn is_trusted(&self, repo: &RepoDirs) -> bool {
        self.load().is_ok_and(|f| f.trusted.contains(repo))
    }

    /// Trust `repo` on this computer.
    pub fn trust(&self, repo: &RepoDirs) -> Result<(), String> {
        let mut f = self.load()?;
        if !f.trusted.contains(repo) {
            f.trusted.push(repo.clone());
            self.save(&f)?;
        }
        Ok(())
    }

    /// Stop trusting `repo`.
    pub fn untrust(&self, repo: &RepoDirs) -> Result<(), String> {
        let mut f = self.load()?;
        let n = f.trusted.len();
        f.trusted.retain(|r| r != repo);
        if f.trusted.len() != n {
            self.save(&f)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_git_dir(dir: &Path) {
        std::fs::create_dir_all(dir.join("objects")).unwrap();
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
        // A `.git` folder that isn't a repository is skipped, as git does.
        std::fs::create_dir_all(top.join("r/notes/.git")).unwrap();
        assert_eq!(find_repo(&top.join("r/notes")).unwrap().root(), top.join("r"));
        // A linked worktree: a `.git` file, and `commondir` in its git folder.
        let wt = top.join("r/.git/worktrees/w");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join("HEAD"), "ref: refs/heads/w\n").unwrap();
        std::fs::write(wt.join("commondir"), "../..\n").unwrap();
        std::fs::create_dir_all(top.join("w")).unwrap();
        std::fs::write(top.join("w/.git"), format!("gitdir: {}\n", wt.display())).unwrap();
        let w = find_repo(&top.join("w")).unwrap();
        assert_eq!((w.root(), w.git_dir(), w.common_dir()), (&*top.join("w"), &*wt, &*top.join("r/.git")));
        // A relative `gitdir:` (a submodule's).
        std::fs::create_dir_all(top.join("r/sub")).unwrap();
        fake_git_dir(&top.join("r/.git/modules/sub"));
        std::fs::write(top.join("r/sub/.git"), "gitdir: ../.git/modules/sub\n").unwrap();
        assert_eq!(find_repo(&top.join("r/sub")).unwrap().git_dir(), top.join("r/.git/modules/sub"));
        // A folder that is itself a git folder, inside a repository: git would
        // use it and its settings, so Quire finds nothing to run git in.
        fake_git_dir(&top.join("r/notes/daily"));
        assert_eq!(find_repo(&top.join("r/notes/daily")), None);
        assert_eq!(find_repo(&top.join("r/notes")).unwrap().root(), top.join("r"));
        // A `.git` file git can't use stops the search, as it stops git.
        std::fs::create_dir_all(top.join("r/bad")).unwrap();
        std::fs::write(top.join("r/bad/.git"), "nonsense\n").unwrap();
        assert_eq!(find_repo(&top.join("r/bad")), None);
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
        // Another store reading the same file agrees (another window).
        assert!(TrustStore::at(top.join("cfg").join(FILE)).is_trusted(&a));
        store.untrust(&a).unwrap();
        assert!(!store.is_trusted(&a));
        // Nowhere to keep it: nothing is trusted, and trusting says why.
        let none = TrustStore::default();
        assert!(!none.is_trusted(&a));
        assert!(none.trust(&a).is_err());
        // An unreadable file trusts nothing and isn't overwritten.
        std::fs::write(top.join("cfg").join(FILE), "not = [toml").unwrap();
        assert!(!store.is_trusted(&a));
        assert!(store.trust(&a).is_err());
        assert_eq!(std::fs::read_to_string(top.join("cfg").join(FILE)).unwrap(), "not = [toml");
    }
}
