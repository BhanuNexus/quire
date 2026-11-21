// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Which notes an agent may read, as one list of rules, checked against `AgentStore::effective_mode`
//! (`docs/design/0.14-agent-tools.md` §6, the first risk: a note that `Visibility` shows and
//! `effective` hides is a leak). Written before the step that adds the type (the first build step).
//!
//! It checks a small reference of the rules written here, in the test, against `effective_mode`
//! over generated vaults and policies. That makes the reference the executable statement of the
//! rules, and it proves the rules below are the whole of what `effective_mode` does for a read, for
//! the inputs the generator covers. `quire_agent::Visibility` is a second implementation of
//! [`ReadRules`], and the same cases run through it: against the reference and against
//! `effective_mode` directly, with the locked folders given up front and found lazily.
//!
//! What the generator covers: hidden folders; encrypted files and key files; a locked folder
//! (a key file in the folder or above it); `deny`, `read` and `direct` zones as folder prefixes
//! and one glob, most specific wins (a file with no zones keeps the default `wiki/` and `raw/`);
//! the policy default (including `deny`); `ai = "off"`; a revoked client; a client's folder and
//! tag allow list (front matter tags, with `work/q4` counting as `work`); a client's own zones and
//! `default`; the client verified (`--client`) or unverified (a reported name: its own zones can
//! narrow but never widen); and a policy file the person has not approved on top of the one they
//! did (both apply, the stricter wins). What it does not cover: links (they need a real folder;
//! `a_note_below_a_symlinked_folder_is_hidden` checks `Visibility` on one), a grant cap (it
//! narrows writes, never reads), and notes whose tags changed on disk but are not indexed yet.
//!
//! [`equal_fingerprints_give_equal_answers`]: over random policies, two callers whose
//! `Visibility::fingerprint` is equal get the same answer for every path.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    clippy::disallowed_methods,
    reason = "an integration test: its tests sit at the top level, and its helpers unwrap or panic to fail the test; it builds a vault with std directly"
)]

use std::collections::BTreeMap;
use std::fmt::Write as _;

use quire_agent::{Actor, AgentStore, DeviceInfo, FixedClock, Mode, OpKind, Via, Visibility};
use quire_vault::{NotePath, Vault};

/// A deterministic generator: the same cases on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() >> 11) as usize % n
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

const FOLDERS: &[&str] = &[
    "",
    "Notes",
    "Private",
    "Work",
    "Work/Public",
    "Projects",
    "Journal",
    "wiki",
    "raw",
    "Arch/Old",
    "Café",
    ".obsidian",
    ".trash",
    "Notes/.cache",
];
const TAGS: &[&str] = &["work", "work/q4", "home", "plan", "Work"];
const MODES: &[&str] = &["deny", "read", "propose", "direct"];
/// Zone patterns the generator draws from: folder prefixes and one glob each of two shapes.
const PATTERNS: &[&str] = &["Private/", "Work/", "Work/Public/", "Projects/", "Journal/", "Arch/", "**/secret-*.md", "Notes/*.log"];
const ALLOW_FOLDERS: &[&str] = &["Work/", "Projects", "wiki/", "Notes/"];

/// A generated vault, its policy and what the rules say about each of its paths.
struct Case {
    /// Every path the case asks about, with its tags (front matter).
    paths: Vec<(String, Vec<String>)>,
    /// The vault's files, with their text.
    files: Vec<(String, String)>,
    /// The policy files: the first is approved on this device, a second (if any) is written after
    /// it without approval.
    policies: Vec<String>,
    rules: Rules,
}

/// The reference: every way a read can be denied, written out once.
struct Rules {
    /// The policies in force: an unapproved file and the approved one both apply.
    policies: Vec<PolicyRules>,
    /// Folders that hold a key file, as the vault's files say.
    key_folders: Vec<String>,
}

/// A client's own `default` and zones.
type Entry = (Option<Mode>, Vec<(String, Mode)>);

/// What one policy file says about reads.
#[derive(Clone)]
struct PolicyRules {
    ai_off: bool,
    revoked: bool,
    default: Mode,
    zones: Vec<(String, Mode)>,
    allow_folders: Option<Vec<String>>,
    allow_tags: Option<Vec<String>>,
    /// The client's own entry: its `default` and its zones (sorted by pattern, as TOML keeps them).
    entry: Option<Entry>,
}

