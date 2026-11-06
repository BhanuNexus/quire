// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Settings → Planning → "Remind me even when Quire is closed" (0.10):
//! installs or removes the per-user job that runs `quire-cli remind`
//! (`quire_app::remind_bg`), and says exactly what is installed where.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use egui::{RichText, Ui};
use quire_app::Workspace;
use quire_app::remind_bg::{self, Backend, Env};

use crate::theme::Palette;

/// What Settings knows about the job for the open vault.
struct State {
    vault: PathBuf,
    backend: Backend,
    installed: bool,
    /// What is (or would be) installed, line by line.
    lines: Vec<String>,
    /// The last install or removal: (message, went wrong).
    message: Option<(String, bool)>,
}

thread_local! {
    /// Checked once per vault (it runs `systemctl` or `crontab`), not every frame.
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn cli_or_placeholder() -> PathBuf {
    remind_bg::find_cli().unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "quire-cli.exe" } else { "quire-cli" }))
}

fn look(vault: &Path) -> State {
    let backend = remind_bg::detect();
    let (installed, lines) = match Env::current() {
        Some(env) => {
            let lines = match remind_bg::plan_install(backend, &cli_or_placeholder(), vault, &env) {
                Ok(plan) => remind_bg::describe(backend, &plan),
                Err(e) => vec![e],
            };
            (remind_bg::installed(backend, vault, &env), lines)
        }
        None => (false, vec!["No home folder was found.".into()]),
    };
    State { vault: vault.to_path_buf(), backend, installed, lines, message: None }
}

fn toggle(st: &mut State, on: bool) {
    let Some(env) = Env::current() else {
        st.message = Some(("No home folder was found.".into(), true));
        return;
    };
    let result = if on {
        match remind_bg::find_cli() {
            Some(cli) => remind_bg::install(st.backend, &cli, &st.vault, &env),
            None => Err("quire-cli wasn't found next to Quire or on the PATH, so nothing was installed.".into()),
        }
    } else {
        remind_bg::uninstall(st.backend, &st.vault, &env)
    };
    match result {
        Ok(plan) => {
            st.installed = on;
            // Off: show what turning it on would install again.
            st.lines = if on {
                remind_bg::describe(st.backend, &plan)
            } else {
                remind_bg::plan_install(st.backend, &cli_or_placeholder(), &st.vault, &env)
                    .map_or_else(|e| vec![e], |plan| remind_bg::describe(st.backend, &plan))
            };
            st.message = Some((if on { "Installed. Reminders now show even when Quire is closed." } else { "Removed." }.into(), false));
        }
        Err(e) => st.message = Some((e, true)),
    }
}

/// The setting, its explanation, and what is installed where.
pub(crate) fn settings(ui: &mut Ui, ws: &Workspace, pal: &Palette) {
    let Some(vault) = ws.vault_root().map(Path::to_path_buf) else { return };
    STATE.with_borrow_mut(|cell| {
        if cell.as_ref().is_none_or(|s| s.vault != vault) {
            *cell = Some(look(&vault));
        }
        let Some(st) = cell.as_mut() else { return };
        let mut on = st.installed;
        if ui
            .checkbox(&mut on, "Remind me even when Quire is closed")
            .on_hover_text("A small job of your own runs quire-cli remind every minute. No administrator rights are needed.")
            .changed()
        {
            toggle(st, on);
        }
        ui.indent("quire-remind-closed", |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "Each reminder shows once, whether Quire or the job shows it. Reminders missed while the computer was off \
                         are still listed when Quire starts. Turning this off removes what was installed.",
                    )
                    .size(12.0)
                    .color(pal.muted),
                )
                .wrap(),
            );
            ui.label(RichText::new(if st.installed { "Installed:" } else { "Turning it on installs:" }).size(12.0).color(pal.strong));
            for l in &st.lines {
                ui.add(egui::Label::new(RichText::new(l).monospace().size(11.5).color(pal.text)).wrap().selectable(true));
            }
            if let Some((m, bad)) = &st.message {
                ui.add(egui::Label::new(RichText::new(m).size(12.0).color(if *bad { pal.danger } else { pal.accent })).wrap());
            }
        });
    });
}
