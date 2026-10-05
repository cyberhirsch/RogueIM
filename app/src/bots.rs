//! Bots: Rogue (the public AI contact), local AI bots that talk to a model
//! straight from this computer, and the local script API.

use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rim_core::{Command, ContactView, EngineHandle, Status};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Rogue's public invite (unlimited uses, no expiry).
pub const ROGUE_INVITE: &str = "rim2:eyJ2IjoyLCJjYXJkIjp7Im5pY2siOiJSb2d1ZSIsImRldmljZXMiOnsibGlzdCI6eyJhY2NvdW50X3BrIjoiQ0FFU0lLMjFkNnZFZWNZYXdUWjFWQmV3SkpaZW9ad3FIaDk0OWwwM1ZMNUpRclpnIiwidmVyc2lvbiI6MSwiZGV2aWNlcyI6W3sicGVlcl9pZCI6IjEyRDNLb29XSEVpMVFhRWs3QmlDdEdhU25UODVvdGVQZHh0V1hFNkVjYWpZeWljY1FtQjQiLCJjdXJ2ZSI6ImNXdExXWWR2RzR5ZU81NG5rbXh5T0JXYkdDV3ZSRVRYODk1dVkveWpkWHMiLCJmYWxsYmFjayI6IjBCN1hseDUxQ1BEdTV1L0I0Q2FEU2ZkZHFhMnB6bWk0cjhDdGNoNU9LaFkiLCJuYW1lIjoiOGRmNWQ3NjhjZTM3Iiwib3MiOiJERUIiLCJjbGFzcyI6IkRlc2t0b3AiLCJhZGRlZCI6MTc5MTIxNTA4OCwibWFuYWdlciI6dHJ1ZX1dfSwic2lnIjoiWC9pOEs2UVJCSGZuaERyWnNFSHlwSk9pY2RZNDd5ZEdBcmJKelhOdHhzZFA3ZEN3MVh0elR5cVFjdFhKcWdkV3BIazVoOFRMTmp1eU1PWE8waSsrQnc9PSJ9fSwiZGV2aWNlIjoiMTJEM0tvb1dIRWkxUWFFazdCaUN0R2FTblQ4NW90ZVBkeHRXWEU2RWNhall5aWNjUW1CNCIsIm90ayI6IiIsInRva2VuIjoiZTAxYWJlMmJkZDgwMDk1Y2RhN2ZhNTM4NDQ1OWFjOGIiLCJzZWVkcyI6eyJpbmJveCI6Ijc3NDQ4MGVlZWI2YWQwMjVhOWEwYzE1NzdjNmIzODMyOTE0Y2U2ZjI3YjEwZjY2ZGI5OWY1YTQzNDI2ODFhMmYiLCJyZHYiOiI5ZjczZDg3MDRmZGYwYTk3ZTQ1NTIyMjc1MmI3OWMwMTA2NTFhOTc3NjgyNmE5YWZlMzg0YTNhOWYwY2EwNmUyIn0sImFkZHJzIjpbXSwicG93X2JpdHMiOjEyLCJleHBpcmVzIjpudWxsfQ";
const ROGUE_FP: &str = "EF5C45BA6CBDAAA151C32DB411197EEC5B7D0E02";

/// The contact that is Rogue, if any.
pub fn rogue(contacts: &[ContactView]) -> Option<&ContactView> {
    contacts.iter().find(|c| c.fingerprint.replace(' ', "").eq_ignore_ascii_case(ROGUE_FP))
}

/// A bot that runs inside the app: the chat goes from this computer straight
/// to an OpenAI-compatible endpoint, never over the RogueIM network.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalBot {
    pub name: String,
    pub endpoint: String,
    pub model: String,
    pub personality: String,
}

/// Chat key of a local bot.
pub fn key(name: &str) -> String {
    format!("b:{name}")
}

fn keychain(profile: &str, bot: &str) -> Option<keyring::Entry> {
    keyring::Entry::new("RogueIM-bot", &format!("{profile}/{bot}")).ok()
}

pub fn set_api_key(profile: &str, bot: &str, key: &str) -> Result<(), String> {
    keychain(profile, bot).ok_or("no keychain")?.set_password(key).map_err(|e| e.to_string())
}

pub fn api_key(profile: &str, bot: &str) -> Option<String> {
    keychain(profile, bot)?.get_password().ok()
}

pub fn forget_api_key(profile: &str, bot: &str) {
    if let Some(e) = keychain(profile, bot) {
        let _ = e.delete_credential();
    }
}

