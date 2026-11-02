// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Change notifications (design §4.3): `subscriptions/listen` (2026-07-28)
//! and the older `resources/subscribe`, in one table.
//!
//! Each subscription has a sink (the stdio writer or an HTTP event stream).
//! `notifications/resources/updated` fire at most once per URI every 300 ms;
//! list changes fire at once. Modern notifications carry the subscription id
//! (the JSON-RPC id of its `subscriptions/listen`) in `_meta`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::protocol::{META_SUBSCRIPTION, modern_result};

/// Where a subscription's messages go.
pub type Sink = Arc<dyn Fn(Value) + Send + Sync>;

/// Most URIs one subscription may watch.
pub const MAX_URIS: usize = 200;
/// Updates to one URI are sent at most this often.
pub const DEBOUNCE: Duration = Duration::from_millis(300);

/// What a client asked to hear about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    pub tools: bool,
    pub resources_list: bool,
    pub uris: Vec<String>,
}

impl Filter {
    /// From `params.notifications`; returns what Quire honours (prompts
    /// never change, so `promptsListChanged` is dropped).
    pub fn from_params(params: &Value) -> Self {
        let n = params.get("notifications").cloned().unwrap_or_default();
        let mut uris: Vec<String> = n
            .get("resourceSubscriptions")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        uris.dedup();
        uris.truncate(MAX_URIS);
        Self {
            tools: n.get("toolsListChanged").and_then(Value::as_bool).unwrap_or(false),
            resources_list: n.get("resourcesListChanged").and_then(Value::as_bool).unwrap_or(false),
            uris,
        }
    }

    /// The `notifications` object of the acknowledgement.
    pub fn agreed(&self) -> Value {
        let mut o = serde_json::Map::new();
        if self.tools {
            o.insert("toolsListChanged".into(), true.into());
        }
        if self.resources_list {
            o.insert("resourcesListChanged".into(), true.into());
        }
        if !self.uris.is_empty() {
            o.insert("resourceSubscriptions".into(), json!(self.uris));
        }
        Value::Object(o)
    }
}

struct Sub {
    handle: u64,
    /// The connection it belongs to (one stdio process, one HTTP stream).
    conn: u64,
    /// The `subscriptions/listen` request id; `None` for `resources/subscribe`.
    id: Option<Value>,
    tools: bool,
    resources_list: bool,
    uris: HashSet<String>,
    sink: Sink,
}

impl Sub {
    fn note(&self, method: &str, mut params: Value) -> Value {
        if let Some(id) = &self.id {
            params["_meta"] = json!({ META_SUBSCRIPTION: id });
        }
        json!({ "jsonrpc": "2.0", "method": method, "params": params })
    }

    fn watches(&self, uri: &str) -> bool {
        self.uris.contains(uri) || self.uris.iter().any(|u| u.split('?').next() == Some(uri))
    }
}

#[derive(Default)]
pub struct Subscriptions {
    subs: Mutex<Vec<Sub>>,
    /// URIs changed, and when their update may go out.
    due: Mutex<HashMap<String, Instant>>,
    next: AtomicU64,
}