/// What a set of rules says about a path: the reference ([`Rules`]) and `Visibility`.
trait ReadRules {
    fn allows(&self, path: &str, tags: &[String]) -> bool;
}

impl ReadRules for Visibility {
    fn allows(&self, path: &str, tags: &[String]) -> bool {
        self.allows_with_tags(&NotePath::new(path), tags)
    }
}

fn mode(name: &str) -> Mode {
    match name {
        "deny" => Mode::Deny,
        "read" => Mode::Read,
        "propose" => Mode::Propose,
        "direct" => Mode::Direct,
        other => panic!("no mode {other}"),
    }
}

/// The lowercase literal part of a zone pattern (up to its first glob character) and whether the
/// whole pattern is literal: the order in which zones compete.
fn specificity(pattern: &str) -> (usize, bool) {
    let literal = pattern.find(['*', '?', '[']).unwrap_or(pattern.len());
    (literal, literal == pattern.len())
}

/// Segment-wise glob over lowercase text: `**` matches any number of segments, `*` any part of one.
fn glob(pat: &[&str], segs: &[&str]) -> bool {
    fn seg(p: &[u8], s: &[u8]) -> bool {
        match p.split_first() {
            None => s.is_empty(),
            Some((b'*', rest)) => (0..=s.len()).any(|i| seg(rest, &s[i..])),
            Some((c, rest)) => s.first() == Some(c) && seg(rest, &s[1..]),
        }
    }
    match pat.split_first() {
        None => segs.is_empty(),
        Some((&"**", rest)) => (0..=segs.len()).any(|i| glob(rest, &segs[i..])),
        Some((p, rest)) => segs.split_first().is_some_and(|(s, tail)| seg(p.as_bytes(), s.as_bytes()) && glob(rest, tail)),
    }
}

fn zone_matches(pattern: &str, path: &str) -> bool {
    let (pattern, path) = (pattern.to_lowercase(), path.to_lowercase());
    if pattern.contains(['*', '?']) {
        glob(&pattern.split('/').collect::<Vec<_>>(), &path.split('/').collect::<Vec<_>>())
    } else {
        path.starts_with(&pattern)
    }
}

fn has_tag(tags: &[String], want: &str) -> bool {
    let want = want.trim_start_matches('#').to_lowercase();
    tags.iter().any(|t| {
        let t = t.trim_start_matches('#').to_lowercase();
        t == want || t.strip_prefix(&want).is_some_and(|rest| rest.starts_with('/'))
    })
}

/// The zone among `zones` that governs `path`: the longest literal prefix, a literal before a glob,
/// the later one at a tie (`max_by_key`).
fn governing<'a>(zones: &'a [(String, Mode)], path: &str) -> Option<&'a (String, Mode)> {
    zones.iter().filter(|(p, _)| zone_matches(p, path)).max_by_key(|(p, _)| specificity(&p.to_lowercase()))
}

impl PolicyRules {
    /// Whether this policy lets the client read `path`; `unverified`: the client only reported its
    /// name, so its own zones can narrow but never widen.
    fn allows(&self, path: &str, tags: &[String], unverified: bool) -> bool {
        if self.ai_off || self.revoked {
            return false;
        }
        if self.allow_folders.is_some() || self.allow_tags.is_some() {
            let in_folder = self.allow_folders.iter().flatten().any(|f| zone_matches(&format!("{}/", f.trim_end_matches('/')), path));
            let tagged = self.allow_tags.iter().flatten().any(|w| has_tag(tags, w));
            if !in_folder && !tagged {
                return false;
            }
        }
        let vault = governing(&self.zones, path);
        let vault_mode = vault.map_or(self.default, |(_, m)| *m);
        let modes: Vec<Mode> = match &self.entry {
            None => vec![vault_mode],
            Some((default, own)) => match governing(own, path) {
                // A verified client's own zone wins when it is at least as specific.
                Some((p, m))
                    if !unverified && vault.is_none_or(|(v, _)| specificity(&p.to_lowercase()) >= specificity(&v.to_lowercase())) =>
                {
                    vec![*m]
                }
                other => {
                    let mut v = vec![vault_mode];
                    v.extend(*default);
                    v.extend(other.filter(|_| unverified).map(|(_, m)| *m));
                    v
                }
            },
        };
        !modes.contains(&Mode::Deny)
    }
}