/// Ask the model on a worker thread; the answer (or error text) comes back on `tx` as (bot name, reply).
pub fn ask(bot: LocalBot, key: String, history: Vec<(bool, String)>, tx: Sender<(String, String)>) {
    std::thread::spawn(move || {
        let mut messages = vec![json!({ "role": "system", "content": format!(
            "Your name is {}. Answer in plain text, no markdown. {}", bot.name, bot.personality) })];
        for (mine, text) in history {
            messages.push(json!({ "role": if mine { "user" } else { "assistant" }, "content": text }));
        }
        let reply = request(&bot, &key, messages).unwrap_or_else(|e| format!("[no answer: {e}]"));
        let _ = tx.send((bot.name, reply));
    });
}

fn request(bot: &LocalBot, key: &str, messages: Vec<serde_json::Value>) -> Result<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(90))).build().into();
    let body = json!({ "model": bot.model, "messages": messages }).to_string();
    let mut req = agent.post(&bot.endpoint).header("Content-Type", "application/json");
    if !key.is_empty() {
        req = req.header("Authorization", &format!("Bearer {key}"));
    }
    let mut resp = req.send(body).map_err(|e| e.to_string())?;
    let text = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    v["choices"][0]["message"]["content"].as_str().map(|s| s.trim().to_string()).ok_or_else(|| "unexpected answer from the endpoint".into())
}

// ---------------------------------------------------------------- local script API

type Clients = Arc<Mutex<Vec<Box<dyn Write + Send>>>>;

/// JSON lines over the local socket / named pipe `rogueim-app-<profile>` (never a network port),
/// same protocol as `rim-cli --api`.
pub struct Api {
    clients: Clients,
    contacts: Arc<Mutex<Vec<(String, String, String)>>>,
}

impl Api {
    pub fn start(profile: &str, h: EngineHandle) -> Result<Api, String> {
        use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions};
        let name = format!("rogueim-app-{profile}");
        let ns = name.clone().to_ns_name::<GenericNamespaced>().map_err(|e| e.to_string())?;
        let listener = ListenerOptions::new().name(ns).create_sync().map_err(|e| e.to_string())?;
        let api = Api { clients: Arc::default(), contacts: Arc::default() };
        let (clients, contacts) = (api.clients.clone(), api.contacts.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming().filter_map(Result::ok) {
                let (recv, send) = conn.split();
                clients.lock().unwrap().push(Box::new(send));
                let (h, contacts, clients) = (h.clone(), contacts.clone(), clients.clone());
                std::thread::spawn(move || {
                    for line in BufReader::new(recv).lines().map_while(Result::ok) {
                        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                        match v["cmd"].as_str() {
                            Some("send") => {
                                let to = v["to"].as_str().unwrap_or("");
                                let id = contacts.lock().unwrap().iter().find(|c| c.1.eq_ignore_ascii_case(to)).map(|c| c.0.clone());
                                if let Some(id) = id {
                                    h.send(Command::SendText { id, body: v["text"].as_str().unwrap_or("").to_string(), reply_to: None, urgent: false });
                                }
                            }
                            Some("status") => {
                                if let Some(s) = v["status"].as_str().and_then(parse_status) {
                                    h.send(Command::SetStatus(s));
                                }
                            }
                            Some("contacts") => {
                                let list: Vec<_> = contacts.lock().unwrap().iter().map(|c| json!({"name": c.1, "status": c.2})).collect();
                                broadcast(&clients, json!({"event":"contacts","list": list}));
                            }
                            _ => {}
                        }
                    }
                });
            }
        });
        Ok(api)
    }

    pub fn set_contacts(&self, list: &[ContactView]) {
        *self.contacts.lock().unwrap() = list.iter().map(|c| (c.id.clone(), c.name.clone(), c.status.label().to_string())).collect();
    }

    pub fn message(&self, from: &str, text: &str, urgent: bool) {
        broadcast(&self.clients, json!({"event":"message","from": from, "text": text, "urgent": urgent}));
    }
}

fn broadcast(clients: &Clients, v: serde_json::Value) {
    let line = format!("{v}\n");
    clients.lock().unwrap().retain_mut(|w| w.write_all(line.as_bytes()).and_then(|_| w.flush()).is_ok());
}

fn parse_status(s: &str) -> Option<Status> {
    Some(match s.to_lowercase().as_str() {
        "online" => Status::Online,
        "ffc" | "free" => Status::FreeForChat,
        "away" => Status::Away,
        "na" | "n/a" => Status::NotAvailable,
        "occupied" | "busy" => Status::Occupied,
        "dnd" => Status::DoNotDisturb,
        "invisible" => Status::Invisible,
        _ => return None,
    })
}
