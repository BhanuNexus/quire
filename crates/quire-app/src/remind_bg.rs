// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Reminders while Quire is closed (0.10).
//!
//! `quire-cli remind <vault>` reads the vault's due reminders and shows a
//! desktop notification for each one nobody has shown yet. What fired is
//! kept on this device (`<config>/reminders/<vault>.json`), and the app
//! checks the same list, so a reminder is shown once whether the app or
//! the command shows it. Missed reminders are still listed when the app
//! starts.
//!
//! "Remind me even when Quire is closed" (Settings → Planning) installs a
//! per-user job that runs the command every minute: a systemd user timer
//! on Linux (or a crontab line when there is no `systemd --user`), a
//! `LaunchAgent` on macOS, a Task Scheduler task on Windows. Nothing needs
//! administrator rights, and turning it off removes exactly what was
//! installed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{Duration, NaiveDateTime};
use quire_plan::reminders::{self, Reminder};
use serde::{Deserialize, Serialize};

/// Fired reminders older than this are forgotten.
const KEEP_DAYS: i64 = 30;

// ---------------------------------------------------------------- the fired list

/// The reminders shown on this device for one vault.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FiredLog {
    #[serde(default = "one")]
    pub format: u32,
    /// The vault (for people reading the file).
    #[serde(default)]
    pub vault: String,
    /// Reminder key → when it was shown (`YYYY-MM-DD HH:MM`).
    #[serde(default)]
    pub fired: BTreeMap<String, String>,
    /// Where it lives; `None` for a list that isn't saved.
    #[serde(skip)]
    path: Option<PathBuf>,
    /// The file didn't exist yet.
    #[serde(skip)]
    pub fresh: bool,
}

const fn one() -> u32 {
    1
}

/// A short, stable name for a vault folder (file and job names).
pub fn vault_key(vault: &Path) -> String {
    let canon = vault.canonicalize().unwrap_or_else(|_| vault.to_path_buf());
    // FNV-1a: stable across builds (the std hasher isn't).
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in canon.to_string_lossy().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:08x}", h >> 32)
}

impl FiredLog {
    /// The list for `vault` under `config` (the app's config folder).
    pub fn load_in(config: &Path, vault: &Path) -> Self {
        let path = config.join("reminders").join(format!("{}.json", vault_key(vault)));
        let mut log = match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => Self { fresh: true, ..Self::default() },
        };
        log.format = 1;
        log.vault = vault.display().to_string();
        log.path = Some(path);
        log
    }

    /// The list for `vault` in Quire's config folder (none in unit tests).
    pub fn load(vault: &Path) -> Self {
        match crate::settings::config_dir().filter(|_| !cfg!(test)) {
            Some(c) => Self::load_in(&c, vault),
            None => Self::default(),
        }
    }

    pub fn has(&self, r: &Reminder) -> bool {
        self.fired.contains_key(&r.key())
    }

    pub fn record(&mut self, r: &Reminder, now: NaiveDateTime) {
        self.fired.insert(r.key(), now.format("%Y-%m-%d %H:%M").to_string());
    }

    /// Forget old entries and write the file.
    pub fn save(&mut self, now: NaiveDateTime) -> std::io::Result<()> {
        let floor = (now - Duration::days(KEEP_DAYS)).format("%Y-%m-%d %H:%M").to_string();
        self.fired.retain(|_, at| *at >= floor);
        let Some(p) = &self.path else { return Ok(()) };
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        crate::atomic::write(p, text.as_bytes(), false)
    }
}

// ---------------------------------------------------------------- what is due

/// Every open task's reminder in the vault. Only notes that mention a
/// reminder (`⏰` or `[remind::`) are parsed, so this is quick enough to run
/// every minute.
pub fn vault_reminders(vault: &quire_vault::Vault) -> Vec<Reminder> {
    let mut tasks = Vec::new();
    for p in vault.notes().unwrap_or_default() {
        let Ok(text) = vault.read(&p) else { continue };
        if !text.contains('⏰') && !text.contains("[remind::") {
            continue;
        }
        let ex = quire_markdown::extract::extract(&text);
        tasks.extend(ex.tasks.into_iter().map(|t| (p.as_str().to_string(), t)));
    }
    reminders::schedule(tasks.iter().map(|(p, t)| (p.as_str(), t)))
}

