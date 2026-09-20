//! Client for Jev 1.13 built for a hot loop, not for "a short burst of decisions":
//!
//! * one warm HTTP/2 connection, headers set once, body assembled from pre-serialised
//!   parts (state, questions) — no per-request builder work;
//! * warm-up with a real mini decision (a HEAD on the vendor takes ~600 ms and does not
//!   guarantee a kept-alive connection);
//! * **hedging**: an identical second POST after `hedge_after`; the first response
//!   wins and the loser is cancelled — cost 2× only on the tail;
//! * short timeout (1.5 s default): a step that is late is a step that is wrong.
//!
//! Two providers serve the same model. Vendor first (one hop fewer: −30…−60 ms measured
//! from Poland), OpenRouter as fallback. Only OpenRouter accepts `session_id`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const PRICE_PER_MTOK_USD: f64 = 0.042;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Typesafe,
    OpenRouter,
}

impl Provider {
    pub fn url(self) -> &'static str {
        match self {
            Provider::Typesafe => "https://api.typesafe.ai/v1/systemone",
            Provider::OpenRouter => "https://openrouter.ai/api/alpha/decisions",
        }
    }
    /// Pinned model id — `jev-latest` moves and thresholds are calibrated per version.
    pub fn model(self) -> &'static str {
        match self {
            Provider::Typesafe => "jev-1.13.0",
            Provider::OpenRouter => "typesafe/jev-1.13",
        }
    }
    pub fn key_names(self) -> &'static [&'static str] {
        match self {
            Provider::Typesafe => &["JEV_API_KEY", "TYPESAFE_API_KEY"],
            Provider::OpenRouter => &["OPENROUTER_API_KEY", "JEVUSE_API_KEY"],
        }
    }
    pub fn accepts_session_id(self) -> bool {
        matches!(self, Provider::OpenRouter)
    }
}

/// Read a user environment variable from the process env, then from
/// `HKCU\Environment` (values set after the terminal opened do not inherit).
pub fn user_env(name: &str) -> Option<String> {
    if let Ok(v) = std::env::var(name) {
        if !v.trim().is_empty() {
            return Some(v.trim().to_string());
        }
    }
    registry_env(name)
}

fn registry_env(name: &str) -> Option<String> {
    use windows::core::{w, PCWSTR};
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    let wname: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buf = vec![0u16; 4096];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: buffer and size are consistent; RegGetValueW writes at most `size` bytes.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Environment"),
            PCWSTR(wname.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return None;
    }
    let n = (size as usize / 2).saturating_sub(1);
    let v = String::from_utf16_lossy(&buf[..n]).trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// Which provider has a key on this machine, vendor first.
pub fn discover() -> Option<(Provider, String, &'static str)> {
    for p in [Provider::Typesafe, Provider::OpenRouter] {
        for name in p.key_names() {
            if let Some(k) = user_env(name) {
                return Some((p, k, name));
            }
        }
    }
    None
}

// ------------------------------------------------------------------ questions

pub fn noul(instructions: &str, true_desc: &str, false_desc: &str) -> Value {
    json!({"type": "noul", "instructions": instructions, "criteria": {"true": true_desc, "false": false_desc}})
}

pub fn choice(instructions: &str, criteria: &[(&str, String)]) -> Value {
    let map: serde_json::Map<String, Value> = criteria
        .iter()
        .map(|(k, v)| (k.to_string(), Value::String(v.clone())))
        .collect();
    json!({"type": "choice", "instructions": instructions, "criteria": map})
}

pub fn score(instructions: &str, levels: &[&str]) -> Value {
    json!({"type": "score", "instructions": instructions, "criteria": levels})
}

/// Questions serialised once per snapshot; the hot path only concatenates bytes.
#[derive(Clone, Debug)]
pub struct Compiled {
    pub bytes: Vec<u8>,
    pub names: Vec<String>,
}

impl Compiled {
    pub fn new(questions: &serde_json::Map<String, Value>) -> Self {
        Self {
            bytes: serde_json::to_vec(questions).expect("questions serialise"),
            names: questions.keys().cloned().collect(),
        }
    }
}

// ------------------------------------------------------------------ answers

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        #[serde(default)]
        confidence: f64,
        #[serde(default)]
        probabilities: HashMap<String, f64>,
    },
    Score {
        score: f64,
        #[serde(default)]
        confidence: f64,
        #[serde(default)]
        probabilities: HashMap<String, f64>,
    },
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cost: Option<f64>,
}

#[derive(Clone, Debug, Deserialize)]
struct Raw {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    answers: HashMap<String, Answer>,
    #[serde(default)]
    usage: Usage,
}