impl Rules {
    fn allows_as(&self, path: &str, tags: &[String], unverified: bool) -> bool {
        let segs: Vec<&str> = path.split('/').collect();
        let (name, folders) = segs.split_last().map(|(n, f)| (*n, f)).unwrap();
        // Hidden folders, and `.quire` with them.
        if folders.iter().any(|s| s.starts_with('.')) {
            return false;
        }
        // Encrypted files and key files, anywhere.
        if name.to_lowercase().ends_with(".qenc") || name == "quire-lock.json" || name == "quire-lock.unlocking.json" {
            return false;
        }
        // A locked folder: a key file in the file's folder or in one above it.
        let mut prefix = String::new();
        for seg in folders {
            prefix.push_str(seg);
            if self.key_folders.contains(&prefix) {
                return false;
            }
            prefix.push('/');
        }
        self.policies.iter().all(|p| p.allows(path, tags, unverified))
    }
}

/// The reference for one client: verified or not.
struct As<'a>(&'a Rules, bool);

impl ReadRules for As<'_> {
    fn allows(&self, path: &str, tags: &[String]) -> bool {
        self.0.allows_as(path, tags, self.1)
    }
}

/// One generated policy file and what it says.
fn generate_policy(rng: &mut Rng) -> (String, PolicyRules) {
    let default = *rng.pick(&["read", "read", "propose", "direct", "deny"]);
    let mut zones: Vec<(String, Mode)> = Vec::new();
    let mut policy = format!("format = 1\ndefault = \"{default}\"\n");
    let ai_off = rng.chance(5);
    if ai_off {
        policy.push_str("ai = \"off\"\n");
    }
    let mut globs = 0;
    for _ in 0..rng.below(4) {
        let pattern = *rng.pick(PATTERNS);
        if zones.iter().any(|(p, _)| p == pattern) || (pattern.contains('*') && globs > 0) {
            continue;
        }
        globs += usize::from(pattern.contains('*'));
        let m = *rng.pick(MODES);
        write!(policy, "[[zone]]\npath = \"{pattern}\"\nrole = \"agent\"\nmode = \"{m}\"\n").expect("a String takes any text");
        zones.push((pattern.to_string(), mode(m)));
    }
    // A policy file without `[[zone]]` tables keeps the default zones (`Policy::default`).
    if zones.is_empty() {
        zones = vec![("wiki/".into(), Mode::Direct), ("raw/".into(), Mode::CreateOnly)];
    }
    let revoked = rng.chance(5);
    let allow_folders: Option<Vec<String>> =
        rng.chance(35).then(|| (0..=rng.below(2)).map(|_| (*rng.pick(ALLOW_FOLDERS)).to_string()).collect());
    let allow_tags: Option<Vec<String>> = rng.chance(35).then(|| vec![(*rng.pick(&["work", "plan", "#home"])).to_string()]);
    // The client's own default and zones.
    let own_default = rng.chance(20).then(|| *rng.pick(MODES));
    let mut own_zones: BTreeMap<String, &str> = BTreeMap::new();
    if rng.chance(40) {
        for _ in 0..=rng.below(2) {
            own_zones.insert((*rng.pick(PATTERNS)).to_string(), *rng.pick(MODES));
        }
    }
    let has_entry = revoked || allow_folders.is_some() || allow_tags.is_some() || own_default.is_some() || !own_zones.is_empty();
    if has_entry {
        policy.push_str("[clients.\"mcp:probe\"]\n");
        if revoked {
            policy.push_str("revoked = true\n");
        }
        let list = |v: &[String]| v.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ");
        if let Some(f) = &allow_folders {
            writeln!(policy, "allow_folders = [{}]", list(f)).expect("a String takes any text");
        }
        if let Some(t) = &allow_tags {
            writeln!(policy, "allow_tags = [{}]", list(t)).expect("a String takes any text");
        }
        if let Some(d) = own_default {
            writeln!(policy, "default = \"{d}\"").expect("a String takes any text");
        }
        if !own_zones.is_empty() {
            policy.push_str("[clients.\"mcp:probe\".zones]\n");
            for (p, m) in &own_zones {
                writeln!(policy, "\"{p}\" = \"{m}\"").expect("a String takes any text");
            }
        }
    }
    let entry = has_entry.then(|| (own_default.map(mode), own_zones.iter().map(|(p, m)| (p.clone(), mode(m))).collect()));
    (policy, PolicyRules { ai_off, revoked, default: mode(default), zones, allow_folders, allow_tags, entry })
}