/// Reminders due by `now` (within the missed-reminder window) not yet in the list.
///
/// On the very first run nothing is shown: what is already due goes into
/// the list, so turning this on doesn't bring up a week of old reminders.
pub fn take_due(all: &[Reminder], log: &mut FiredLog, now: NaiveDateTime) -> Vec<Reminder> {
    let floor = now - Duration::days(reminders::MISSED_DAYS);
    let due: Vec<Reminder> = all.iter().filter(|r| r.at <= now && r.at > floor && !log.has(r)).cloned().collect();
    for r in &due {
        log.record(r, now);
    }
    if log.fresh {
        log.fresh = false;
        return Vec::new();
    }
    due
}

/// An exclusive lock on `vault`'s fired list under `config`, until dropped.
///
/// The app and `quire-cli remind` each take it from reading the list to
/// saving it, so neither can read the list while the other is about to
/// note a reminder as shown (both would show it). `None` when no lock
/// could be taken; the list then works as without one.
pub fn lock_in(config: &Path, vault: &Path) -> Option<std::fs::File> {
    let dir = config.join("reminders");
    std::fs::create_dir_all(&dir).ok()?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(format!("{}.lock", vault_key(vault))))
        .ok()?;
    f.lock().ok()?;
    Some(f)
}

/// [`lock_in`] in Quire's config folder (none in unit tests).
pub fn lock(vault: &Path) -> Option<std::fs::File> {
    crate::settings::config_dir().filter(|_| !cfg!(test)).and_then(|c| lock_in(&c, vault))
}

/// The app's side: of reminders coming due now, the ones not shown on this
/// device yet (by `quire-cli remind`), which are then noted as shown.
pub fn not_yet_shown(vault: Option<&Path>, due: Vec<Reminder>, now: NaiveDateTime) -> Vec<Reminder> {
    let Some(vault) = vault.filter(|_| !due.is_empty()) else { return due };
    let _lock = lock(vault);
    let mut log = FiredLog::load(vault);
    claim(&mut log, due, now)
}

/// Of `due`, the reminders `log` hasn't got, noted in it and saved.
fn claim(log: &mut FiredLog, due: Vec<Reminder>, now: NaiveDateTime) -> Vec<Reminder> {
    let due: Vec<Reminder> = due.into_iter().filter(|r| !log.has(r)).collect();
    for r in &due {
        log.record(r, now);
    }
    if !due.is_empty() {
        let _ = log.save(now);
    }
    due
}

/// Show one reminder as a desktop notification (not in tests, nor with
/// `QUIRE_NO_NOTIFICATIONS` set). Waits for the system to take it.
pub fn notify(r: &Reminder) {
    if cfg!(test) || std::env::var_os("QUIRE_NO_NOTIFICATIONS").is_some() {
        return;
    }
    let note = r.path.trim_end_matches(".md");
    let _ = notify_rust::Notification::new().summary("Quire reminder").body(&format!("{}\n{note}", r.text)).appname("Quire").show();
}

// ---------------------------------------------------------------- the scheduled job

/// Where the per-user job lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// `systemd --user`: a `.service` and a `.timer`.
    SystemdUser,
    /// A line in the user's crontab.
    Cron,
    /// A `LaunchAgent` plist.
    LaunchAgent,
    /// A Task Scheduler task (`schtasks`).
    TaskScheduler,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Self::SystemdUser => "a systemd user timer",
            Self::Cron => "a line in your crontab",
            Self::LaunchAgent => "a LaunchAgent",
            Self::TaskScheduler => "a Task Scheduler task",
        }
    }
}

/// What installing (or removing) does, spelled out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Files written: (path, contents).
    pub files: Vec<(PathBuf, String)>,
    /// Commands run, in order.
    pub commands: Vec<Vec<String>>,
    /// The crontab line (cron only).
    pub cron_line: Option<String>,
}

