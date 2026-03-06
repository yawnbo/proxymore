use crate::{
    server::PrintMode,
    traffic::{wrap_entries, Body, Traffic, TrafficHead},
};

use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
use indexmap::IndexMap;
use serde::Serialize;
use serde_json::Value;
use std::{collections::HashMap, fmt};
use time::OffsetDateTime;
use tokio::sync::{broadcast, oneshot, Mutex};
use tokio_tungstenite::tungstenite;

#[derive(Debug)]
pub struct State {
    print_mode: PrintMode,
    traffics: Mutex<IndexMap<usize, Traffic>>,
    rules: Mutex<Vec<Rule>>,
    pending: Mutex<HashMap<usize, PendingEntry>>,
    traffics_notifier: broadcast::Sender<TrafficHead>,
    websockets: Mutex<IndexMap<usize, Vec<WebsocketMessage>>>,
    websockets_notifier: broadcast::Sender<(usize, WebsocketMessage)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleMatcher {
    Any,
    POST,
    GET,
    PUT,
    DELETE,
    PATCH,
    HEAD,
    OPTIONS,
    AnyWebsocket,
}

impl RuleMatcher {
    pub const ALL: &'static [RuleMatcher] = &[
        RuleMatcher::Any,
        RuleMatcher::GET,
        RuleMatcher::POST,
        RuleMatcher::PUT,
        RuleMatcher::DELETE,
        RuleMatcher::PATCH,
        RuleMatcher::HEAD,
        RuleMatcher::OPTIONS,
        RuleMatcher::AnyWebsocket,
    ];

    pub fn matches(&self, method: &str, is_websocket: bool) -> bool {
        match self {
            RuleMatcher::Any => true,
            RuleMatcher::AnyWebsocket => is_websocket,
            RuleMatcher::GET => method.eq_ignore_ascii_case("GET"),
            RuleMatcher::POST => method.eq_ignore_ascii_case("POST"),
            RuleMatcher::PUT => method.eq_ignore_ascii_case("PUT"),
            RuleMatcher::DELETE => method.eq_ignore_ascii_case("DELETE"),
            RuleMatcher::PATCH => method.eq_ignore_ascii_case("PATCH"),
            RuleMatcher::HEAD => method.eq_ignore_ascii_case("HEAD"),
            RuleMatcher::OPTIONS => method.eq_ignore_ascii_case("OPTIONS"),
        }
    }
}

impl fmt::Display for RuleMatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuleMatcher::Any => write!(f, "ANY"),
            RuleMatcher::POST => write!(f, "POST"),
            RuleMatcher::GET => write!(f, "GET"),
            RuleMatcher::PUT => write!(f, "PUT"),
            RuleMatcher::DELETE => write!(f, "DELETE"),
            RuleMatcher::PATCH => write!(f, "PATCH"),
            RuleMatcher::HEAD => write!(f, "HEAD"),
            RuleMatcher::OPTIONS => write!(f, "OPTIONS"),
            RuleMatcher::AnyWebsocket => write!(f, "WS"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleAction {
    PassToDestination,
    PauseToEditRequest,
    PauseToEditResponse,
    PauseToEditRequestAndResponse,
}

impl RuleAction {
    pub const ALL: &'static [RuleAction] = &[
        RuleAction::PassToDestination,
        RuleAction::PauseToEditRequest,
        RuleAction::PauseToEditResponse,
        RuleAction::PauseToEditRequestAndResponse,
    ];
}

impl fmt::Display for RuleAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuleAction::PassToDestination => write!(f, "Pass"),
            RuleAction::PauseToEditRequest => write!(f, "Edit Request"),
            RuleAction::PauseToEditResponse => write!(f, "Edit Response"),
            RuleAction::PauseToEditRequestAndResponse => write!(f, "Edit Req+Res"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub matcher: RuleMatcher,
    pub uri_pattern: Option<String>,
    pub action: RuleAction,
}