fn generate(rng: &mut Rng) -> Case {
    let mut files: Vec<(String, String)> = Vec::new();
    let mut paths: Vec<(String, Vec<String>)> = Vec::new();
    let mut key_folders: Vec<String> = Vec::new();
    for i in 0..28 {
        let folder = *rng.pick(FOLDERS);
        let at = |name: String| if folder.is_empty() { name } else { format!("{folder}/{name}") };
        match rng.below(10) {
            // A key file: the folder is locked (a folder inside a hidden one is not a locked folder).
            0 if !folder.is_empty() && !key_folders.iter().any(|k| k == folder) => {
                let name = if rng.chance(30) { "quire-lock.unlocking.json" } else { "quire-lock.json" };
                files.push((at(name.to_string()), "{}".into()));
                if !folder.split('/').any(|s| s.starts_with('.')) {
                    key_folders.push(folder.to_string());
                }
            }
            1 => paths.push((at(format!("x{i}.md.qenc")), Vec::new())),
            2 => paths.push((at(format!("pic{i}.png")), Vec::new())),
            3 => paths.push((at(format!("secret-{i}.md")), Vec::new())),
            4 => paths.push((at(format!("n{i}.log")), Vec::new())),
            _ => {
                let tags: Vec<String> = (0..rng.below(3)).map(|_| (*rng.pick(TAGS)).to_string()).collect();
                let front = if tags.is_empty() { String::new() } else { format!("---\ntags: [{}]\n---\n", tags.join(", ")) };
                let path = at(format!("Note {i}.md"));
                files.push((path.clone(), format!("{front}# Note {i}\n\nbody\n")));
                paths.push((path, tags));
            }
        }
    }
    // A note that does not exist, to be asked about as well.
    paths.push(("Work/missing.md".into(), Vec::new()));
    paths.extend(files.iter().filter(|(p, _)| p.contains("quire-lock")).map(|(p, _)| (p.clone(), Vec::new())));

    let (first, first_rules) = generate_policy(rng);
    let (mut policies, mut rules) = (vec![first], vec![first_rules]);
    // A policy file changed outside Quire and not approved here: it and the approved one both apply.
    if rng.chance(25) {
        let (second, second_rules) = generate_policy(rng);
        policies.push(second);
        rules.insert(0, second_rules);
    }
    Case { paths, files, policies, rules: Rules { policies: rules, key_folders } }
}

/// The policies as the case has them (the first approved, a second written after it without
/// approval), on a store over the generated vault.
fn store(case: &Case, state: &std::path::Path) -> AgentStore {
    store_with(case, &case.policies, state)
}

/// [`store`] with other policy files.
fn store_with(case: &Case, policies: &[String], state: &std::path::Path) -> AgentStore {
    // Key files last, the deepest first: the in-memory storage refuses plain writes into a folder
    // once it is locked.
    let mut files: Vec<(&str, &str)> = case.files.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    files.sort_by_key(|(p, _)| {
        let key = p.rsplit('/').next().is_some_and(|n| n.starts_with("quire-lock"));
        (key, std::cmp::Reverse(p.matches('/').count()))
    });
    let vault = Vault::in_memory(&files);
    let store = AgentStore::open(&vault, DeviceInfo { id: "d_rules".into(), name: "t".into() }, FixedClock::new(1_793_577_600_000));
    store.set_state_dir(Some(state.to_path_buf()));
    store.write_policy(&policies[0]).unwrap_or_else(|e| panic!("{e}\n{}", policies[0]));
    for unapproved in &policies[1..] {
        vault.storage().write_atomic(&NotePath::new(".quire/shared/agent-policy.toml"), unapproved).unwrap();
    }
    store
}