#[derive(Clone, Debug)]
pub struct Timing {
    pub http_ms: f64,
    pub total_ms: f64,
    pub hedged: bool,
    /// Which attempt produced the answer: 0 = primary, 1 = hedge.
    pub winner: u8,
}

#[derive(Clone, Debug)]
pub struct Decision {
    pub answers: HashMap<String, Answer>,
    pub model: String,
    pub request_id: String,
    pub usage: Usage,
    pub cost_usd: f64,
    pub timing: Timing,
}

impl Decision {
    pub fn noul(&self, name: &str) -> Option<f64> {
        match self.answers.get(name)? {
            Answer::Noul { noul } => Some(*noul),
            _ => None,
        }
    }
    pub fn choice(&self, name: &str) -> Option<(&str, f64)> {
        match self.answers.get(name)? {
            Answer::Choice {
                choice, confidence, ..
            } => Some((choice.as_str(), *confidence)),
            _ => None,
        }
    }
    pub fn score(&self, name: &str) -> Option<(f64, f64)> {
        match self.answers.get(name)? {
            Answer::Score {
                score, confidence, ..
            } => Some((*score, *confidence)),
            _ => None,
        }
    }
    pub fn probs(&self, name: &str) -> Option<&HashMap<String, f64>> {
        match self.answers.get(name)? {
            Answer::Choice { probabilities, .. } | Answer::Score { probabilities, .. } => {
                Some(probabilities)
            }
            _ => None,
        }
    }
    /// Top-k options by probability.
    pub fn top(&self, name: &str, k: usize) -> Vec<(String, f64)> {
        let mut v: Vec<(String, f64)> = self
            .probs(name)
            .map(|m| m.iter().map(|(a, b)| (a.clone(), *b)).collect())
            .unwrap_or_default();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        v.truncate(k);
        v
    }
    /// Normalised Shannon entropy of a distribution (0 = certain, 1 = uniform).
    pub fn entropy_norm(&self, name: &str) -> Option<f64> {
        let p = self.probs(name)?;
        let k = p.len();
        if k < 2 {
            return Some(0.0);
        }
        let h: f64 = p
            .values()
            .filter(|&&x| x > 0.0)
            .map(|&x| -x * x.log2())
            .sum();
        Some(h / (k as f64).log2())
    }
    /// Gap between the two most probable options.
    pub fn top2_gap(&self, name: &str) -> Option<f64> {
        let t = self.top(name, 2);
        Some(t.first()?.1 - t.get(1).map(|x| x.1).unwrap_or(0.0))
    }
}

// ------------------------------------------------------------------ client

#[derive(Debug, thiserror::Error)]
pub enum JevError {
    #[error("no API key found (looked for JEV_API_KEY, TYPESAFE_API_KEY, OPENROUTER_API_KEY, JEVUSE_API_KEY in env and HKCU\\Environment)")]
    NoKey,
    #[error("HTTP {status}: {body}")]
    Api {
        status: u16,
        body: String,
        retryable: bool,
    },
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("timeout after {0:?}")]
    Timeout(Duration),
    #[error("bad response: {0}")]
    Parse(String),
}

#[derive(Clone, Debug)]
pub struct Config {
    pub provider: Provider,
    pub api_key: String,
    pub timeout: Duration,
    pub hedge_after: Option<Duration>,
    pub session_id: Option<String>,
}

impl Config {
    pub fn discover() -> Result<Self, JevError> {
        let (provider, api_key, _) = discover().ok_or(JevError::NoKey)?;
        Ok(Self {
            provider,
            api_key,
            timeout: Duration::from_millis(1500),
            hedge_after: None,
            session_id: None,
        })
    }
    pub fn for_provider(provider: Provider) -> Result<Self, JevError> {
        let api_key = provider
            .key_names()
            .iter()
            .find_map(|n| user_env(n))
            .ok_or(JevError::NoKey)?;
        Ok(Self {
            provider,
            api_key,
            timeout: Duration::from_millis(1500),
            hedge_after: None,
            session_id: None,
        })
    }
}

pub struct Client {
    cfg: Config,
    http: reqwest::Client,
    prefix: Vec<u8>,
    pub requests_sent: std::sync::atomic::AtomicU64,
}

