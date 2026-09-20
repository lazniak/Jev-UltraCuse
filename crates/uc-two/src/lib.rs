//! System Two: a chat LLM (OpenRouter) that runs *beside* the Jev loop.
//!
//! Jev decides one element and one operation per step in ~300 ms but cannot plan or
//! write. This crate adds the slow, rare part: a plan of sub-goals at the start, a
//! rescue when Jev is stuck, and text on demand. It never blocks the loop by itself:
//! the [`Advisor`] runs on its own thread, requests go through a channel, and the loop
//! decides when to look at replies (`try_recv`) and when waiting is worth it
//! (`recv_timeout` — only when Jev has already given up on the step).
//!
//! Every proposal from the model goes through the same code gates as a Jev decision
//! (`uc-loop::policy`): the LLM can suggest, it cannot bypass.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant};

pub const CHAT_URL: &str = "https://openrouter.ai/api/v1/chat/completions";
pub const MODELS_URL: &str = "https://openrouter.ai/api/v1/models";
/// Referer/title OpenRouter shows in its usage dashboard.
const REFERER: &str = "https://github.com/lazniak/Jev-UltraCuse";
const TITLE: &str = "Jev-UltraCuse";

#[derive(Debug, thiserror::Error)]
pub enum TwoError {
    #[error("no OpenRouter key: set OPENROUTER_API_KEY (or OPEN_ROUTER_API_KEY)")]
    NoKey,
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("OpenRouter {status}: {body}")]
    Status { status: u16, body: String },
    #[error("bad reply: {0}")]
    Protocol(String),
    #[error("runtime: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug)]
pub struct Config {
    pub model: String,
    key: String,
    /// Which variable the key came from (never the key itself — safe to log).
    pub key_name: &'static str,
    pub max_calls: u32,
    pub timeout: Duration,
}

impl Config {
    /// Key from the same places the Jev client looks (process env, then
    /// `HKCU\Environment`), under the OpenRouter names.
    pub fn discover(model: &str, max_calls: u32, timeout: Duration) -> Result<Self, TwoError> {
        let name = Self::key_available().ok_or(TwoError::NoKey)?;
        let key = uc_jev::user_env(name).ok_or(TwoError::NoKey)?;
        Ok(Self {
            model: model.to_string(),
            key,
            key_name: name,
            max_calls,
            timeout,
        })
    }

    /// Which OpenRouter key variable is set (for the settings window), without
    /// reading the key out. `JEVUSE_API_KEY` is a Jev-only alias, not a chat key.
    pub fn key_available() -> Option<&'static str> {
        uc_jev::Provider::OpenRouter
            .key_names()
            .iter()
            .copied()
            .filter(|n| n.contains("ROUTER"))
            .find(|n| uc_jev::user_env(n).is_some())
    }
}

// ------------------------------------------------------------------ model list

/// One row of OpenRouter's `/models` (prices normalised to USD per million tokens).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub context_length: u64,
    pub prompt_usd_per_mtok: f64,
    pub completion_usd_per_mtok: f64,
}

impl ModelInfo {
    pub fn is_free(&self) -> bool {
        self.prompt_usd_per_mtok == 0.0 && self.completion_usd_per_mtok == 0.0
    }
}

fn parse_models(v: &Value) -> Vec<ModelInfo> {
    let mut out: Vec<ModelInfo> = v["data"]
        .as_array()
        .map(|a| a.iter().filter_map(parse_model).collect())
        .unwrap_or_default();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn parse_model(m: &Value) -> Option<ModelInfo> {
    let id = m["id"].as_str()?.to_string();
    let price = |k: &str| -> f64 {
        let p = &m["pricing"][k];
        p.as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .or_else(|| p.as_f64())
            .unwrap_or(0.0)
            * 1_000_000.0
    };
    Some(ModelInfo {
        name: m["name"].as_str().unwrap_or(&id).to_string(),
        context_length: m["context_length"].as_u64().unwrap_or(0),
        prompt_usd_per_mtok: price("prompt"),
        completion_usd_per_mtok: price("completion"),
        id,
    })
}

/// Fetch the model catalogue (public endpoint; the key only adds account-specific
/// availability). Blocking; ~1 s. Call it from a background thread in a UI.
pub fn list_models(key: Option<&str>) -> Result<Vec<ModelInfo>, TwoError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()?;
        let mut req = client.get(MODELS_URL);
        if let Some(k) = key {
            req = req.bearer_auth(k);
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let body = resp.text().await?;
        if status >= 300 {
            return Err(TwoError::Status {
                status,
                body: body.chars().take(300).collect(),
            });
        }
        let v: Value =
            serde_json::from_str(&body).map_err(|e| TwoError::Protocol(e.to_string()))?;
        Ok(parse_models(&v))
    })
}