#[cfg(unix)]
/// [`store_with`] for a vault on disk in `dir`, with `linked` (a folder of the case) made a
/// symlink to a copy of itself: its notes are reached only through the link.
fn store_on_disk(case: &Case, policy: &str, dir: &std::path::Path, linked: Option<&str>) -> AgentStore {
    let root = dir.join("vault");
    for (p, t) in &case.files {
        std::fs::create_dir_all(root.join(p).parent().unwrap()).unwrap();
        std::fs::write(root.join(p), t).unwrap();
    }
    if let Some(folder) = linked {
        std::fs::create_dir_all(root.join(folder)).unwrap();
        std::fs::rename(root.join(folder), root.join(format!("{folder}-real"))).unwrap();
        std::os::unix::fs::symlink(root.join(format!("{folder}-real")), root.join(folder)).unwrap();
    }
    let vault = Vault::open_local(&root).unwrap();
    let store = AgentStore::open(&vault, DeviceInfo { id: "d_rules".into(), name: "t".into() }, FixedClock::new(1_793_577_600_000));
    store.set_state_dir(Some(dir.join("state")));
    store.write_policy(policy).unwrap_or_else(|e| panic!("{e}\n{policy}"));
    store
}

/// Every folder the generator can use: what a listing would hand `AgentStore::visibility`.
fn all_folders() -> impl Iterator<Item = &'static str> {
    FOLDERS.iter().copied()
}

#[test]
fn the_read_rules_are_what_effective_mode_does() {
    let state = tempfile::tempdir().unwrap();
    let mut rng = Rng(0x5EED_CAFE_F00D_0001);
    let (mut checked, mut shown, mut hidden, mut unapproved, mut unverified_cases) = (0, 0, 0, 0, 0);
    for case_no in 0..200 {
        let case = generate(&mut rng);
        let store = store(&case, state.path());
        // Verified (`--client probe`) or unverified (it reported the name "probe").
        let unverified = rng.chance(50);
        let actor = if unverified {
            Actor::unverified_agent("mcp:probe", "probe", Via::McpStdio, "d_rules")
        } else {
            Actor::agent("mcp:probe", "probe", Via::McpStdio, "d_rules")
        };
        unapproved += usize::from(case.policies.len() > 1);
        unverified_cases += usize::from(unverified);
        let reference = As(&case.rules, unverified);
        let policy = case.policies.join("--- (not approved above, approved below)\n");
        // The locked folders checked up front, and found as paths are asked about.
        let eager = store.visibility(&actor, all_folders());
        let lazy = store.visibility(&actor, []);
        for (path, tags) in &case.paths {
            let want = reference.allows(path, tags);
            let got = store.effective_mode(&actor, &NotePath::new(path.as_str()), OpKind::Read) != Mode::Deny;
            for (how, vis) in [("given the folders", &eager), ("finding the folders", &lazy)] {
                let seen = ReadRules::allows(vis, path, tags);
                assert_eq!(
                    seen,
                    got,
                    "case {case_no}: Visibility ({how}) says {path} ({tags:?}) is {}, effective_mode says {}\npolicy:\n{}key folders: {:?}",
                    if seen { "readable" } else { "hidden" },
                    if got { "readable" } else { "hidden" },
                    policy,
                    case.rules.key_folders
                );
            }
            assert_eq!(
                want,
                got,
                "case {case_no} ({}): the rules say {path} ({tags:?}) is {}, effective_mode says {}\npolicy:\n{}key folders: {:?}",
                actor.id,
                if want { "readable" } else { "hidden" },
                if got { "readable" } else { "hidden" },
                policy,
                case.rules.key_folders
            );
            checked += 1;
            shown += usize::from(got);
            hidden += usize::from(!got);
        }
    }
    // The generator must reach both answers often, and each kind of case, or the test proves nothing.
    assert!(shown > checked / 10 && hidden > checked / 10, "{checked} paths: {shown} readable, {hidden} hidden");
    assert!(unapproved > 20 && unverified_cases > 50, "{unapproved} cases with an unapproved policy, {unverified_cases} unverified");
}