/// Where things go: the home folder and this user's id (for launchd).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Env {
    pub home: PathBuf,
    /// `systemd --user` config folder (`~/.config/systemd/user`).
    pub systemd_dir: PathBuf,
    pub uid: Option<u32>,
    /// `TZ` when Quire runs with one: the job gets it too, so "09:00" is
    /// the same time for it (cron and systemd use the system's zone).
    pub tz: Option<String>,
}

impl Env {
    pub fn current() -> Option<Self> {
        let home = directories::BaseDirs::new()?.home_dir().to_path_buf();
        let xdg =
            std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| home.join(".config"));
        let uid = Command::new("id").arg("-u").output().ok().and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok());
        let tz = std::env::var("TZ").ok().filter(|t| !t.trim().is_empty() && !t.contains(['\n', '\'', '"', '%', '$', ' ']));
        Some(Self { systemd_dir: xdg.join("systemd").join("user"), home, uid, tz })
    }
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

/// A path for a systemd `ExecStart=`: double quotes with `\\` and `"`
/// escaped, `%` and `$` doubled (systemd's specifiers and variables).
fn systemd_quote(p: &str) -> String {
    let mut out = String::from('"');
    for c in p.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            '%' | '$' => {
                out.push(c);
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A path for a crontab line: single quotes for the shell, and `%` (a
/// line break to cron) escaped.
fn cron_quote(p: &str) -> String {
    format!("'{}'", p.replace('\'', "'\\''").replace('%', "\\%"))
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The job's name for this vault.
pub fn job_name(backend: Backend, vault: &Path) -> String {
    let k = vault_key(vault);
    match backend {
        Backend::SystemdUser => format!("quire-remind-{k}"),
        Backend::Cron => format!("quire-remind {k}"),
        Backend::LaunchAgent => format!("dev.quire.remind.{k}"),
        Backend::TaskScheduler => format!("Quire Remind {k}"),
    }
}

/// Why a path can't go into a job file, if it can't.
///
/// 0.10 security review #6: every format here is line-based or quoted, and a newline,
/// carriage return, NUL or other control character in a folder's name
/// would start a new systemd directive or crontab line. A `"` can't be
/// quoted for Task Scheduler (Windows paths never contain one).
pub fn unsafe_path(backend: Backend, p: &str) -> Option<String> {
    if p.chars().any(char::is_control) {
        return Some(format!("{}: the path contains a line break or another control character", p.escape_debug()));
    }
    if backend == Backend::TaskScheduler && p.contains('"') {
        return Some(format!("{p}: the path contains a double quote"));
    }
    None
}

/// What installing does, without doing it. Refuses paths that can't be
/// written safely into the job ([`unsafe_path`]).
pub fn plan_install(backend: Backend, cli: &Path, vault: &Path, env: &Env) -> Result<Plan, String> {
    let name = job_name(backend, vault);
    let (cli_s, vault_s) = (s(cli), s(vault));
    for p in [&cli_s, &vault_s] {
        if let Some(why) = unsafe_path(backend, p) {
            return Err(format!("Reminders can't be scheduled for this vault: {why}."));
        }
    }
    Ok(match backend {
        Backend::SystemdUser => {
            let service = format!(
                "# Written by Quire (Settings → Planning → Remind me even when Quire is closed).\n\
                 [Unit]\nDescription=Quire reminders for {}\n\n\
                 [Service]\nType=oneshot\n{}ExecStart={} remind {}\n",
                // The folder's name only, with systemd's `%` specifiers and
                // `\` (a line continuation at the end) escaped.
                vault.file_name().map_or_else(|| "a vault".to_string(), |n| n.to_string_lossy().replace('\\', "\\\\").replace('%', "%%")),
                env.tz.as_ref().map(|t| format!("Environment=TZ={t}\n")).unwrap_or_default(),
                systemd_quote(&cli_s),
                systemd_quote(&vault_s)
            );
            let timer = "# Written by Quire (Settings → Planning → Remind me even when Quire is closed).\n\
                 [Unit]\nDescription=Quire reminders every minute\n\n\
                 [Timer]\nOnCalendar=*-*-* *:*:00\nAccuracySec=5s\nPersistent=false\n\n\
                 [Install]\nWantedBy=timers.target\n"
                .to_string();
            Plan {
                files: vec![
                    (env.systemd_dir.join(format!("{name}.service")), service),
                    (env.systemd_dir.join(format!("{name}.timer")), timer),
                ],
                commands: vec![
                    vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
                    vec!["systemctl".into(), "--user".into(), "enable".into(), "--now".into(), format!("{name}.timer")],
                ],
                cron_line: None,
            }
        }
        Backend::Cron => {
            // Cron jobs don't get the desktop session's bus; point at the usual one.
            let mut bus = env.uid.map(|u| format!("DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{u}/bus ")).unwrap_or_default();
            if let Some(t) = &env.tz {
                bus = format!("TZ={t} {bus}");
            }
            let line = format!("* * * * * {bus}{} remind {} >/dev/null 2>&1 # {name}", cron_quote(&cli_s), cron_quote(&vault_s));
            Plan { files: Vec::new(), commands: vec![vec!["crontab".into(), "-".into()]], cron_line: Some(line) }
        }
        Backend::LaunchAgent => {
            let plist = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
                 <!-- Written by Quire (Settings → Planning → Remind me even when Quire is closed). -->\n\
                 <plist version=\"1.0\">\n<dict>\n\
                 \t<key>Label</key>\n\t<string>{name}</string>\n\
                 \t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{}</string>\n\t\t<string>remind</string>\n\t\t<string>{}</string>\n\t</array>\n\
                 \t<key>StartInterval</key>\n\t<integer>60</integer>\n\
                 \t<key>RunAtLoad</key>\n\t<true/>\n\
                 \t<key>ProcessType</key>\n\t<string>Background</string>\n\
                 </dict>\n</plist>\n",
                xml(&cli_s),
                xml(&vault_s)
            );
            let path = env.home.join("Library").join("LaunchAgents").join(format!("{name}.plist"));
            let domain = format!("gui/{}", env.uid.unwrap_or(501));
            Plan {
                commands: vec![vec!["launchctl".into(), "bootstrap".into(), domain, s(&path)]],
                files: vec![(path, plist)],
                cron_line: None,
            }
        }
        Backend::TaskScheduler => {
            let run = format!("\"{cli_s}\" remind \"{vault_s}\"");
            Plan {
                files: Vec::new(),
                commands: vec![vec![
                    "schtasks".into(),
                    "/Create".into(),
                    "/F".into(),
                    "/SC".into(),
                    "MINUTE".into(),
                    "/MO".into(),
                    "1".into(),
                    "/TN".into(),
                    name,
                    "/TR".into(),
                    run,
                ]],
                cron_line: None,
            }
        }
    })
}

/// What removing does.
pub fn plan_uninstall(backend: Backend, vault: &Path, env: &Env) -> Plan {
    let name = job_name(backend, vault);
    match backend {
        Backend::SystemdUser => Plan {
            files: vec![
                (env.systemd_dir.join(format!("{name}.service")), String::new()),
                (env.systemd_dir.join(format!("{name}.timer")), String::new()),
            ],
            commands: vec![
                vec!["systemctl".into(), "--user".into(), "disable".into(), "--now".into(), format!("{name}.timer")],
                vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
            ],
            cron_line: None,
        },
        Backend::Cron => {
            Plan { files: Vec::new(), commands: vec![vec!["crontab".into(), "-".into()]], cron_line: Some(format!("# {name}")) }
        }
        Backend::LaunchAgent => {
            let path = env.home.join("Library").join("LaunchAgents").join(format!("{name}.plist"));
            let domain = format!("gui/{}", env.uid.unwrap_or(501));
            Plan {
                commands: vec![vec!["launchctl".into(), "bootout".into(), domain, s(&path)]],
                files: vec![(path, String::new())],
                cron_line: None,
            }
        }
        Backend::TaskScheduler => Plan {
            files: Vec::new(),
            commands: vec![vec!["schtasks".into(), "/Delete".into(), "/F".into(), "/TN".into(), name]],
            cron_line: None,
        },
    }
}

/// A crontab with our line for `name` added (once) or removed.
pub fn edit_crontab(current: &str, line: Option<&str>, marker: &str) -> String {
    let mut lines: Vec<&str> = current.lines().filter(|l| !l.trim_end().ends_with(marker)).collect();
    if let Some(l) = line {
        lines.push(l);
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

fn ok(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// The job kind for this computer.
pub fn detect() -> Backend {
    if cfg!(target_os = "macos") {
        Backend::LaunchAgent
    } else if cfg!(windows) {
        Backend::TaskScheduler
    } else if ok("systemctl", &["--user", "show-environment"]) {
        Backend::SystemdUser
    } else {
        Backend::Cron
    }
}

/// `quire-cli` next to the running app, else on the `PATH`.
pub fn find_cli() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "quire-cli.exe" } else { "quire-cli" };
    if let Some(p) = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.join(exe))).filter(|p| p.is_file()) {
        return Some(p);
    }
    std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths).map(|d| d.join(exe)).find(|p| p.is_file()))
}