impl Rule {
    pub fn matches(&self, method: &str, uri: &str, is_websocket: bool) -> bool {
        if !self.matcher.matches(method, is_websocket) {
            return false;
        }
        match &self.uri_pattern {
            Some(pattern) => glob_match::glob_match(pattern, uri),
            None => true,
        }
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.matcher)?;
        if let Some(pattern) = &self.uri_pattern {
            write!(f, " {}", pattern)?;
        }
        write!(f, " → {}", self.action)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingPhase {
    Request,
    Response,
}

impl fmt::Display for PendingPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PendingPhase::Request => write!(f, "Request"),
            PendingPhase::Response => write!(f, "Response"),
        }
    }
}

#[derive(Debug)]
pub enum PendingResolution {
    Continue(Option<ModifiedTraffic>),
    Cancel,
}

#[derive(Debug, Clone)]
pub struct ModifiedTraffic {
    pub headers: Option<Vec<(String, String)>>,
    pub body: Option<Vec<u8>>,
}

impl ModifiedTraffic {
    /// parse modified json request back into modifiedtraffic
    pub fn from_edited_json_req(json: &str) -> Option<Self> {
        let val: serde_json::Value = serde_json::from_str(json).ok()?;
        let headers = val.get("req_headers").and_then(|h| {
            h.get("items").and_then(|items| {
                items.as_array().map(|arr| {
                    arr.iter()
                        .filter_map(|item| {
                            let name = item.get("name")?.as_str()?;
                            let value = item.get("value")?.as_str()?;
                            Some((name.to_string(), value.to_string()))
                        })
                        .collect()
                })
            })
        });
        let body = val.get("req_body").and_then(|b| {
            let encode = b.get("encode")?.as_str()?;
            let value = b.get("value")?.as_str()?;
            if encode == "base64" {
                base64::engine::general_purpose::STANDARD.decode(value).ok()
            } else {
                Some(value.as_bytes().to_vec())
            }
        });
        Some(Self { headers, body })
    }

    /// parse json response object into a modified traffic object
    pub fn from_edited_json_res(json: &str) -> Option<Self> {
        let val: serde_json::Value = serde_json::from_str(json).ok()?;
        let headers = val.get("res_headers").and_then(|h| {
            h.get("items").and_then(|items| {
                items.as_array().map(|arr| {
                    arr.iter()
                        .filter_map(|item| {
                            let name = item.get("name")?.as_str()?;
                            let value = item.get("value")?.as_str()?;
                            Some((name.to_string(), value.to_string()))
                        })
                        .collect()
                })
            })
        });
        let body = val.get("res_body").and_then(|b| {
            let encode = b.get("encode")?.as_str()?;
            let value = b.get("value")?.as_str()?;
            if encode == "base64" {
                base64::engine::general_purpose::STANDARD.decode(value).ok()
            } else {
                Some(value.as_bytes().to_vec())
            }
        });
        Some(Self { headers, body })
    }
}

#[derive(Debug)]
struct PendingEntry {
    sender: oneshot::Sender<PendingResolution>,
    phase: PendingPhase,
}

impl State {
    pub fn new(print_mode: PrintMode) -> Self {
        let (traffics_notifier, _) = broadcast::channel(128);
        let (websockets_notifier, _) = broadcast::channel(64);
        Self {
            print_mode,
            traffics: Default::default(),
            rules: Default::default(),
            pending: Default::default(),
            traffics_notifier,
            websockets: Default::default(),
            websockets_notifier,
        }
    }

    pub async fn list_rules(&self) -> Vec<Rule> {
        self.rules.lock().await.clone()
    }

    pub async fn add_rule(&self, rule: Rule) {
        self.rules.lock().await.push(rule);
    }

    pub async fn remove_rule(&self, index: usize) {
        let mut rules = self.rules.lock().await;
        if index < rules.len() {
            rules.remove(index);
        }
    }

    pub async fn match_rules(
        &self,
        method: &str,
        uri: &str,
        is_websocket: bool,
    ) -> Option<RuleAction> {
        let rules = self.rules.lock().await;
        rules
            .iter()
            .find(|rule| rule.matches(method, uri, is_websocket))
            .map(|rule| rule.action)
    }

