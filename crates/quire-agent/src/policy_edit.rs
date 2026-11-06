// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Changing `agent-policy.toml` from Settings → Agents without losing what
//! the person wrote by hand: comments, order and unknown keys survive
//! (`toml_edit`, design §2.4).

use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

use crate::policy::{Mode, ZoneRole};

/// The file Settings starts from when the vault has none.
pub const STARTER: &str = "\
# What AI agents may do in this vault. Quire's Settings → Agents edits this
# file; a change made elsewhere takes effect on a device only after it is
# approved there.
format = 1
# deny | read | propose | create-only | direct
default = \"propose\"

[[zone]]
path = \"wiki/\"
role = \"agent\"
mode = \"direct\"

[[zone]]
path = \"raw/\"
role = \"raw\"
mode = \"create-only\"
";

/// An editable policy file.
pub struct PolicyDoc {
    doc: DocumentMut,
}

fn same_path(a: &str, b: &str) -> bool {
    a.trim_start_matches("./").eq_ignore_ascii_case(b.trim_start_matches("./"))
}

impl PolicyDoc {
    /// The file's text, or the starter when there is none.
    pub fn parse(text: Option<&str>) -> Result<Self, String> {
        let doc = text.unwrap_or(STARTER).parse::<DocumentMut>().map_err(|e| e.to_string())?;
        Ok(Self { doc })
    }

    pub fn text(&self) -> String {
        self.doc.to_string()
    }

    pub fn set_default(&mut self, mode: Mode) {
        self.doc["default"] = value(mode.as_str());
    }

    fn zones(&mut self) -> &mut ArrayOfTables {
        if !self.doc.get("zone").is_some_and(Item::is_array_of_tables) {
            self.doc["zone"] = Item::ArrayOfTables(ArrayOfTables::new());
        }
        self.doc["zone"].as_array_of_tables_mut().unwrap_or_else(|| unreachable!("just set"))
    }

    /// Add a zone, or change the one with this path.
    pub fn set_zone(&mut self, path: &str, role: ZoneRole, mode: Mode) {
        let zones = self.zones();
        if let Some(t) = zones.iter_mut().find(|t| t.get("path").and_then(Item::as_str).is_some_and(|p| same_path(p, path))) {
            t["role"] = value(role.as_str());
            t["mode"] = value(mode.as_str());
            return;
        }
        let mut t = Table::new();
        t["path"] = value(path);
        t["role"] = value(role.as_str());
        t["mode"] = value(mode.as_str());
        zones.push(t);
    }

    pub fn remove_zone(&mut self, path: &str) {
        let zones = self.zones();
        let keep: Vec<usize> = (0..zones.len())
            .filter(|i| !zones.get(*i).and_then(|t| t.get("path")).and_then(Item::as_str).is_some_and(|p| same_path(p, path)))
            .collect();
        let mut fresh = ArrayOfTables::new();
        for i in keep {
            if let Some(t) = zones.get(i) {
                fresh.push(t.clone());
            }
        }
        *zones = fresh;
    }

    fn client(&mut self, key: &str) -> &mut Table {
        if !self.doc.get("clients").is_some_and(Item::is_table) {
            let mut t = Table::new();
            t.set_implicit(true);
            self.doc["clients"] = Item::Table(t);
        }
        let clients = self.doc["clients"].as_table_mut().unwrap_or_else(|| unreachable!("just set"));
        if !clients.get(key).is_some_and(Item::is_table) {
            clients.insert(key, Item::Table(Table::new()));
        }
        clients[key].as_table_mut().unwrap_or_else(|| unreachable!("just set"))
    }

    /// Cap a client (`None`: no cap of its own).
    pub fn set_client_default(&mut self, key: &str, mode: Option<Mode>) {
        let c = self.client(key);
        match mode {
            Some(m) => c["default"] = value(m.as_str()),
            None => {
                c.remove("default");
            }
        }
    }

    pub fn set_approve_in_client(&mut self, key: &str, on: bool) {
        let c = self.client(key);
        if on {
            c["approve_in_client"] = value(true);
        } else {
            c.remove("approve_in_client");
        }
    }

    /// Revoke a client (0.11): denied everything, and its reads logged
    /// from now on. `false` gives its access back (a widening).
    pub fn set_revoked(&mut self, key: &str, on: bool) {
        let c = self.client(key);
        if on {
            c["revoked"] = value(true);
            c["log_reads"] = value(true);
        } else {
            c.remove("revoked");
        }
    }

    /// Limit a client to tags and folders (`None`: no limit of that kind).
    pub fn set_allow(&mut self, key: &str, tags: Option<&[String]>, folders: Option<&[String]>) {
        let c = self.client(key);
        for (k, v) in [("allow_tags", tags), ("allow_folders", folders)] {
            match v {
                Some(list) => {
                    let mut a = toml_edit::Array::new();
                    for x in list {
                        a.push(x.as_str());
                    }
                    c[k] = value(a);
                }
                None => {
                    c.remove(k);
                }
            }
        }
    }

    /// "AI off" for the vault.
    pub fn set_ai_off(&mut self, off: bool) {
        if off {
            self.doc["ai"] = value("off");
        } else {
            self.doc.remove("ai");
        }
    }

    pub fn remove_client(&mut self, key: &str) {
        if let Some(t) = self.doc.get_mut("clients").and_then(Item::as_table_mut) {
            t.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Policy;

    #[test]
    fn edits_keep_comments_and_unknown_keys() {
        let text = "# mine\nformat = 1\ndefault = \"propose\" # keep\nfuture = true\n\n[[zone]]\npath = \"wiki/\" # agents\nrole = \"agent\"\nmode = \"direct\"\n";
        let mut d = PolicyDoc::parse(Some(text)).unwrap();
        d.set_zone("Private/", ZoneRole::Private, Mode::Deny);
        d.set_zone("WIKI/", ZoneRole::Agent, Mode::Propose);
        d.set_client_default("mcp:claude-code", Some(Mode::Read));
        d.set_approve_in_client("mcp:claude-code", true);
        let out = d.text();
        assert!(out.starts_with("# mine\n") && out.contains("# agents") && out.contains("future = true"), "{out}");
        let p = Policy::parse(&out).unwrap();
        assert_eq!(p.zones.len(), 2);
        assert_eq!(p.zone_mode("wiki/x.md"), Mode::Propose);
        assert_eq!(p.zone_mode("Private/x.md"), Mode::Deny);
        assert_eq!(p.clients["mcp:claude-code"].default, Some(Mode::Read));
        assert!(p.clients["mcp:claude-code"].approve_in_client);
        d.remove_zone("private/");
        d.set_client_default("mcp:claude-code", None);
        d.remove_client("mcp:claude-code");
        d.set_default(Mode::Read);
        let p = Policy::parse(&d.text()).unwrap();
        assert_eq!(p.zones.len(), 1);
        assert!(p.clients.is_empty());
        assert_eq!(p.default, Mode::Read);
        assert_eq!(Policy::parse(STARTER).unwrap(), Policy::default());
    }
}