// ------------------------------------------------------------------ advice

/// One proposed step, in the loop's own vocabulary (`policy::judge` vets it).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Next {
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// What the model may answer. Every field is optional; the loop applies what it can.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Advice {
    /// Ordered sub-goals, each one concrete UI action, in the goal's language.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Vec<String>>,
    /// A rephrased current sub-goal (rescue).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subgoal: Option<String>,
    /// A concrete next step (rescue).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<Next>,
    /// Text to type when the goal needs prose the user did not dictate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// One line for the log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Advice {
    pub fn is_empty(&self) -> bool {
        self.plan.as_ref().is_none_or(|p| p.is_empty())
            && self.subgoal.is_none()
            && self.next.is_none()
            && self.text.is_none()
    }
}

/// Parse the model's reply leniently: code fences, prose around the object and
/// trailing commentary are tolerated; the first `{` … last `}` must be valid JSON.
pub fn parse_advice(text: &str) -> Result<Advice, String> {
    let t = text.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t).trim();
    let start = t.find('{').ok_or("no JSON object in reply")?;
    let end = t.rfind('}').ok_or("no JSON object in reply")?;
    if end < start {
        return Err("no JSON object in reply".into());
    }
    let mut a: Advice = serde_json::from_str(&t[start..=end]).map_err(|e| e.to_string())?;
    if let Some(p) = a.plan.as_mut() {
        p.retain(|s| !s.trim().is_empty());
        if p.is_empty() {
            a.plan = None;
        }
    }
    if a.subgoal.as_deref().is_some_and(|s| s.trim().is_empty()) {
        a.subgoal = None;
    }
    if a.text.as_deref().is_some_and(|s| s.is_empty()) {
        a.text = None;
    }
    if a.next.as_ref().is_some_and(|n| n.op.trim().is_empty()) {
        a.next = None;
    }
    Ok(a)
}

// ------------------------------------------------------------------ requests

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// At the start: break the goal into sub-goals. Fire-and-forget.
    Plan,
    /// Jev is stuck: a next step, a rephrased sub-goal, or text.
    Rescue,
}

#[derive(Clone, Debug)]
pub struct Request {
    pub kind: Kind,
    pub goal: String,
    pub subgoal: Option<String>,
    /// The same reduced GUI state Jev saw (scene + elements with `e<i>` ids).
    pub state: Value,
    /// Jev's reading of the state (top targets, op, goal signal) as a prior.
    pub jev: Option<Value>,
    pub why: Option<String>,
    pub need_text: bool,
}

#[derive(Clone, Debug)]
pub struct Reply {
    pub kind: Kind,
    pub advice: Result<Advice, String>,
    pub model: String,
    pub ms: f64,
    pub cost_usd: f64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// What the loop writes to its ledger about one consultation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Note {
    pub kind: Kind,
    pub model: String,
    pub ms: f64,
    pub cost_usd: f64,
    pub note: String,
    /// Did the loop act on it (plan adopted / step taken / text used)?
    pub applied: bool,
}

pub const SYSTEM_PROMPT: &str = "You are System Two of a Windows computer-use agent. A fast decision model (Jev) picks one visible UI element and one operation per step from the GUI state you receive; it cannot plan and cannot write text. You do both, and you answer with ONE JSON object and nothing else.\n\
Operations Jev can execute: click, right_click, type (needs text), key (one of: enter, f2, delete, esc, tab, ctrl+s, ctrl+z, down, up), scroll_down, scroll_up, wait, done.\n\
Elements are listed with ids like \"e3\"; refer to them by id only. Only elements in the state exist; never invent one. The state may include the pop-up menus of the target process.\n\
Never propose deleting, sending, paying, or closing without saving unless the goal explicitly asks for it; the agent has its own guard for those anyway.\n\
Sub-goals: 2 to 6 items, each ONE concrete UI action that can be checked on screen (e.g. \"open the context menu of the desktop background\", \"choose New > Text Document\"), written in the language of the goal, in order. Do not include steps already done.\n\
Schema: {\"plan\": [string, ...] | null, \"subgoal\": string | null, \"next\": {\"op\": string, \"target\": \"eN\" | null, \"key\": string | null, \"text\": string | null} | null, \"text\": string | null, \"note\": string}\n\
task=plan: fill \"plan\" (and \"text\" if the goal needs prose the user did not dictate). task=rescue: fill \"next\" (a step Jev should take now) and/or \"subgoal\" (a clearer wording of the current sub-goal); fill \"text\" when need_text is true. \"note\" is one short line for the log.";