impl Subscriptions {
    fn subs(&self) -> std::sync::MutexGuard<'_, Vec<Sub>> {
        self.subs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A new connection id.
    pub fn conn(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Start a `subscriptions/listen` subscription: sends the acknowledgement
    /// first, then notifications. Returns its handle.
    pub fn listen(&self, conn: u64, id: Value, filter: &Filter, sink: Sink) -> u64 {
        let handle = self.conn();
        let sub = Sub {
            handle,
            conn,
            id: Some(id),
            tools: filter.tools,
            resources_list: filter.resources_list,
            uris: filter.uris.iter().cloned().collect(),
            sink,
        };
        (sub.sink)(sub.note("notifications/subscriptions/acknowledged", json!({ "notifications": filter.agreed() })));
        self.subs().push(sub);
        handle
    }

    /// Legacy `resources/subscribe`: one subscription per connection, URIs added to it.
    pub fn subscribe_legacy(&self, conn: u64, uri: &str, sink: Sink) {
        let mut subs = self.subs();
        if let Some(s) = subs.iter_mut().find(|s| s.conn == conn && s.id.is_none()) {
            if s.uris.len() < MAX_URIS * 5 {
                s.uris.insert(uri.to_string());
            }
            return;
        }
        let handle = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        subs.push(Sub { handle, conn, id: None, tools: false, resources_list: false, uris: HashSet::from([uri.to_string()]), sink });
    }

    pub fn unsubscribe_legacy(&self, conn: u64, uri: &str) {
        for s in self.subs().iter_mut().filter(|s| s.conn == conn && s.id.is_none()) {
            s.uris.remove(uri);
        }
    }

    /// End the subscription opened by request `id` on `conn` (the client cancelled it).
    pub fn cancel(&self, conn: u64, id: &Value) -> bool {
        let mut subs = self.subs();
        let before = subs.len();
        subs.retain(|s| !(s.conn == conn && s.id.as_ref() == Some(id)));
        subs.len() != before
    }

    pub fn remove(&self, handle: u64) {
        self.subs().retain(|s| s.handle != handle);
    }

    /// End every subscription of `conn` gracefully: each `subscriptions/listen`
    /// gets its result, so the client knows it wasn't a dropped connection.
    pub fn close_conn(&self, conn: u64) {
        let gone: Vec<Sub> = {
            let mut subs = self.subs();
            let (gone, keep) = std::mem::take(&mut *subs).into_iter().partition(|s| s.conn == conn);
            *subs = keep;
            gone
        };
        for s in gone {
            if let Some(id) = &s.id {
                let result = modern_result(json!({ "_meta": { META_SUBSCRIPTION: id } }));
                (s.sink)(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
            }
        }
    }

    /// Whether any subscription is open (the pump idles otherwise).
    pub fn any(&self) -> bool {
        !self.subs().is_empty()
    }

    /// `uri` changed: its update goes out after the debounce.
    pub fn changed(&self, uri: &str, now: Instant) {
        if !self.subs().iter().any(|s| s.watches(uri)) {
            return;
        }
        self.due.lock().unwrap_or_else(PoisonError::into_inner).entry(uri.to_string()).or_insert(now + DEBOUNCE);
    }

    /// Send updates whose debounce has passed.
    pub fn flush(&self, now: Instant) {
        let ready: Vec<String> = {
            let mut due = self.due.lock().unwrap_or_else(PoisonError::into_inner);
            let ready: Vec<String> = due.iter().filter(|(_, t)| **t <= now).map(|(u, _)| u.clone()).collect();
            for u in &ready {
                due.remove(u);
            }
            ready
        };
        if ready.is_empty() {
            return;
        }
        let subs = self.subs();
        for uri in ready {
            for s in subs.iter() {
                for watched in s.uris.iter().filter(|w| w.as_str() == uri || w.split('?').next() == Some(uri.as_str())) {
                    (s.sink)(s.note("notifications/resources/updated", json!({ "uri": watched })));
                }
            }
        }
    }

    /// The list of resources changed (a note was created, renamed or deleted).
    pub fn resources_list_changed(&self) {
        for s in self.subs().iter().filter(|s| s.resources_list) {
            (s.sink)(s.note("notifications/resources/list_changed", json!({})));
        }
    }

    /// The tools on offer may have changed (the agent policy changed).
    pub fn tools_changed(&self) {
        for s in self.subs().iter().filter(|s| s.tools) {
            (s.sink)(s.note("notifications/tools/list_changed", json!({})));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink() -> (Sink, Arc<Mutex<Vec<Value>>>) {
        let got = Arc::new(Mutex::new(Vec::new()));
        let g = Arc::clone(&got);
        (Arc::new(move |v| g.lock().unwrap().push(v)), got)
    }

    #[test]
    fn acknowledges_then_notifies_debounced() {
        let subs = Subscriptions::default();
        let (s, got) = sink();
        let f = Filter::from_params(
            &json!({"notifications": {"toolsListChanged": true, "promptsListChanged": true, "resourceSubscriptions": ["quire://note/A.md"]}}),
        );
        let conn = subs.conn();
        subs.listen(conn, json!(7), &f, s);
        let ack = got.lock().unwrap()[0].clone();
        assert_eq!(ack["method"], "notifications/subscriptions/acknowledged");
        assert_eq!(ack["params"]["_meta"][META_SUBSCRIPTION], 7);
        assert_eq!(
            ack["params"]["notifications"],
            json!({"toolsListChanged": true, "resourceSubscriptions": ["quire://note/A.md"]}),
            "prompts are dropped"
        );
        let t0 = Instant::now();
        subs.changed("quire://note/A.md", t0);
        subs.changed("quire://note/A.md", t0 + Duration::from_millis(100));
        subs.changed("quire://note/B.md", t0);
        subs.flush(t0 + Duration::from_millis(100));
        assert_eq!(got.lock().unwrap().len(), 1, "debounced");
        subs.flush(t0 + DEBOUNCE);
        let n = got.lock().unwrap().clone();
        assert_eq!(n.len(), 2, "one update for A, none for B");
        assert_eq!(n[1]["params"]["uri"], "quire://note/A.md");
        subs.tools_changed();
        subs.resources_list_changed();
        assert_eq!(got.lock().unwrap().len(), 3, "not subscribed to the resource list");
        subs.close_conn(conn);
        let last = got.lock().unwrap().last().cloned().unwrap();
        assert_eq!(last["id"], 7);
        assert_eq!(last["result"]["resultType"], "complete");
        assert!(!subs.any());
    }

    #[test]
    fn legacy_subscriptions_and_cancel() {
        let subs = Subscriptions::default();
        let (s, got) = sink();
        let conn = subs.conn();
        subs.subscribe_legacy(conn, "quire://note/A.md?section=Undo", Arc::clone(&s));
        subs.changed("quire://note/A.md", Instant::now());
        subs.flush(Instant::now() + DEBOUNCE);
        let n = got.lock().unwrap()[0].clone();
        assert_eq!(n["params"], json!({"uri": "quire://note/A.md?section=Undo"}), "no subscription id for legacy clients");
        subs.unsubscribe_legacy(conn, "quire://note/A.md?section=Undo");
        subs.listen(conn, json!("s1"), &Filter { tools: true, ..Filter::default() }, s);
        assert!(subs.cancel(conn, &json!("s1")));
        assert!(!subs.cancel(conn, &json!("s1")));
        let many = Filter::from_params(
            &json!({"notifications": {"resourceSubscriptions": (0..300).map(|i| format!("u{i}")).collect::<Vec<_>>()}}),
        );
        assert_eq!(many.uris.len(), MAX_URIS);
    }
}