impl Client {
    pub fn new(cfg: Config) -> Result<Self, JevError> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", cfg.api_key).parse().expect("header"),
        );
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            "application/json".parse().expect("header"),
        );
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(cfg.timeout)
            .connect_timeout(Duration::from_secs(3))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(4)
            .tcp_keepalive(Duration::from_secs(30))
            .tcp_nodelay(true)
            .use_rustls_tls()
            .build()?;
        let prefix = format!("{{\"model\":\"{}\",\"state\":", cfg.provider.model()).into_bytes();
        Ok(Self {
            cfg,
            http,
            prefix,
            requests_sent: std::sync::atomic::AtomicU64::new(0),
        })
    }

    pub fn provider(&self) -> Provider {
        self.cfg.provider
    }

    fn body(&self, state: &[u8], questions: &[u8]) -> Vec<u8> {
        let mut b = Vec::with_capacity(self.prefix.len() + state.len() + questions.len() + 64);
        b.extend_from_slice(&self.prefix);
        b.extend_from_slice(state);
        b.extend_from_slice(b",\"questions\":");
        b.extend_from_slice(questions);
        if let (true, Some(sid)) = (self.cfg.provider.accepts_session_id(), &self.cfg.session_id) {
            b.extend_from_slice(b",\"session_id\":");
            b.extend_from_slice(
                serde_json::to_string(&sid[..sid.len().min(256)])
                    .unwrap_or_default()
                    .as_bytes(),
            );
        }
        b.push(b'}');
        b
    }

    async fn post_once(&self, body: Vec<u8>) -> Result<(Raw, f64), JevError> {
        let t0 = Instant::now();
        self.requests_sent
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let resp = self
            .http
            .post(self.cfg.provider.url())
            .body(body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    JevError::Timeout(self.cfg.timeout)
                } else {
                    JevError::Transport(e)
                }
            })?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await?;
        let http_ms = t0.elapsed().as_secs_f64() * 1000.0;
        if status != 200 {
            let body = String::from_utf8_lossy(&bytes[..bytes.len().min(600)]).to_string();
            return Err(JevError::Api {
                status,
                body,
                retryable: matches!(status, 429 | 502 | 503 | 529),
            });
        }
        let raw: Raw =
            serde_json::from_slice(&bytes).map_err(|e| JevError::Parse(e.to_string()))?;
        Ok((raw, http_ms))
    }

    /// One decision for one state — hedged when configured.
    pub async fn decide(&self, state: &[u8], questions: &Compiled) -> Result<Decision, JevError> {
        let t0 = Instant::now();
        let body = self.body(state, &questions.bytes);
        let (raw, http_ms, winner) = match self.cfg.hedge_after {
            None => {
                let (r, ms) = self.post_once(body).await?;
                (r, ms, 0u8)
            }
            Some(delay) => {
                let primary = self.post_once(body.clone());
                tokio::pin!(primary);
                let hedge_start = tokio::time::sleep(delay);
                tokio::pin!(hedge_start);
                // Phase 1: wait for the primary or for the hedge timer.
                let first = tokio::select! {
                    r = &mut primary => Some(r),
                    _ = &mut hedge_start => None,
                };
                match first {
                    Some(r) => {
                        let (raw, ms) = r?;
                        (raw, ms, 0)
                    }
                    None => {
                        // Phase 2: race the primary against the hedge; first success wins.
                        let hedge = self.post_once(body);
                        tokio::pin!(hedge);
                        tokio::select! {
                            r = &mut primary => match r {
                                Ok((raw, ms)) => (raw, ms, 0),
                                Err(_) => { let (raw, ms) = hedge.await?; (raw, ms, 1) }
                            },
                            r = &mut hedge => match r {
                                Ok((raw, ms)) => (raw, ms, 1),
                                Err(_) => { let (raw, ms) = primary.await?; (raw, ms, 0) }
                            },
                        }
                    }
                }
            }
        };
        let cost_usd = raw
            .usage
            .cost
            .unwrap_or(raw.usage.input_tokens as f64 * PRICE_PER_MTOK_USD / 1_000_000.0);
        Ok(Decision {
            answers: raw.answers,
            model: raw
                .model
                .unwrap_or_else(|| self.cfg.provider.model().to_string()),
            request_id: raw.id.unwrap_or_default(),
            usage: raw.usage,
            cost_usd,
            timing: Timing {
                http_ms,
                total_ms: t0.elapsed().as_secs_f64() * 1000.0,
                hedged: self.cfg.hedge_after.is_some(),
                winner,
            },
        })
    }

    /// Establish DNS+TCP+TLS+h2 with a real, tiny decision (~320 tokens, ~$0.00001).
    pub async fn warm(&self) -> Result<f64, JevError> {
        let mut q = serde_json::Map::new();
        q.insert(
            "ready".into(),
            noul(
                "Is `state.ok` true?",
                "`state.ok` is true.",
                "`state.ok` is false or missing.",
            ),
        );
        let d = self.decide(br#"{"ok":true}"#, &Compiled::new(&q)).await?;
        Ok(d.timing.total_ms)
    }
}