    pub async fn add_pending(
        &self,
        gid: usize,
        phase: PendingPhase,
    ) -> oneshot::Receiver<PendingResolution> {
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .await
            .insert(gid, PendingEntry { sender: tx, phase });
        self.set_traffic_pending(gid, true).await;
        rx
    }

    pub async fn resolve_pending(&self, gid: usize, resolution: PendingResolution) -> bool {
        let entry = self.pending.lock().await.remove(&gid);
        if let Some(entry) = entry {
            self.set_traffic_pending(gid, false).await;
            let _ = entry.sender.send(resolution);
            true
        } else {
            false
        }
    }

    pub async fn is_pending(&self, gid: usize) -> Option<PendingPhase> {
        self.pending.lock().await.get(&gid).map(|e| e.phase)
    }

    pub async fn cancel_all_pending(&self) {
        let entries: Vec<_> = {
            let mut pending = self.pending.lock().await;
            pending.drain().collect()
        };
        for (_, entry) in entries {
            let _ = entry.sender.send(PendingResolution::Continue(None));
        }
    }

    async fn set_traffic_pending(&self, gid: usize, pending: bool) {
        let mut traffics = self.traffics.lock().await;
        let Some((id, traffic)) = traffics.iter_mut().find(|(_, v)| v.gid == gid) else {
            return;
        };
        let mut head = traffic.head(*id);
        head.pending = pending;
        let _ = self.traffics_notifier.send(head);
    }

    // --- end of bullshit that i have to implement for the type ---

    pub async fn add_traffic(&self, traffic: Traffic) {
        if !traffic.valid {
            return;
        }
        let mut traffics = self.traffics.lock().await;
        let id = traffics.len() + 1;
        let head = traffic.head(id);
        traffics.insert(id, traffic);
        let _ = self.traffics_notifier.send(head);
    }

    pub async fn add_traffic_early(&self, traffic: &Traffic) -> usize {
        let mut traffics = self.traffics.lock().await;
        let id = traffics.len() + 1;
        let head = traffic.head(id);
        traffics.insert(id, traffic.clone());
        let _ = self.traffics_notifier.send(head);
        id
    }

    pub async fn update_traffic(&self, traffic: &Traffic) {
        let mut traffics = self.traffics.lock().await;
        let Some((id, existing)) = traffics.iter_mut().find(|(_, v)| v.gid == traffic.gid) else {
            return;
        };
        *existing = traffic.clone();
        let head = traffic.head(*id);
        let _ = self.traffics_notifier.send(head);
    }

    pub async fn done_traffic(&self, gid: usize, raw_size: u64) {
        let mut traffics = self.traffics.lock().await;
        let Some((id, traffic)) = traffics.iter_mut().find(|(_, v)| v.gid == gid) else {
            return;
        };

        traffic.uncompress_res_file().await;
        traffic.done_res_body(raw_size);

        let head = traffic.head(*id);
        let _ = self.traffics_notifier.send(head);
        match self.print_mode {
            PrintMode::Nothing => {}
            PrintMode::Oneline => {
                println!("# {}", traffic.oneline());
            }
            PrintMode::Markdown => {
                println!("{}", traffic.markdown().await);
            }
        }
    }

    pub async fn get_traffic(&self, id: usize) -> Option<Traffic> {
        let traffics = self.traffics.lock().await;
        traffics.get(&id).cloned()
    }

    pub async fn get_traffic_by_gid(&self, gid: usize) -> Option<(usize, Traffic)> {
        let traffics = self.traffics.lock().await;
        traffics
            .iter()
            .find(|(_, v)| v.gid == gid)
            .map(|(id, t)| (*id, t.clone()))
    }

    pub fn subscribe_traffics(&self) -> broadcast::Receiver<TrafficHead> {
        self.traffics_notifier.subscribe()
    }

    pub async fn list_heads(&self) -> Vec<TrafficHead> {
        let traffics = self.traffics.lock().await;
        traffics
            .iter()
            .map(|(id, traffic)| traffic.head(*id))
            .collect()
    }