/// The same policy with every mode that does not hide a note changed for another one that does
/// not: the read answers stay, so the fingerprint must too.
fn other_writes(policy: &str) -> String {
    policy
        .lines()
        .map(|l| {
            let swap = |l: &str, key: &str| {
                for (from, to) in [("\"read\"", "\"direct\""), ("\"propose\"", "\"read\""), ("\"direct\"", "\"propose\"")] {
                    if l.starts_with(key) && l.ends_with(from) {
                        return l.replace(from, to);
                    }
                }
                l.to_string()
            };
            swap(&swap(l, "mode ="), "default =")
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// `docs/design/0.14-agent-tools.md` §6, the first risk: two callers with equal fingerprints get
/// equal answers for every path. Over random policies, variants that change only write modes, two
/// clients (one with an entry in the policy, verified and unverified, one without), policies not
/// approved on top of approved ones, and the vaults' locked folders.
#[test]
fn equal_fingerprints_give_equal_answers() {
    let state = tempfile::tempdir().unwrap();
    let mut rng = Rng(0x5EED_CAFE_F00D_0002);
    let actors = [
        Actor::agent("mcp:probe", "probe", Via::McpStdio, "d_rules"),
        Actor::unverified_agent("mcp:probe", "probe", Via::McpStdio, "d_rules"),
        Actor::agent("mcp:other", "other", Via::McpStdio, "d_rules"),
    ];
    let cases: Vec<Case> = (0..80).map(|_| generate(&mut rng)).collect();
    // Every path any case asks about, with its tags: each fingerprint group answers all of them.
    let mut probes: Vec<(String, Vec<String>)> = cases.iter().flat_map(|c| c.paths.iter().cloned()).collect();
    probes.sort();
    probes.dedup();
    let mut groups: BTreeMap<[u8; 32], Vec<(String, Visibility)>> = BTreeMap::new();
    for (i, case) in cases.iter().enumerate() {
        for policies in [case.policies.clone(), case.policies.iter().map(|p| other_writes(p)).collect()] {
            let store = store_with(case, &policies, state.path());
            let policy = policies.join("--- (not approved above, approved below)\n");
            for actor in &actors {
                let vis = store.visibility(actor, all_folders());
                groups.entry(vis.fingerprint()).or_default().push((format!("case {i}, {}, policy:\n{policy}", actor.id), vis));
            }
        }
    }
    let mut shared = 0;
    for members in groups.values() {
        let Some(((first_how, first), rest)) = members.split_first() else { continue };
        for (how, vis) in rest {
            shared += 1;
            for (path, tags) in &probes {
                assert_eq!(
                    ReadRules::allows(first, path, tags),
                    ReadRules::allows(vis, path, tags),
                    "equal fingerprints, different answers for {path} ({tags:?}):\n{first_how}\n---\n{how}"
                );
            }
        }
    }
    // Write-only variants and clients without an entry must share fingerprints often, and
    // different read rules must not all collapse into one.
    assert!(shared > 100, "only {shared} callers shared a fingerprint");
    assert!(groups.len() > 60, "only {} fingerprints", groups.len());
}

/// Review B1 (`docs/design/0.14-agent-tools.md` §3.5): a folder another handle on the vault locks
/// (another process: only its key file appears) is hidden from this store's next `Visibility` at
/// once, the folders inside it too, with no event and nothing rebuilt; the value made before keeps
/// its answers for the rest of its call; unlocking shows the notes again.
#[test]
fn a_folder_locked_by_another_handle_hides_from_the_next_visibility() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vault");
    for p in ["Work/a.md", "Work/Deep/b.md", "Notes/c.md"] {
        std::fs::create_dir_all(root.join(p).parent().unwrap()).unwrap();
        std::fs::write(root.join(p), "# n\n").unwrap();
    }
    let ours = Vault::open_local(&root).unwrap();
    let theirs = Vault::open_local(&root).unwrap();
    let store = AgentStore::open(&ours, DeviceInfo { id: "d_rules".into(), name: "t".into() }, FixedClock::new(1_793_577_600_000));
    store.set_state_dir(Some(dir.path().join("state")));
    let actor = Actor::agent("mcp:probe", "probe", Via::McpStdio, "d_rules");
    let shown = |v: &Visibility| ["Work/a.md", "Work/Deep/b.md", "Notes/c.md"].into_iter().filter(|p| v.allows(&NotePath::new(*p))).count();
    let before = store.visibility(&actor, ["Work", "Work/Deep", "Notes"]);
    assert_eq!(shown(&before), 3);
    theirs.storage().write_atomic(&NotePath::new("Work/quire-lock.json"), "{}").unwrap();
    assert_eq!(shown(&before), 3, "one call keeps the answers it started with");
    let after = store.visibility(&actor, ["Work", "Work/Deep", "Notes"]);
    assert_eq!(shown(&after), 1, "a locked folder and the folders inside it hide at once");
    assert_ne!(before.fingerprint(), after.fingerprint(), "a lock changes the fingerprint");
    let lazy = store.visibility(&actor, []);
    assert_eq!(shown(&lazy), 1, "a folder not named up front is checked when a path in it is asked about");
    // Being unlocked for good (the unlocking file) still hides it; gone, it shows again.
    std::fs::rename(root.join("Work/quire-lock.json"), root.join("Work/quire-lock.unlocking.json")).unwrap();
    assert_eq!(shown(&store.visibility(&actor, ["Work"])), 1);
    std::fs::remove_file(root.join("Work/quire-lock.unlocking.json")).unwrap();
    assert_eq!(shown(&store.visibility(&actor, ["Work"])), 3, "unlocked, they show again");
}

/// A note below a folder that is a symlink is hidden, wherever the link leads: the vault listing
/// never yields one, so a path like it reached the index some other way.
#[cfg(unix)]
#[test]
fn a_note_below_a_symlinked_folder_is_hidden() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("vault");
    std::fs::create_dir_all(root.join("Notes")).unwrap();
    std::fs::write(root.join("Notes/a.md"), "# a\n").unwrap();
    std::os::unix::fs::symlink(root.join("Notes"), root.join("Alias")).unwrap();
    let vault = Vault::open_local(&root).unwrap();
    let store = AgentStore::open(&vault, DeviceInfo { id: "d_rules".into(), name: "t".into() }, FixedClock::new(1_793_577_600_000));
    store.set_state_dir(Some(dir.path().join("state")));
    let actor = Actor::agent("mcp:probe", "probe", Via::McpStdio, "d_rules");
    for vis in [store.visibility(&actor, ["Notes", "Alias"]), store.visibility(&actor, [])] {
        assert!(vis.allows(&NotePath::new("Notes/a.md")));
        assert!(!vis.allows(&NotePath::new("Alias/a.md")), "a path through a symlinked folder is refused");
    }
}

/// A policy file that says exactly what `r` says (the default zones written out when it has no
/// others).
fn render(r: &PolicyRules) -> String {
    let mut t = format!("format = 1\ndefault = \"{}\"\n", r.default.as_str());
    if r.ai_off {
        t.push_str("ai = \"off\"\n");
    }
    for (p, m) in &r.zones {
        write!(t, "[[zone]]\npath = \"{p}\"\nrole = \"agent\"\nmode = \"{}\"\n", m.as_str()).expect("a String takes any text");
    }
    if let Some((default, zones)) = &r.entry {
        t.push_str("[clients.\"mcp:probe\"]\n");
        if r.revoked {
            t.push_str("revoked = true\n");
        }
        let list = |v: &[String]| v.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ");
        if let Some(f) = &r.allow_folders {
            writeln!(t, "allow_folders = [{}]", list(f)).expect("a String takes any text");
        }
        if let Some(g) = &r.allow_tags {
            writeln!(t, "allow_tags = [{}]", list(g)).expect("a String takes any text");
        }
        if let Some(d) = default {
            writeln!(t, "default = \"{}\"", d.as_str()).expect("a String takes any text");
        }
        if !zones.is_empty() {
            t.push_str("[clients.\"mcp:probe\".zones]\n");
            for (p, m) in zones {
                writeln!(t, "\"{p}\" = \"{}\"", m.as_str()).expect("a String takes any text");
            }
        }
    }
    t
}

/// Variants of a policy that each change one rule `Visibility` must see: the allow lists, a
/// revoke, "AI off", whether the default or the entry's default denies, whether a zone denies,
/// which folder a zone covers, and the client's own zones.
fn one_rule_changed(r: &PolicyRules) -> Vec<(&'static str, PolicyRules)> {
    let with = |f: &dyn Fn(&mut PolicyRules)| {
        let mut v = r.clone();
        f(&mut v);
        v
    };
    let entry = |v: &mut PolicyRules| {
        v.entry.get_or_insert_with(|| (None, Vec::new()));
    };
    vec![
        (
            "allow_folders",
            with(&|v| {
                entry(v);
                v.allow_folders = if v.allow_folders.is_some() { None } else { Some(vec!["Notes/".into()]) };
            }),
        ),
        (
            "allow_tags",
            with(&|v| {
                entry(v);
                v.allow_tags = if v.allow_tags.is_some() { None } else { Some(vec!["work".into()]) };
            }),
        ),
        (
            "revoked",
            with(&|v| {
                entry(v);
                v.revoked = !v.revoked;
            }),
        ),
        ("ai", with(&|v| v.ai_off = !v.ai_off)),
        ("the default's deny", with(&|v| v.default = if v.default == Mode::Deny { Mode::Read } else { Mode::Deny })),
        (
            "the entry's default",
            with(&|v| {
                entry(v);
                if let Some((default, _)) = &mut v.entry {
                    *default = if *default == Some(Mode::Deny) { None } else { Some(Mode::Deny) };
                }
            }),
        ),
        (
            "a zone's path",
            with(&|v| {
                // The same mode on another folder.
                if let Some((p, _)) = v.zones.first_mut() {
                    *p = if p.as_str() == "Projects/" { "Journal/".into() } else { "Projects/".into() };
                }
            }),
        ),
        (
            "a zone's deny",
            with(&|v| {
                if let Some((_, m)) = v.zones.first_mut() {
                    *m = if *m == Mode::Deny { Mode::Read } else { Mode::Deny };
                }
            }),
        ),
        (
            "the entry's zones",
            with(&|v| {
                entry(v);
                if let Some((_, zones)) = &mut v.entry {
                    // Add one, or flip whether the first denies (the pattern stays: only the mode differs).
                    match zones.first_mut() {
                        None => zones.push(("Work/".into(), Mode::Deny)),
                        Some((_, m)) => *m = if *m == Mode::Deny { Mode::Read } else { Mode::Deny },
                    }
                }
            }),
        ),
    ]
}

/// The converse that matters for views keyed by the fingerprint: a change to any one rule that
/// changes an answer changes the fingerprint (a rule left out of the fingerprint would let two
/// callers who see different notes share a view). Each generated policy, each variant of it that
/// changes one rule, and a folder locked meanwhile.
#[cfg(unix)]
#[test]
fn a_rule_that_changes_an_answer_changes_the_fingerprint() {
    let state = tempfile::tempdir().unwrap();
    let mut rng = Rng(0x5EED_CAFE_F00D_0003);
    let actor = Actor::agent("mcp:probe", "probe", Via::McpStdio, "d_rules");
    let mut changed: BTreeMap<&str, usize> = BTreeMap::new();
    for case_no in 0..60 {
        let case = generate(&mut rng);
        let base_rules = &case.rules.policies[case.rules.policies.len() - 1];
        let answers = |vis: &Visibility| case.paths.iter().map(|(p, t)| ReadRules::allows(vis, p, t)).collect::<Vec<bool>>();
        let base = store_with(&case, &[render(base_rules)], state.path()).visibility(&actor, all_folders());
        let mut variants: Vec<(&str, Visibility)> = one_rule_changed(base_rules)
            .into_iter()
            .map(|(what, r)| (what, store_with(&case, &[render(&r)], state.path()).visibility(&actor, all_folders())))
            .collect();
        // A folder locked after the value was made: another `Visibility` sees it.
        let locked = store_with(&case, &[render(base_rules)], state.path());
        locked.vault().storage().write_atomic(&NotePath::new("Work/quire-lock.json"), "{}").unwrap();
        variants.push(("a locked folder", locked.visibility(&actor, all_folders())));
        // A folder that is a symlink, against the same vault on disk without it.
        let (plain_dir, linked_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let on_disk = store_on_disk(&case, &render(base_rules), plain_dir.path(), None).visibility(&actor, all_folders());
        let linked = store_on_disk(&case, &render(base_rules), linked_dir.path(), Some("Projects")).visibility(&actor, all_folders());
        if answers(&linked) != answers(&on_disk) {
            *changed.entry("a symlinked folder").or_default() += 1;
            assert_ne!(
                linked.fingerprint(),
                on_disk.fingerprint(),
                "case {case_no}: a symlinked folder changes answers but not the fingerprint"
            );
        }
        for (what, vis) in &variants {
            if answers(vis) != answers(&base) {
                *changed.entry(what).or_default() += 1;
                assert_ne!(
                    vis.fingerprint(),
                    base.fingerprint(),
                    "case {case_no}: changing {what} changes answers but not the fingerprint\n{}",
                    render(base_rules)
                );
            }
        }
    }
    // Every kind of change must have changed answers in some case, or it was not tested.
    for what in [
        "allow_folders",
        "allow_tags",
        "revoked",
        "ai",
        "the default's deny",
        "the entry's default",
        "a zone's deny",
        "a zone's path",
        "the entry's zones",
        "a locked folder",
        "a symlinked folder",
    ] {
        assert!(changed.get(what).copied().unwrap_or(0) > 2, "{what} changed answers in only {:?} cases", changed.get(what));
    }
}