fn run(cmd: &[String], stdin: Option<&str>) -> Result<String, String> {
    let (prog, args) = cmd.split_first().ok_or("no command")?;
    let mut c = Command::new(prog);
    c.args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = c.spawn().map_err(|e| format!("{prog} couldn't run: {e}"))?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        use std::io::Write as _;
        pipe.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!("{} failed: {}", cmd.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// The user's crontab ("" when there is none).
fn crontab() -> String {
    run(&["crontab".into(), "-l".into()], None).unwrap_or_default()
}

/// Is the job for `vault` installed?
pub fn installed(backend: Backend, vault: &Path, env: &Env) -> bool {
    let name = job_name(backend, vault);
    match backend {
        Backend::Cron => crontab().lines().any(|l| l.trim_end().ends_with(&format!("# {name}"))),
        Backend::TaskScheduler => ok("schtasks", &["/Query", "/TN", &name]),
        _ => plan_install(backend, Path::new("quire-cli"), vault, env).is_ok_and(|p| p.files.iter().all(|(p, _)| p.is_file())),
    }
}

/// Install the job; returns the plan carried out (what was put where).
pub fn install(backend: Backend, cli: &Path, vault: &Path, env: &Env) -> Result<Plan, String> {
    let plan = plan_install(backend, cli, vault, env)?;
    for (p, text) in &plan.files {
        crate::atomic::write(p, text.as_bytes(), false).map_err(|e| format!("{} couldn't be written: {e}", p.display()))?;
    }
    for cmd in &plan.commands {
        if backend == Backend::Cron {
            let marker = format!("# {}", job_name(backend, vault));
            run(cmd, Some(&edit_crontab(&crontab(), plan.cron_line.as_deref(), &marker)))?;
        } else {
            run(cmd, None)?;
        }
    }
    Ok(plan)
}

/// Remove the job and its files.
pub fn uninstall(backend: Backend, vault: &Path, env: &Env) -> Result<Plan, String> {
    let plan = plan_uninstall(backend, vault, env);
    let mut errors = Vec::new();
    for cmd in &plan.commands {
        let r = if backend == Backend::Cron {
            run(cmd, Some(&edit_crontab(&crontab(), None, plan.cron_line.as_deref().unwrap_or("# quire-remind"))))
        } else {
            run(cmd, None)
        };
        // Already gone is fine.
        if let Err(e) = r {
            errors.push(e);
        }
    }
    for (p, _) in &plan.files {
        if p.exists() {
            std::fs::remove_file(p).map_err(|e| format!("{} couldn't be removed: {e}", p.display()))?;
        }
    }
    if backend == Backend::SystemdUser {
        let _ = run(&["systemctl".into(), "--user".into(), "daemon-reload".into()], None);
    }
    match backend {
        Backend::TaskScheduler | Backend::Cron if !errors.is_empty() => Err(errors.join("; ")),
        _ => Ok(plan),
    }
}

/// Words for Settings: what is (or would be) installed and where.
pub fn describe(backend: Backend, plan: &Plan) -> Vec<String> {
    let mut out = vec![format!("Uses {}.", backend.label())];
    for (p, _) in &plan.files {
        out.push(format!("File: {}", p.display()));
    }
    if let Some(l) = &plan.cron_line {
        out.push(format!("Crontab line: {l}"));
    }
    for c in &plan.commands {
        if c.first().map(String::as_str) != Some("crontab") {
            out.push(format!("Runs: {}", c.join(" ")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Env {
        Env { home: PathBuf::from("/home/ada"), systemd_dir: PathBuf::from("/home/ada/.config/systemd/user"), uid: Some(1000), tz: None }
    }

    fn at(s: &str) -> NaiveDateTime {
        reminders::parse_at(s).unwrap()
    }

    #[test]
    fn job_files_and_commands() {
        let (cli, vault) = (Path::new("/opt/Quire/quire-cli"), Path::new("/home/ada/My Notes"));
        let k = vault_key(vault);
        assert_eq!(k.len(), 8);
        let p = plan_install(Backend::SystemdUser, cli, vault, &env()).unwrap();
        assert_eq!(p.files[0].0, PathBuf::from(format!("/home/ada/.config/systemd/user/quire-remind-{k}.service")));
        assert!(p.files[0].1.contains("ExecStart=\"/opt/Quire/quire-cli\" remind \"/home/ada/My Notes\"\n"), "{}", p.files[0].1);
        assert!(p.files[1].1.contains("OnCalendar=*-*-* *:*:00") && p.files[1].1.contains("WantedBy=timers.target"));
        assert_eq!(p.commands[1], ["systemctl", "--user", "enable", "--now", &format!("quire-remind-{k}.timer")]);
        let u = plan_uninstall(Backend::SystemdUser, vault, &env());
        assert_eq!(u.commands[0], ["systemctl", "--user", "disable", "--now", &format!("quire-remind-{k}.timer")]);
        assert_eq!(u.files.len(), 2);

        let c = plan_install(Backend::Cron, cli, vault, &env()).unwrap();
        let line = c.cron_line.unwrap();
        assert_eq!(
            line,
            format!(
                "* * * * * DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus '/opt/Quire/quire-cli' remind '/home/ada/My Notes' >/dev/null 2>&1 # quire-remind {k}"
            )
        );

        let tz = Env { tz: Some("Asia/Kolkata".into()), ..env() };
        assert!(
            plan_install(Backend::Cron, cli, vault, &tz)
                .unwrap()
                .cron_line
                .unwrap()
                .starts_with("* * * * * TZ=Asia/Kolkata DBUS_SESSION_BUS_ADDRESS=")
        );
        assert!(
            plan_install(Backend::SystemdUser, cli, vault, &tz).unwrap().files[0].1.contains("Environment=TZ=Asia/Kolkata\nExecStart=")
        );
        let l = plan_install(
            Backend::LaunchAgent,
            Path::new("/Applications/Quire.app/Contents/MacOS/quire-cli"),
            Path::new("/Users/ada/A&B"),
            &env(),
        )
        .unwrap();
        assert!(l.files[0].0.ends_with(format!("Library/LaunchAgents/dev.quire.remind.{}.plist", vault_key(Path::new("/Users/ada/A&B")))));
        assert!(l.files[0].1.contains("<string>/Users/ada/A&amp;B</string>") && l.files[0].1.contains("<integer>60</integer>"));
        assert_eq!(l.commands[0][..3], ["launchctl", "bootstrap", "gui/1000"]);

        let w = plan_install(
            Backend::TaskScheduler,
            Path::new(r"C:\Program Files\Quire\quire-cli.exe"),
            Path::new(r"C:\Users\ada\Notes"),
            &env(),
        )
        .unwrap();
        assert_eq!(w.commands[0][..7], ["schtasks", "/Create", "/F", "/SC", "MINUTE", "/MO", "1"]);
        assert_eq!(w.commands[0][10], r#""C:\Program Files\Quire\quire-cli.exe" remind "C:\Users\ada\Notes""#);
        assert_eq!(systemd_quote("/a b/50%$x\"y"), r#""/a b/50%%$$x\"y""#);
        assert_eq!(cron_quote("/it's/100%"), r"'/it'\''s/100\%'");
        let wu = plan_uninstall(Backend::TaskScheduler, Path::new(r"C:\Users\ada\Notes"), &env());
        assert_eq!(wu.commands[0][..4], ["schtasks", "/Delete", "/F", "/TN"]);
        // systemd only runs on Linux; on Windows the path would be joined with `\`.
        #[cfg(unix)]
        assert!(describe(Backend::SystemdUser, &p).iter().any(|l| l.starts_with("File: /home/ada/.config/systemd/user/quire-remind-")));
    }

    /// 0.10 security review #6: a line break in a folder's name can't add
    /// a systemd directive or a crontab line, in any format.
    #[test]
    fn paths_with_line_breaks_are_refused_and_others_quoted() {
        let cli = Path::new("/opt/Quire/quire-cli");
        for bad in [
            "/home/ada/x\nExecStartPre=/bin/sh -c 'touch /tmp/pwned'",
            "/home/ada/x\n* * * * * touch /tmp/pwned",
            "/home/ada/x\ry",
            "/home/ada/x\0y",
            "/home/ada/x\u{85}y",
        ] {
            for b in [Backend::SystemdUser, Backend::Cron, Backend::LaunchAgent, Backend::TaskScheduler] {
                let e = plan_install(b, cli, Path::new(bad), &env()).unwrap_err();
                assert!(e.contains("control character"), "{e}");
                assert!(plan_install(b, Path::new(bad), Path::new("/home/ada/Notes"), &env()).is_err(), "the program's path too");
                assert!(install(b, cli, Path::new(bad), &env()).is_err());
            }
        }
        assert!(plan_install(Backend::TaskScheduler, cli, Path::new(r#"C:\a"b"#), &env()).is_err());
        // Odd but legal Unix names stay one line and one argument in each Unix
        // format (on Windows the trailing `\` would be a separator).
        #[cfg(unix)]
        {
            let odd = Path::new("/home/ada/50% \"it's\" $HOME;rm -rf ~ <x>&\\");
            let p = plan_install(Backend::SystemdUser, cli, odd, &env()).unwrap();
            let unit = &p.files[0].1;
            assert_eq!(unit.lines().filter(|l| l.starts_with("ExecStart")).count(), 1, "{unit}");
            assert!(unit.contains(r#"remind "/home/ada/50%% \"it's\" $$HOME;rm -rf ~ <x>&\\""#), "{unit}");
            assert!(unit.contains("Description=Quire reminders for 50%% \"it's\" $HOME;rm -rf ~ <x>&\\\\\n"), "{unit}");
            let line = plan_install(Backend::Cron, cli, odd, &env()).unwrap().cron_line.unwrap();
            assert!(!line.contains('\n'));
            assert!(line.contains(r#"remind '/home/ada/50\% "it'\''s" $HOME;rm -rf ~ <x>&\'"#), "the cron line quotes the odd path");
            let plist = &plan_install(Backend::LaunchAgent, cli, odd, &env()).unwrap().files[0].1;
            assert!(plist.contains("<string>/home/ada/50% &quot;it's&quot; $HOME;rm -rf ~ &lt;x&gt;&amp;\\</string>"), "{plist}");
        }
        let w = plan_install(Backend::TaskScheduler, Path::new(r"C:\Q\quire-cli.exe"), Path::new(r"C:\Users\ada\50% notes & more"), &env())
            .unwrap();
        assert_eq!(w.commands[0][10], r#""C:\Q\quire-cli.exe" remind "C:\Users\ada\50% notes & more""#);
    }

    #[test]
    fn crontab_lines_are_added_once_and_removed() {
        let mine = "* * * * * quire-cli remind /v # quire-remind abcd1234";
        let t = edit_crontab("MAILTO=\"\"\n0 3 * * * backup\n", Some(mine), "# quire-remind abcd1234");
        assert_eq!(t, format!("MAILTO=\"\"\n0 3 * * * backup\n{mine}\n"));
        assert_eq!(edit_crontab(&t, Some(mine), "# quire-remind abcd1234"), t, "not twice");
        assert_eq!(edit_crontab(&t, None, "# quire-remind abcd1234"), "MAILTO=\"\"\n0 3 * * * backup\n");
        assert_eq!(edit_crontab("", None, "# x"), "");
    }

    /// The app and `quire-cli remind` checking at the same moment: with the
    /// lock, exactly one of them shows each reminder.
    #[test]
    fn the_app_and_the_command_never_both_show_a_reminder() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().to_path_buf();
        let vault = Path::new("/v");
        let v = quire_vault::Vault::in_memory(&[("A.md", "- [ ] Call Bo ⏰ 2026-11-05 09:00\n- [ ] Pay rent ⏰ 2026-11-05 09:00\n")]);
        let all = vault_reminders(&v);
        for round in 0..5 {
            let now = at("2026-11-05 09:00") + Duration::minutes(round);
            let _ = std::fs::remove_file(config.join("reminders").join(format!("{}.json", vault_key(vault))));
            let mut seed = FiredLog::load_in(&config, vault);
            seed.fresh = false;
            seed.save(now).unwrap();
            let shown = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let threads: Vec<_> = (0..8)
                .map(|i| {
                    let (config, all, shown) = (config.clone(), all.clone(), std::sync::Arc::clone(&shown));
                    std::thread::spawn(move || {
                        let _lock = lock_in(&config, vault).expect("a lock");
                        let mut log = FiredLog::load_in(&config, vault);
                        let n = if i % 2 == 0 {
                            // `quire-cli remind`
                            let due = take_due(&all, &mut log, now);
                            log.save(now).unwrap();
                            due.len()
                        } else {
                            // the app
                            claim(&mut log, all.clone(), now).len()
                        };
                        shown.fetch_add(n, std::sync::atomic::Ordering::SeqCst);
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }
            assert_eq!(shown.load(std::sync::atomic::Ordering::SeqCst), 2, "round {round}: each reminder once");
        }
    }

    #[test]
    fn each_reminder_fires_once() {
        let dir = tempfile::tempdir().unwrap();
        let v = quire_vault::Vault::in_memory(&[
            ("A.md", "- [ ] Call Bo ⏰ 2026-11-05 09:00\n- [ ] Later ⏰ 2026-11-05 18:00\n- [x] Done ⏰ 2026-11-05 08:00\n"),
            ("B.md", "- [ ] Old ⏰ 2026-10-01 09:00\n- [ ] Field [remind:: 2026-11-05 09:30]\n"),
            ("C.md", "no reminders"),
        ]);
        let all = vault_reminders(&v);
        assert_eq!(all.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(), ["Old", "Call Bo", "Field", "Later"]);
        let mut log = FiredLog::load_in(dir.path(), Path::new("/v"));
        assert!(log.fresh);
        assert!(take_due(&all, &mut log, at("2026-11-05 08:59")).is_empty(), "the first run only takes note");
        log.save(at("2026-11-05 08:59")).unwrap();
        let mut log = FiredLog::load_in(dir.path(), Path::new("/v"));
        assert!(!log.fresh);
        let due = take_due(&all, &mut log, at("2026-11-05 09:31"));
        assert_eq!(due.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(), ["Call Bo", "Field"]);
        log.save(at("2026-11-05 09:31")).unwrap();
        // The app (or the next run) sees they were shown.
        let mut again = FiredLog::load_in(dir.path(), Path::new("/v"));
        assert!(take_due(&all, &mut again, at("2026-11-05 09:32")).is_empty());
        assert_eq!(take_due(&all, &mut again, at("2026-11-05 18:00")).len(), 1);
        again.save(at("2026-12-20 00:00")).unwrap();
        assert!(again.fired.is_empty(), "old entries are forgotten");
    }
}