    pub async fn export_traffic(&self, id: usize, format: &str) -> Result<(String, &'static str)> {
        let traffic = self
            .get_traffic(id)
            .await
            .ok_or_else(|| anyhow!("Not found traffic {id}"))?;
        traffic.export(format).await
    }

    pub async fn export_all_traffics(&self, format: &str) -> Result<(String, &'static str)> {
        let traffics = self.traffics.lock().await;
        match format {
            "markdown" => {
                let output =
                    futures_util::future::join_all(traffics.iter().map(|(_, v)| v.markdown()))
                        .await
                        .into_iter()
                        .collect::<Vec<String>>()
                        .join("\n\n");
                Ok((output, "text/markdown; charset=UTF-8"))
            }
            "har" => {
                let values: Vec<Value> =
                    futures_util::future::join_all(traffics.iter().map(|(_, v)| v.har_entry()))
                        .await
                        .into_iter()
                        .flatten()
                        .collect();
                let json_output = wrap_entries(values);
                let output = serde_json::to_string_pretty(&json_output)?;
                Ok((output, "application/json; charset=UTF-8"))
            }
            "curl" => {
                let output = futures_util::future::join_all(traffics.iter().map(|(_, v)| v.curl()))
                    .await
                    .into_iter()
                    .collect::<Vec<String>>()
                    .join("\n\n");
                Ok((output, "text/plain; charset=UTF-8"))
            }
            "json" => {
                let values = futures_util::future::join_all(traffics.iter().map(|(_, v)| v.json()))
                    .await
                    .into_iter()
                    .collect::<Vec<Value>>();
                let output = serde_json::to_string_pretty(&values)?;
                Ok((output, "application/json; charset=UTF-8"))
            }
            "" => {
                let values = traffics
                    .iter()
                    .map(|(id, traffic)| traffic.head(*id))
                    .collect::<Vec<TrafficHead>>();
                let output = serde_json::to_string_pretty(&values)?;
                Ok((output, "application/json; charset=UTF-8"))
            }
            _ => bail!("Unsupported format: {}", format),
        }
    }

    pub async fn new_websocket(&self) -> usize {
        let mut websockets = self.websockets.lock().await;
        let id = websockets.len() + 1;
        websockets.insert(id, vec![]);
        id
    }

    pub async fn add_websocket_error(&self, id: usize, error: String) {
        let mut websockets = self.websockets.lock().await;
        let Some(messages) = websockets.get_mut(&id) else {
            return;
        };
        let message = WebsocketMessage::Error(error);
        messages.push(message.clone());
        let _ = self.websockets_notifier.send((id, message));
    }

    pub async fn add_websocket_message(
        &self,
        id: usize,
        message: &tungstenite::Message,
        server_to_client: bool,
    ) {
        let mut websockets = self.websockets.lock().await;
        let Some(messages) = websockets.get_mut(&id) else {
            return;
        };
        let body = match message {
            tungstenite::Message::Text(text) => Body::text(text),
            tungstenite::Message::Binary(bin) => Body::bytes(bin),
            _ => return,
        };
        let message = WebsocketMessage::Data(WebsocketData {
            create: OffsetDateTime::now_utc(),
            server_to_client,
            body,
        });
        messages.push(message.clone());
        let _ = self.websockets_notifier.send((id, message));
    }

    pub async fn subscribe_websocket(&self, id: usize) -> Option<SubscribedWebSocket> {
        let websockets = self.websockets.lock().await;
        let messages = websockets.get(&id)?;
        Some((messages.to_vec(), self.websockets_notifier.subscribe()))
    }
}

pub type SubscribedWebSocket = (
    Vec<WebsocketMessage>,
    broadcast::Receiver<(usize, WebsocketMessage)>,
);

#[derive(Debug, Clone, Serialize)]
pub enum WebsocketMessage {
    #[serde(rename = "error")]
    Error(String),
    #[serde(rename = "data")]
    Data(WebsocketData),
}

#[derive(Debug, Clone, Serialize)]
pub struct WebsocketData {
    #[serde(serialize_with = "crate::utils::serialize_datetime")]
    pub create: OffsetDateTime,
    pub server_to_client: bool,
    pub body: Body,
}