fn user_message(r: &Request) -> String {
    let mut body = json!({
        "task": match r.kind { Kind::Plan => "plan", Kind::Rescue => "rescue" },
        "goal": r.goal,
        "state": r.state,
    });
    if let Some(s) = &r.subgoal {
        body["subgoal"] = json!(s);
    }
    if let Some(j) = &r.jev {
        body["jev"] = j.clone();
    }
    if let Some(w) = &r.why {
        body["why"] = json!(w);
    }
    if r.need_text {
        body["need_text"] = json!(true);
    }
    body.to_string()
}

fn chat_body(model: &str, r: &Request, json_mode: bool) -> Value {
    let mut b = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": user_message(r)},
        ],
        "temperature": 0.2,
        "max_tokens": 700,
        "usage": {"include": true},
    });
    if json_mode {
        b["response_format"] = json!({"type": "json_object"});
    }
    b
}

async fn chat_once(
    client: &reqwest::Client,
    cfg: &Config,
    r: &Request,
    json_mode: bool,
) -> Result<(String, f64, u64, u64), TwoError> {
    let resp = client
        .post(CHAT_URL)
        .bearer_auth(&cfg.key)
        .header("HTTP-Referer", REFERER)
        .header("X-Title", TITLE)
        .json(&chat_body(&cfg.model, r, json_mode))
        .send()
        .await?;
    let status = resp.status().as_u16();
    let body = resp.text().await?;
    if status >= 300 {
        return Err(TwoError::Status {
            status,
            body: body.chars().take(300).collect(),
        });
    }
    let v: Value = serde_json::from_str(&body).map_err(|e| TwoError::Protocol(e.to_string()))?;
    if let Some(err) = v.get("error") {
        return Err(TwoError::Status {
            status,
            body: err.to_string().chars().take(300).collect(),
        });
    }
    let content = v["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| TwoError::Protocol("no choices[0].message.content".into()))?
        .to_string();
    let usage = &v["usage"];
    Ok((
        content,
        usage["cost"].as_f64().unwrap_or(0.0),
        usage["prompt_tokens"].as_u64().unwrap_or(0),
        usage["completion_tokens"].as_u64().unwrap_or(0),
    ))
}

/// One consultation; JSON mode first, then a plain retry for models that reject
/// `response_format` (they answer 400/404 with a message naming it).
async fn consult(client: &reqwest::Client, cfg: &Config, r: &Request) -> Reply {
    let t0 = Instant::now();
    let mut res = chat_once(client, cfg, r, true).await;
    if let Err(TwoError::Status { status, body }) = &res {
        if (*status == 400 || *status == 404)
            && body.to_ascii_lowercase().contains("response_format")
        {
            res = chat_once(client, cfg, r, false).await;
        }
    }
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    match res {
        Ok((content, cost_usd, p, c)) => Reply {
            kind: r.kind,
            advice: parse_advice(&content),
            model: cfg.model.clone(),
            ms,
            cost_usd,
            prompt_tokens: p,
            completion_tokens: c,
        },
        Err(e) => Reply {
            kind: r.kind,
            advice: Err(e.to_string()),
            model: cfg.model.clone(),
            ms,
            cost_usd: 0.0,
            prompt_tokens: 0,
            completion_tokens: 0,
        },
    }
}

/// The System Two thread handle. Requests are queued; replies are read when the
/// loop wants them. Dropping the handle ends the thread after the current call.
pub struct Advisor {
    tx: Option<Sender<Request>>,
    rx: Receiver<Reply>,
    model: String,
    max_calls: u32,
    calls: u32,
    cost_usd: f64,
}

impl Advisor {
    pub fn spawn(cfg: Config) -> Result<Self, TwoError> {
        let (tx, req_rx) = mpsc::channel::<Request>();
        let (rep_tx, rx) = mpsc::channel::<Reply>();
        let model = cfg.model.clone();
        let max_calls = cfg.max_calls;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let client = reqwest::Client::builder().timeout(cfg.timeout).build()?;
        std::thread::Builder::new()
            .name("uc-two".into())
            .spawn(move || {
                for req in req_rx {
                    let reply = rt.block_on(consult(&client, &cfg, &req));
                    if rep_tx.send(reply).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            tx: Some(tx),
            rx,
            model,
            max_calls,
            calls: 0,
            cost_usd: 0.0,
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }
    pub fn calls(&self) -> u32 {
        self.calls
    }
    pub fn cost_usd(&self) -> f64 {
        self.cost_usd
    }
    pub fn remaining(&self) -> u32 {
        self.max_calls.saturating_sub(self.calls)
    }

    /// Queue a consultation; `false` when the per-run budget is spent.
    pub fn ask(&mut self, req: Request) -> bool {
        if self.remaining() == 0 {
            return false;
        }
        let Some(tx) = &self.tx else {
            return false;
        };
        if tx.send(req).is_err() {
            return false;
        }
        self.calls += 1;
        true
    }

    fn book(&mut self, r: &Reply) {
        self.cost_usd += r.cost_usd;
    }

    /// A reply if one is waiting; never blocks.
    pub fn try_recv(&mut self) -> Option<Reply> {
        match self.rx.try_recv() {
            Ok(r) => {
                self.book(&r);
                Some(r)
            }
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }

    /// Wait up to `d` for a reply — only worth it when the loop has nothing better
    /// to do (Jev already gave up on the step).
    pub fn recv_timeout(&mut self, d: Duration) -> Option<Reply> {
        match self.rx.recv_timeout(d) {
            Ok(r) => {
                self.book(&r);
                Some(r)
            }
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_and_prosed_json() {
        let a = parse_advice(
            "Sure:\n```json\n{\"plan\": [\"a\", \" \", \"b\"], \"note\": \"ok\"}\n```\n",
        )
        .unwrap();
        assert_eq!(a.plan, Some(vec!["a".into(), "b".into()]));
        assert_eq!(a.note.as_deref(), Some("ok"));
        assert!(!a.is_empty());
        let b = parse_advice(
            "{\"next\": {\"op\": \"click\", \"target\": \"e3\"}, \"note\": \"x\"} trailing",
        )
        .unwrap();
        assert_eq!(b.next.unwrap().target.as_deref(), Some("e3"));
    }

    #[test]
    fn empty_and_broken_replies() {
        assert!(parse_advice("no json here").is_err());
        assert!(parse_advice("{\"plan\": null, \"next\": null}")
            .unwrap()
            .is_empty());
        assert!(parse_advice("{\"next\": {\"op\": \"\"}}")
            .unwrap()
            .is_empty());
        assert!(parse_advice("{\"plan\": [\"\"]}").unwrap().is_empty());
    }

    #[test]
    fn model_prices_are_per_mtok_and_sorted() {
        let v = json!({"data": [
            {"id": "z/two", "name": "Two", "context_length": 8000,
             "pricing": {"prompt": "0.000001", "completion": "0.000002"}},
            {"id": "a/one", "name": "One", "context_length": 128000,
             "pricing": {"prompt": "0", "completion": "0"}},
        ]});
        let m = parse_models(&v);
        assert_eq!(m[0].id, "a/one");
        assert!(m[0].is_free());
        assert!((m[1].prompt_usd_per_mtok - 1.0).abs() < 1e-9);
        assert!((m[1].completion_usd_per_mtok - 2.0).abs() < 1e-9);
    }

    /// Live: `cargo test -p uc-two -- --ignored --nocapture` (free GET, no key needed).
    #[test]
    #[ignore]
    fn live_model_catalogue() {
        let key = Config::key_available().and_then(uc_jev::user_env);
        let models = list_models(key.as_deref()).expect("GET /models");
        assert!(models.len() > 50, "only {} models", models.len());
        let default = models
            .iter()
            .find(|m| m.id == "google/gemini-2.5-flash-lite")
            .expect("default model listed");
        println!(
            "{} models; default {} ${:.3}/${:.3} per Mtok, ctx {}",
            models.len(),
            default.id,
            default.prompt_usd_per_mtok,
            default.completion_usd_per_mtok,
            default.context_length
        );
        assert!(default.context_length > 0);
    }

    #[test]
    fn request_body_shape() {
        let r = Request {
            kind: Kind::Rescue,
            goal: "g".into(),
            subgoal: Some("s".into()),
            state: json!({"elements": []}),
            jev: Some(json!({"op": "click"})),
            why: Some("w".into()),
            need_text: true,
        };
        let b = chat_body("m/x", &r, true);
        assert_eq!(b["model"], "m/x");
        assert_eq!(b["response_format"]["type"], "json_object");
        assert_eq!(b["usage"]["include"], true);
        let u: Value = serde_json::from_str(b["messages"][1]["content"].as_str().unwrap()).unwrap();
        assert_eq!(u["task"], "rescue");
        assert_eq!(u["need_text"], true);
        assert_eq!(u["subgoal"], "s");
        let plain = chat_body("m/x", &r, false);
        assert!(plain.get("response_format").is_none());
    }
}
