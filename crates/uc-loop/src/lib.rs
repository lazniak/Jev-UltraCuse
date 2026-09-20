//! Step loop: perceive (UIA) → reduce → Jev fan-out → validate in code → act → settle.
//!
//! Everything that *decides* lives in this crate, in one place, as the vendor advises:
//! question texts and thresholds in [`consts`], the code-side gate in [`policy`]
//! (Jev advises, code decides), the executor in [`exec`] and the MVP runner in
//! [`runner`]. The runner is single-threaded on purpose (COM/STA for UIA on the
//! caller's thread, a tokio runtime for HTTP); the multi-thread layout from
//! `docs/03-architektura.md` is phase 1.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

pub mod consts {
    /// Universal floor from the confidence-routing pattern: below it, never act.
    pub const CONF_FLOOR: f64 = 0.60;
    /// System keys (Win, Alt+F4) and window-level operations.
    pub const CONF_SYSTEM_KEY: f64 = 0.75;
    /// Irreversible actions: threshold AND a spoken confirmation, always.
    pub const CONF_IRREVERSIBLE: f64 = 0.85;
    /// `goal_reached` must clear this AND `op` must say `done` (at `CONF_FLOOR`) before
    /// the loop stops. Lower than the irreversible bar on purpose: a false `done` costs a
    /// re-run, and the same task measured 0.85 / 0.84 on two runs (`bench/R4-mvp-run.md`).
    pub const DONE_THRESHOLD: f64 = 0.75;
    /// Narrow (region → element) instead of accepting when the top-2 gap is below this.
    pub const TOP2_GAP_MIN: f64 = 0.15;
    /// Escalate after this many consecutive steps with normalised entropy above 0.6.
    pub const ENTROPY_MAX: f64 = 0.60;
    pub const ENTROPY_STRIKES: u32 = 2;
    /// `is_destructive` at or above this blocks the action unless explicitly allowed.
    pub const DESTRUCTIVE_BLOCK: f64 = 0.50;
    /// Consecutive uncertain verdicts before the run stops and asks for help.
    pub const UNCERTAIN_STRIKES: u32 = 2;
    /// Consecutive actions with no visible change before the run stops.
    pub const STALL_STRIKES: u32 = 2;
    pub const MAX_STEPS_DEFAULT: usize = 12;
    /// Max candidates sent to Jev (measured stable to 60).
    pub const MAX_CANDIDATES: usize = 60;
    /// Pop-ups of the target process scanned in addition to the foreground window
    /// (a menu and its sub-menu, a drop-down, an owned dialog).
    pub const MAX_POPUPS: usize = 3;
    /// Settle cap after an action (jev-ultrafast: 50 ms / 2 frames; combobox 200 ms).
    /// MVP sleeps this long; phase 1 replaces it with UIA events + hash polling.
    pub const SETTLE_CAP_MS: u64 = 200;
    /// Pause between focusing a field and typing into it.
    pub const FOCUS_SETTLE_MS: u64 = 40;
    /// Length of a `wait` step.
    pub const WAIT_MS: u64 = 300;
    /// Consecutive `wait` steps before the run stops (waits never count as stalls).
    pub const WAIT_STRIKES: u32 = 3;
    pub const SCROLL_NOTCHES: i32 = 3;
    /// Hedge an in-flight decision after this delay. A hedge can only win when the
    /// first request stalls past `delay + floor` (floor ≈ 267 ms from PL); with a
    /// 350–400 ms delay it won 0/8 times in R2 (max of 110 calls: 425 ms), so this is
    /// a stall guard at ~2×p50, not a tail trimmer (`bench/R2-jev.md`).
    pub const HEDGE_AFTER_MS: u64 = 600;
    pub const DECISION_TIMEOUT_MS: u64 = 1500;

    /// Names on controls that make an action irreversible regardless of what Jev says.
    pub const IRREVERSIBLE_MARKERS: [&str; 20] = [
        "usuń",
        "usun",
        "delete",
        "remove",
        "wyślij",
        "wyslij",
        "send",
        "zapłać",
        "zaplac",
        "pay",
        "kup",
        "buy",
        "formatuj",
        "format",
        "uninstall",
        "odinstaluj",
        "wyczyść",
        "wyczysc",
        "drop",
        "truncate",
    ];

    /// Keys that destroy or send regardless of the control under focus.
    pub const DESTRUCTIVE_KEYS: [&str; 2] = ["delete", "shift+delete"];

    pub fn is_irreversible_name(name: &str) -> bool {
        let n = name.to_lowercase();
        IRREVERSIBLE_MARKERS.iter().any(|m| n.contains(m))
    }

    /// The question bundle. Every step asks all of these in one request — questions to
    /// one state run in parallel and cost only their own tokens.
    pub mod q {
        pub const TARGET: &str =
            "Which element in `elements` should be acted on next to advance `goal`?";
        pub const TARGET_NONE: &str =
            "No listed element advances `goal`; a key, scroll or wait is needed.";
        pub const OP: &str = "What kind of input advances `goal` from this screen?";
        pub const OPS: [(&str, &str); 8] = [
            ("click", "Activate a visible control."),
            (
                "right_click",
                "Open the context menu of a control or of the background.",
            ),
            ("type", "Enter `dictated` text into a text field."),
            ("key", "Press a keyboard shortcut (see `key`)."),
            ("scroll_down", "Content needed is below the visible area."),
            ("scroll_up", "Content needed is above the visible area."),
            ("wait", "The UI is still loading or animating."),
            ("done", "`goal` is already satisfied by the visible state."),
        ];
        pub const KEY: &str = "If a key press is the next step, which one?";
        pub const KEYS: [(&str, &str); 10] = [
            ("enter", "Confirm / activate the default button."),
            ("f2", "Rename the selected item."),
            ("delete", "Delete the selected item (hard to undo)."),
            ("esc", "Cancel / close the dialog or menu."),
            ("tab", "Move focus to the next control."),
            ("ctrl+s", "Save."),
            ("ctrl+z", "Undo the last edit."),
            ("down", "Move the selection down."),
            ("up", "Move the selection up."),
            ("none", "No key press is needed."),
        ];
        pub const GOAL: &str = "Given `last` (the previous action and its effect), is `goal` already satisfied by the visible state (`elements`, `scene.title`)?";
        pub const GOAL_TRUE: &str = "The visible state shows `goal` completed; nothing more to do.";
        pub const GOAL_FALSE: &str = "At least one step of `goal` is still pending.";
        /// Inverted phrasing of `GOAL`; the two are averaged (self-consistency), because a
        /// single noul is a weak signal and P(x) ≠ 1 − P(¬x) on this model.
        pub const GOAL_PENDING: &str =
            "Is there still something left to do before `goal` is complete?";
        pub const GOAL_PENDING_TRUE: &str = "Yes — at least one more input is required.";
        pub const GOAL_PENDING_FALSE: &str = "No — `goal` is done as far as the screen shows.";
        pub const NEEDS_TEXT: &str = "Does the next step toward `goal` require typing new text?";
        pub const NEEDS_TEXT_TRUE: &str =
            "A text field must receive content before `goal` can advance.";
        pub const NEEDS_TEXT_FALSE: &str = "No typing is needed for the next step.";
        pub const DESTRUCTIVE: &str = "Would the most likely next action delete data, send a message, pay, or otherwise be hard to undo?";
        pub const DESTRUCTIVE_TRUE: &str = "The next action is irreversible or destructive.";
        pub const DESTRUCTIVE_FALSE: &str = "The next action is safe and reversible.";
        /// Shortlist round: one independent `noul` per finalist instead of a forced
        /// choice — two valid routes (button vs menu) split a `choice` 50/50 forever,
        /// while each of them gates high on its own.
        pub const OK_PREFIX: &str = "Is acting on this control a correct next step toward `goal`:";
        pub const OK_TRUE: &str = "Yes — it advances `goal` from the current screen.";
        pub const OK_FALSE: &str = "No — wrong control, or not yet.";
    }
}

// ------------------------------------------------------------------ questions

pub mod questions {
    use serde_json::{Map, Value};
    use uc_uia::Element;

    use crate::consts::q;

    /// One `choice` criterion per element (`e0`, `e1`, …) plus `none`. Same shape as the
    /// Python reference (`JevUse/questions.py`) so R2 numbers transfer.
    pub fn criteria(elements: &[Element]) -> Vec<(String, String)> {
        let mut c = Vec::with_capacity(elements.len() + 1);
        for e in elements {
            let mut d = format!("{} „{}”", e.role, e.name);
            if let Some(v) = &e.val {
                d.push_str(&format!(" = „{v}”"));
            }
            if !e.enabled {
                d.push_str(" (disabled)");
            }
            c.push((format!("e{}", e.i), d));
        }
        c.push(("none".to_string(), q::TARGET_NONE.to_string()));
        c
    }

    /// The seven questions of one step, keyed by name.
    pub fn bundle(criteria: &[(String, String)]) -> Map<String, Value> {
        let crit: Vec<(&str, String)> = criteria
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();
        let ops: Vec<(&str, String)> = q::OPS.iter().map(|(k, v)| (*k, v.to_string())).collect();
        let keys: Vec<(&str, String)> = q::KEYS.iter().map(|(k, v)| (*k, v.to_string())).collect();
        let mut m = Map::new();
        m.insert("target".into(), uc_jev::choice(q::TARGET, &crit));
        m.insert("op".into(), uc_jev::choice(q::OP, &ops));
        m.insert("key".into(), uc_jev::choice(q::KEY, &keys));
        m.insert(
            "goal_reached".into(),
            uc_jev::noul(q::GOAL, q::GOAL_TRUE, q::GOAL_FALSE),
        );
        m.insert(
            "goal_pending".into(),
            uc_jev::noul(q::GOAL_PENDING, q::GOAL_PENDING_TRUE, q::GOAL_PENDING_FALSE),
        );
        m.insert(
            "needs_text".into(),
            uc_jev::noul(q::NEEDS_TEXT, q::NEEDS_TEXT_TRUE, q::NEEDS_TEXT_FALSE),
        );
        m.insert(
            "is_destructive".into(),
            uc_jev::noul(q::DESTRUCTIVE, q::DESTRUCTIVE_TRUE, q::DESTRUCTIVE_FALSE),
        );
        m
    }

    pub fn compile(elements: &[Element]) -> uc_jev::Compiled {
        uc_jev::Compiled::new(&bundle(&criteria(elements)))
    }
}

// ------------------------------------------------------------------ policy

pub mod policy {
    use serde::Serialize;
    use uc_uia::Element;

    use crate::consts::*;

    /// What the executor can do. Coordinates are physical pixels.
    #[derive(Clone, Debug, PartialEq, Serialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum Action {
        Click {
            target: usize,
            name: String,
            x: i32,
            y: i32,
            /// Context menu instead of activation.
            right: bool,
        },
        Type {
            text: String,
            focus: Option<(i32, i32)>,
            target_name: Option<String>,
        },
        Key {
            key: String,
        },
        Scroll {
            notches: i32,
            at: Option<(i32, i32)>,
        },
        Wait,
    }

    #[derive(Clone, Debug, PartialEq, Serialize)]
    #[serde(tag = "verdict", rename_all = "snake_case")]
    pub enum Verdict {
        Done,
        Act {
            action: Action,
        },
        /// `narrow`: candidate ids worth one narrower re-ask (the shortlist pattern).
        Uncertain {
            reason: String,
            narrow: Option<Vec<String>>,
        },
        NeedsText,
        Blocked {
            reason: String,
        },
    }

    /// The numbers one step reads out of a [`uc_jev::Decision`] — everything the gate
    /// needs, nothing else, so the gate is testable without a network.
    #[derive(Clone, Debug, Default, Serialize)]
    pub struct Signals {
        pub target: String,
        pub target_name: Option<String>,
        /// Top-3 `(id, probability)` of `target`, for the duplicate-name merge.
        pub target_top: Vec<(String, f64)>,
        pub target_conf: f64,
        pub target_gap: f64,
        pub target_entropy: f64,
        pub op: String,
        pub op_conf: f64,
        pub key: String,
        pub key_conf: f64,
        /// Mean of `goal_reached` and `1 − goal_pending` (two phrasings, one call).
        pub goal_reached: f64,
        pub goal_a: f64,
        pub goal_pending: f64,
        pub needs_text: f64,
        pub is_destructive: f64,
    }

    /// `e12` → `12`.
    pub fn parse_target(id: &str) -> Option<usize> {
        id.strip_prefix('e').and_then(|n| n.parse().ok())
    }

    pub fn find(els: &[Element], i: usize) -> Option<&Element> {
        els.iter().find(|e| e.i == i)
    }

    pub fn describe(e: &Element) -> String {
        format!("{} „{}”", e.role, e.name)
    }

    impl Signals {
        pub fn from_decision(d: &uc_jev::Decision, els: &[Element]) -> Self {
            let (target, target_conf) = d
                .choice("target")
                .map(|(c, p)| (c.to_string(), p))
                .unwrap_or_else(|| ("none".to_string(), 0.0));
            let target_name = parse_target(&target)
                .and_then(|i| find(els, i))
                .map(describe);
            let (op, op_conf) = d
                .choice("op")
                .map(|(c, p)| (c.to_string(), p))
                .unwrap_or_else(|| ("wait".to_string(), 0.0));
            let (key, key_conf) = d
                .choice("key")
                .map(|(c, p)| (c.to_string(), p))
                .unwrap_or_else(|| ("none".to_string(), 0.0));
            let goal_a = d.noul("goal_reached").unwrap_or(0.0);
            let goal_pending = d.noul("goal_pending").unwrap_or(1.0);
            Self {
                target,
                target_name,
                target_top: d.top("target", 3),
                target_conf,
                target_gap: d.top2_gap("target").unwrap_or(0.0),
                target_entropy: d.entropy_norm("target").unwrap_or(1.0),
                op,
                op_conf,
                key,
                key_conf,
                goal_reached: (goal_a + (1.0 - goal_pending)) / 2.0,
                goal_a,
                goal_pending,
                needs_text: d.noul("needs_text").unwrap_or(0.0),
                is_destructive: d.noul("is_destructive").unwrap_or(0.0),
            }
        }
    }

    fn unsure(reason: String) -> Verdict {
        Verdict::Uncertain {
            reason,
            narrow: None,
        }
    }

    /// The two strongest *listed* candidates, if the call is between them (not `none`).
    fn shortlist(s: &Signals) -> Option<Vec<String>> {
        let ids: Vec<String> = s
            .target_top
            .iter()
            .take(2)
            .map(|(id, _)| id.clone())
            .filter(|id| id != "none")
            .collect();
        (ids.len() == 2).then_some(ids)
    }

    /// The gate. Pure code over the signals: thresholds, agreement between independent
    /// questions, the irreversible list. Jev never gets the last word on an action.
    pub fn judge(
        s: &Signals,
        els: &[Element],
        dictated: Option<&str>,
        allow_irreversible: bool,
    ) -> Verdict {
        let goal_done = s.goal_reached >= DONE_THRESHOLD;
        if goal_done && s.op == "done" {
            if s.op_conf < CONF_FLOOR {
                return unsure(format!(
                    "done at op confidence {:.2} < {CONF_FLOOR}",
                    s.op_conf
                ));
            }
            return Verdict::Done;
        }
        if goal_done || s.op == "done" {
            return unsure(format!(
                "goal_reached {:.2} and op {} disagree",
                s.goal_reached, s.op
            ));
        }
        if s.op_conf < CONF_FLOOR {
            return unsure(format!("op {} at {:.2} < {CONF_FLOOR}", s.op, s.op_conf));
        }
        let target_el = parse_target(&s.target).and_then(|i| find(els, i));
        let (target_conf, target_gap) = (s.target_conf, s.target_gap);
        let target_ok = target_el.is_some_and(|e| e.enabled)
            && target_conf >= CONF_FLOOR
            && target_gap >= TOP2_GAP_MIN;
        let unsure_target = |reason: String| Verdict::Uncertain {
            reason,
            narrow: shortlist(s),
        };
        let destructive = s.is_destructive >= DESTRUCTIVE_BLOCK
            || target_el.is_some_and(|e| is_irreversible_name(&e.name))
            || (s.op == "key" && DESTRUCTIVE_KEYS.contains(&s.key.as_str()));
        let blocked = |what: &str| {
            Verdict::Blocked {
            reason: format!(
                "{what} looks irreversible (is_destructive {:.2}, target {}); pass --allow-irreversible",
                s.is_destructive,
                s.target_name.as_deref().unwrap_or("none")
            ),
        }
        };
        match s.op.as_str() {
            "wait" => Verdict::Act {
                action: Action::Wait,
            },
            "scroll_down" | "scroll_up" => Verdict::Act {
                action: Action::Scroll {
                    notches: if s.op == "scroll_down" {
                        SCROLL_NOTCHES
                    } else {
                        -SCROLL_NOTCHES
                    },
                    at: target_el.map(Element::center),
                },
            },
            "key" => {
                if s.key == "none" || s.key_conf < CONF_FLOOR {
                    return unsure(format!("key {} at {:.2}", s.key, s.key_conf));
                }
                if destructive {
                    if !allow_irreversible {
                        return blocked(&format!("key {}", s.key));
                    }
                    if s.key_conf < CONF_IRREVERSIBLE {
                        return unsure(format!(
                            "irreversible key {} at {:.2} < {CONF_IRREVERSIBLE}",
                            s.key, s.key_conf
                        ));
                    }
                }
                Verdict::Act {
                    action: Action::Key { key: s.key.clone() },
                }
            }
            "type" => {
                let Some(text) = dictated else {
                    return Verdict::NeedsText;
                };
                if destructive {
                    if !allow_irreversible {
                        return blocked("typing here");
                    }
                    if s.op_conf < CONF_IRREVERSIBLE {
                        return unsure(format!(
                            "irreversible typing at op confidence {:.2} < {CONF_IRREVERSIBLE}",
                            s.op_conf
                        ));
                    }
                }
                Verdict::Act {
                    action: Action::Type {
                        text: text.to_string(),
                        focus: if target_ok {
                            target_el.map(Element::center)
                        } else {
                            None
                        },
                        target_name: if target_ok {
                            s.target_name.clone()
                        } else {
                            None
                        },
                    },
                }
            }
            "click" | "right_click" => {
                let Some(el) = target_el else {
                    return unsure("click without a listed target".into());
                };
                if !el.enabled {
                    return unsure(format!("target {} is disabled", describe(el)));
                }
                if !target_ok {
                    return unsure_target(format!(
                        "target {} at {:.2}, gap {:.2}",
                        s.target, target_conf, target_gap
                    ));
                }
                if destructive {
                    if !allow_irreversible {
                        return blocked(&format!("click {}", describe(el)));
                    }
                    if target_conf < CONF_IRREVERSIBLE {
                        return unsure_target(format!(
                            "irreversible click at {:.2} < {CONF_IRREVERSIBLE}",
                            target_conf
                        ));
                    }
                }
                let (x, y) = el.center();
                Verdict::Act {
                    action: Action::Click {
                        target: el.i,
                        name: describe(el),
                        x,
                        y,
                        right: s.op == "right_click",
                    },
                }
            }
            other => unsure(format!("unknown op {other}")),
        }
    }
}

// ------------------------------------------------------------------ exec

pub mod exec {
    use std::time::Duration;

    use uc_input::Button;

    use crate::consts::{FOCUS_SETTLE_MS, WAIT_MS};
    use crate::policy::Action;

    /// Inject one action. Every branch is a single `SendInput` batch (plus a clipboard
    /// paste for long text); no allocation happens between the call and the syscall
    /// beyond what `uc-input` already does.
    pub fn perform(a: &Action) -> Result<(), uc_input::InputError> {
        match a {
            Action::Click { x, y, right, .. } => {
                uc_input::click(*x, *y, if *right { Button::Right } else { Button::Left }, 1)?;
            }
            Action::Type { text, focus, .. } => {
                if let Some((x, y)) = focus {
                    uc_input::click(*x, *y, Button::Left, 1)?;
                    std::thread::sleep(Duration::from_millis(FOCUS_SETTLE_MS));
                }
                uc_input::type_auto(text)?;
            }
            Action::Key { key } => {
                let parts: Vec<&str> = key.split('+').collect();
                if parts.len() == 1 {
                    uc_input::press(parts[0])?;
                } else {
                    uc_input::hotkey(&parts)?;
                }
            }
            Action::Scroll { notches, at } => {
                uc_input::scroll(*notches, *at)?;
            }
            Action::Wait => std::thread::sleep(Duration::from_millis(WAIT_MS)),
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ text helpers

/// The first quoted fragment of a goal — „…”, "…", '…' or «…» — is the text to type.
/// Jev never generates text; it comes from the user (typed here, dictated later).
pub fn extract_quoted(goal: &str) -> Option<String> {
    const PAIRS: [(char, char); 5] = [('„', '”'), ('„', '"'), ('"', '"'), ('\'', '\''), ('«', '»')];
    let mut best: Option<(usize, String)> = None;
    for (open, close) in PAIRS {
        // An apostrophe is a quote only at a word boundary: "Don't save" has none.
        let strict = open == '\'';
        let Some(start) = goal
            .match_indices(open)
            .map(|(i, _)| i)
            .find(|&i| !strict || at_word_start(goal, i))
        else {
            continue;
        };
        let inner_start = start + open.len_utf8();
        let Some(end) = goal[inner_start..]
            .match_indices(close)
            .map(|(i, _)| inner_start + i)
            .find(|&i| !strict || at_word_end(goal, i + close.len_utf8()))
        else {
            continue;
        };
        let text = goal[inner_start..end].trim();
        if text.is_empty() {
            continue;
        }
        if best.as_ref().is_none_or(|(s, _)| start < *s) {
            best = Some((start, text.to_string()));
        }
    }
    best.map(|(_, t)| t)
}

/// A quote character opens a quotation only after start-of-text or a non-letter…
fn at_word_start(s: &str, i: usize) -> bool {
    s[..i]
        .chars()
        .next_back()
        .is_none_or(|c| !c.is_alphanumeric())
}

/// …and closes one only before end-of-text or a non-letter.
fn at_word_end(s: &str, i: usize) -> bool {
    s[i..].chars().next().is_none_or(|c| !c.is_alphanumeric())
}

fn slug(goal: &str) -> String {
    let mut s: String = goal
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    s.trim_matches('-').chars().take(40).collect()
}

fn ts_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

// ------------------------------------------------------------------ runner

#[derive(Debug, thiserror::Error)]
pub enum LoopError {
    #[error("UI Automation init failed: {0}")]
    Uia(String),
    #[error("no foreground window")]
    NoForeground,
    #[error(transparent)]
    Jev(#[from] uc_jev::JevError),
    #[error(transparent)]
    Input(#[from] uc_input::InputError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// One step's view of the target: the foreground window plus its pop-ups, reduced.
pub struct Perception {
    pub scan: uc_uia::Scan,
    pub reduced: Vec<uc_uia::Element>,
    /// Pop-up windows of the target process merged into `scan` (menus, drop-downs,
    /// owned dialogs).
    pub popups: usize,
}

/// Scan `scene`'s window and every pop-up of its process, then reduce. Menus,
/// drop-downs and owned dialogs live in their own top-level windows that never take
/// the foreground, so a scan of the foreground alone would miss them; the viewport
/// grows to cover them because a context menu often hangs outside the window.
pub fn perceive(
    scanner: &uc_uia::UiaScanner,
    scene: &uc_win32::Scene,
    include_context: bool,
    max_n: usize,
) -> Perception {
    let mut scan = scanner.scan_hwnd(scene.hwnd(), include_context);
    let mut viewport = scene.rect;
    let mut popups = 0usize;
    // Pop-up elements go *first*: `reduce` dedupes on (role, name) keeping the first
    // occurrence, and a menu's "Delete" must win over a same-named ribbon button.
    let mut front: Vec<uc_uia::Element> = Vec::new();
    for (h, r) in uc_win32::popups_of(scene.pid, scene.hwnd(), consts::MAX_POPUPS) {
        let extra = scanner.scan_hwnd(h, include_context);
        if extra.elements.is_empty() {
            continue;
        }
        popups += 1;
        scan.raw_count += extra.raw_count;
        scan.total_ms += extra.total_ms;
        viewport = uc_win32::rect_union(viewport, r);
        front.extend(extra.elements);
    }
    if !front.is_empty() {
        front.append(&mut scan.elements);
        scan.elements = front;
    }
    let reduced = uc_uia::reduce(
        &scan.elements,
        uc_uia::ReduceOpts {
            max_n,
            near: Some(scene.cursor),
            viewport: Some(viewport),
            ..Default::default()
        },
    );
    Perception {
        scan,
        reduced,
        popups,
    }
}

#[derive(Clone, Debug)]
pub struct RunOpts {
    /// Inject input. Off = show the first decision and stop (preview).
    pub act: bool,
    pub max_steps: usize,
    pub allow_irreversible: bool,
    /// Text for `type` steps.
    pub dictated: Option<String>,
    /// Directory for the JSONL ledger (one line per step + a summary line).
    pub ledger_dir: Option<PathBuf>,
    /// `None` = discover (vendor first).
    pub provider: Option<uc_jev::Provider>,
    /// Only act while this process owns the foreground window (`None` = lock onto
    /// whatever is in front at the first step). Dialogs of the same process pass.
    pub target_pid: Option<u32>,
    /// The window the run started on (`None` = the foreground window at step 1);
    /// when it stops existing the run ends with [`Outcome::TargetGone`].
    pub target_hwnd: Option<isize>,
    /// Cooperative stop from another thread (a Stop button); checked wherever the
    /// kill switch is.
    pub stop: Option<Arc<AtomicBool>>,
}

impl Default for RunOpts {
    fn default() -> Self {
        Self {
            act: false,
            max_steps: consts::MAX_STEPS_DEFAULT,
            allow_irreversible: false,
            dictated: None,
            ledger_dir: None,
            provider: None,
            target_pid: None,
            target_hwnd: None,
            stop: None,
        }
    }
}

/// One step, as printed and as written to the ledger.
#[derive(Clone, Debug, Serialize)]
pub struct StepRecord {
    pub step: usize,
    pub ts_unix: u64,
    pub app: String,
    pub title: String,
    pub raw_elements: usize,
    /// Pop-up windows of the target process (menus, drop-downs, dialogs) merged into
    /// the scan.
    pub popups: usize,
    pub sent_elements: usize,
    pub est_tokens: usize,
    pub tree_hash: u64,
    /// `None` on the first step; afterwards whether the hash moved since the last action.
    pub changed: Option<bool>,
    pub scan_ms: f64,
    pub jev_ms: f64,
    pub act_ms: f64,
    pub total_ms: f64,
    pub input_tokens: u64,
    pub cost_usd: f64,
    pub hedged_winner: u8,
    /// A second, top-2-only ask happened inside this step.
    pub narrowed: bool,
    /// The signals of the first ask when the step was narrowed (`signals` = final).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signals_first: Option<policy::Signals>,
    pub signals: policy::Signals,
    pub verdict: policy::Verdict,
    pub executed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "outcome", content = "detail", rename_all = "snake_case")]
pub enum Outcome {
    Done,
    /// Preview mode: the first action was shown, nothing injected.
    Preview,
    Budget,
    Uncertain(String),
    NeedsText,
    Blocked(String),
    Stalled,
    /// Ctrl+Alt+K.
    Killed,
    /// The caller asked to stop (`RunOpts::stop`).
    Stopped,
    /// Another process took the foreground; nothing was injected into it.
    FocusLost(String),
    /// The window the run started on no longer exists (closed by the last action or
    /// by someone else); code cannot tell whether that was the goal, so it is reported
    /// neutrally and the caller decides.
    TargetGone,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunSummary {
    pub goal: String,
    pub outcome: Outcome,
    pub steps: usize,
    pub elapsed_ms: f64,
    pub jev_calls: u64,
    pub cost_usd: f64,
    pub ledger: Option<PathBuf>,
}

pub type StepHook = Box<dyn FnMut(&StepRecord)>;

/// The MVP loop. Create on the thread that will run it (UIA initialises COM/STA there).
pub struct Runner {
    opts: RunOpts,
    scanner: uc_uia::UiaScanner,
    rt: tokio::runtime::Runtime,
    client: uc_jev::Client,
    /// Called after every step, before the ledger write.
    pub on_step: Option<StepHook>,
}

impl Runner {
    pub fn new(opts: RunOpts) -> Result<Self, LoopError> {
        let scanner = uc_uia::UiaScanner::new().map_err(|e| LoopError::Uia(e.to_string()))?;
        let mut cfg = match opts.provider {
            Some(p) => uc_jev::Config::for_provider(p)?,
            None => uc_jev::Config::discover()?,
        };
        cfg.timeout = Duration::from_millis(consts::DECISION_TIMEOUT_MS);
        cfg.hedge_after = Some(Duration::from_millis(consts::HEDGE_AFTER_MS));
        let client = uc_jev::Client::new(cfg)?;
        let rt = tokio::runtime::Runtime::new()?;
        Ok(Self {
            opts,
            scanner,
            rt,
            client,
            on_step: None,
        })
    }

    pub fn provider(&self) -> uc_jev::Provider {
        self.client.provider()
    }

    fn stop_requested(&self) -> bool {
        self.opts
            .stop
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Relaxed))
    }

    /// Open the HTTP/2 connection with one tiny decision; returns its latency in ms.
    pub fn warm(&self) -> Result<f64, LoopError> {
        Ok(self.rt.block_on(self.client.warm())?)
    }

    pub fn run(&mut self, goal: &str) -> Result<RunSummary, LoopError> {
        let t_run = Instant::now();
        let mut ledger = match &self.opts.ledger_dir {
            Some(dir) => Some(open_ledger(dir, goal)?),
            None => None,
        };
        let dictated = self.opts.dictated.clone();
        let mut last: Option<Value> = None;
        let mut last_hash: Option<u64> = None;
        let mut uncertain = 0u32;
        let mut entropy_strikes = 0u32;
        let mut stall = 0u32;
        let mut waits = 0u32;
        let mut last_was_wait = false;
        let mut steps = 0usize;
        let mut jev_calls = 0u64;
        let mut cost = 0.0f64;
        let mut locked_pid = self.opts.target_pid;
        let mut target_hwnd = self.opts.target_hwnd;

        let outcome = loop {
            if steps >= self.opts.max_steps {
                break Outcome::Budget;
            }
            if uc_win32::kill_switch_pressed() {
                break Outcome::Killed;
            }
            if self.stop_requested() {
                break Outcome::Stopped;
            }
            steps += 1;
            let t0 = Instant::now();

            // 1. Perceive: coarse scene in µs, UIA tree in ms (the app's provider decides).
            let scene = uc_win32::scene().ok_or(LoopError::NoForeground)?;
            if let Some(h) = target_hwnd {
                if !uc_win32::is_window(uc_win32::HWND(h as *mut core::ffi::c_void)) {
                    break Outcome::TargetGone;
                }
            } else {
                target_hwnd = Some(scene.hwnd);
            }
            match locked_pid {
                None => locked_pid = Some(scene.pid),
                Some(pid) if pid != scene.pid => {
                    break Outcome::FocusLost(format!(
                        "foreground is now {} (pid {}), locked on pid {pid}",
                        scene.app, scene.pid
                    ));
                }
                Some(_) => {}
            }
            let Perception {
                scan,
                reduced,
                popups,
            } = perceive(&self.scanner, &scene, false, consts::MAX_CANDIDATES);
            let hash = uc_uia::tree_hash(&reduced);
            let scan_ms = ms(t0);

            // 2. Did the last action change anything? Code compares states, not Jev.
            let changed = last_hash.map(|h| h != hash);
            match changed {
                Some(false) => {
                    if !last_was_wait {
                        stall += 1;
                    }
                    if let Some(l) = last.as_mut() {
                        l["effect"] = json!("no visible change");
                    }
                }
                Some(true) => {
                    stall = 0;
                    if let Some(l) = last.as_mut() {
                        l["effect"] = json!("screen changed");
                    }
                }
                None => {}
            }
            if stall >= consts::STALL_STRIKES {
                break Outcome::Stalled;
            }

            // 3. Decide: one request, six questions.
            let state = uc_uia::GuiState {
                goal,
                scene: &scene,
                elements: &reduced,
                last: last.clone(),
                dictated: dictated.as_deref(),
            };
            let est_tokens = state.estimate_tokens();
            let state_bytes = serde_json::to_vec(&state)?;
            let compiled = questions::compile(&reduced);
            let t_jev = Instant::now();
            let decision = self
                .rt
                .block_on(self.client.decide(&state_bytes, &compiled))?;
            let mut jev_ms = ms(t_jev);
            jev_calls += 1;
            cost += decision.cost_usd;

            // 4. Gate in code — and on a close call between listed candidates, ask once
            //    more over the top two only (shortlist pattern), still inside this step.
            let mut step_tokens = decision.usage.input_tokens;
            let mut step_cost = decision.cost_usd;
            let mut signals = policy::Signals::from_decision(&decision, &reduced);
            let mut verdict = policy::judge(
                &signals,
                &reduced,
                dictated.as_deref(),
                self.opts.allow_irreversible,
            );
            let mut narrowed = false;
            let mut signals_first: Option<policy::Signals> = None;
            if let policy::Verdict::Uncertain {
                narrow: Some(ids), ..
            } = &verdict
            {
                let mut q = serde_json::Map::new();
                for id in ids {
                    if let Some(el) =
                        policy::parse_target(id).and_then(|i| policy::find(&reduced, i))
                    {
                        q.insert(
                            format!("ok_{id}"),
                            uc_jev::noul(
                                &format!(
                                    "{} {} ({id})",
                                    consts::q::OK_PREFIX,
                                    policy::describe(el)
                                ),
                                consts::q::OK_TRUE,
                                consts::q::OK_FALSE,
                            ),
                        );
                    }
                }
                let compiled2 = uc_jev::Compiled::new(&q);
                let t2 = Instant::now();
                let d2 = self
                    .rt
                    .block_on(self.client.decide(&state_bytes, &compiled2))?;
                jev_ms += ms(t2);
                jev_calls += 1;
                cost += d2.cost_usd;
                step_tokens += d2.usage.input_tokens;
                step_cost += d2.cost_usd;
                let mut scored: Vec<(String, f64)> = ids
                    .iter()
                    .map(|id| (id.clone(), d2.noul(&format!("ok_{id}")).unwrap_or(0.0)))
                    .collect();
                scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                let mut gated = signals.clone();
                if let Some((id, p)) = scored.first() {
                    gated.target = id.clone();
                    gated.target_conf = *p;
                    // Resolved by independent gates, not by a margin between them.
                    gated.target_gap = 1.0;
                    gated.target_entropy = 0.0;
                    gated.target_name = policy::parse_target(id)
                        .and_then(|i| policy::find(&reduced, i))
                        .map(policy::describe);
                    gated.target_top = scored.clone();
                }
                signals_first = Some(std::mem::replace(&mut signals, gated));
                verdict = policy::judge(
                    &signals,
                    &reduced,
                    dictated.as_deref(),
                    self.opts.allow_irreversible,
                );
                narrowed = true;
            }

            let mut rec = StepRecord {
                step: steps,
                ts_unix: ts_unix(),
                app: scene.app.clone(),
                title: scene.title.clone(),
                raw_elements: scan.raw_count,
                popups,
                sent_elements: reduced.len(),
                est_tokens,
                tree_hash: hash,
                changed,
                scan_ms,
                jev_ms,
                act_ms: 0.0,
                total_ms: 0.0,
                input_tokens: step_tokens,
                cost_usd: step_cost,
                hedged_winner: decision.timing.winner,
                narrowed,
                signals_first,
                signals,
                verdict: verdict.clone(),
                executed: false,
            };

            // 5. Act (or not).
            let mut next: Option<Outcome> = None;
            match &verdict {
                policy::Verdict::Done => next = Some(Outcome::Done),
                policy::Verdict::NeedsText => next = Some(Outcome::NeedsText),
                policy::Verdict::Blocked { reason } => {
                    next = Some(Outcome::Blocked(reason.clone()))
                }
                policy::Verdict::Uncertain { reason, .. } => {
                    uncertain += 1;
                    if uncertain >= consts::UNCERTAIN_STRIKES {
                        next = Some(Outcome::Uncertain(reason.clone()));
                    } else {
                        last =
                            Some(json!({"op": "look", "effect": format!("uncertain: {reason}")}));
                        last_hash = None;
                    }
                }
                policy::Verdict::Act { action } => {
                    uncertain = 0;
                    // The narrowed round reports the gates' entropy (0); the guard must
                    // look at the first, full distribution.
                    let entropy = rec
                        .signals_first
                        .as_ref()
                        .unwrap_or(&rec.signals)
                        .target_entropy;
                    if matches!(action, policy::Action::Click { .. }) {
                        if entropy > consts::ENTROPY_MAX {
                            entropy_strikes += 1;
                        } else {
                            entropy_strikes = 0;
                        }
                    }
                    if entropy_strikes >= consts::ENTROPY_STRIKES {
                        next = Some(Outcome::Uncertain(format!(
                            "target entropy {entropy:.2} for {entropy_strikes} steps"
                        )));
                    } else if !self.opts.act {
                        next = Some(Outcome::Preview);
                    } else if uc_win32::kill_switch_pressed() {
                        next = Some(Outcome::Killed);
                    } else if self.stop_requested() {
                        next = Some(Outcome::Stopped);
                    } else if !focus_still_ours(locked_pid, target_hwnd) {
                        // Deciding took 0.3–2 s; a toast, a UAC prompt or an Alt-Tab may
                        // have moved the foreground meanwhile. Never inject blind.
                        next = Some(Outcome::FocusLost(
                            "foreground changed while deciding".into(),
                        ));
                    } else {
                        let t_act = Instant::now();
                        exec::perform(action)?;
                        rec.act_ms = ms(t_act);
                        rec.executed = true;
                        last = Some(action_summary(action));
                        last_hash = Some(hash);
                        last_was_wait = matches!(action, policy::Action::Wait);
                        if last_was_wait {
                            waits += 1;
                            if waits >= consts::WAIT_STRIKES {
                                next = Some(Outcome::Stalled);
                            }
                        } else {
                            waits = 0;
                        }
                        std::thread::sleep(Duration::from_millis(consts::SETTLE_CAP_MS));
                    }
                }
            }
            rec.total_ms = ms(t0);
            if let Some(hook) = self.on_step.as_mut() {
                hook(&rec);
            }
            if let Some((f, _)) = ledger.as_mut() {
                writeln!(f, "{}", serde_json::to_string(&rec)?)?;
            }
            if let Some(o) = next {
                break o;
            }
        };

        let summary = RunSummary {
            goal: goal.to_string(),
            outcome,
            steps,
            elapsed_ms: ms(t_run),
            jev_calls,
            cost_usd: cost,
            ledger: ledger.as_ref().map(|(_, p)| p.clone()),
        };
        if let Some((f, _)) = ledger.as_mut() {
            writeln!(f, "{}", serde_json::to_string(&summary)?)?;
        }
        Ok(summary)
    }
}

/// Is the locked process still in front and the target window still alive? Checked
/// at the scan and again right before injecting.
fn focus_still_ours(locked_pid: Option<u32>, target_hwnd: Option<isize>) -> bool {
    let alive = target_hwnd
        .is_none_or(|h| uc_win32::is_window(uc_win32::HWND(h as *mut core::ffi::c_void)));
    let fg_pid = uc_win32::foreground_hwnd().map(uc_win32::window_pid);
    alive && fg_pid.is_some() && fg_pid == locked_pid
}

fn open_ledger(dir: &Path, goal: &str) -> Result<(std::fs::File, PathBuf), LoopError> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}-{}.jsonl", ts_unix(), slug(goal)));
    Ok((std::fs::File::create(&path)?, path))
}

/// What the next state says about the previous action (`last` in the Jev state).
fn action_summary(a: &policy::Action) -> Value {
    match a {
        policy::Action::Click { name, right, .. } => {
            json!({"op": if *right { "right_click" } else { "click" }, "target": name})
        }
        policy::Action::Type {
            text, target_name, ..
        } => json!({"op": "type", "text": text, "target": target_name}),
        policy::Action::Key { key } => json!({"op": "key", "key": key}),
        policy::Action::Scroll { notches, .. } => {
            json!({"op": if *notches > 0 { "scroll_down" } else { "scroll_up" }})
        }
        policy::Action::Wait => json!({"op": "wait"}),
    }
}

#[cfg(test)]
mod tests {
    use super::policy::{judge, Action, Signals, Verdict};
    use super::*;
    use uc_uia::Element;

    fn el(i: usize, role: &str, name: &str) -> Element {
        Element {
            i,
            role: role.into(),
            name: name.into(),
            bbox: [100 * i as i32, 50, 80, 30],
            enabled: true,
            val: None,
            focused: false,
            auto_id: None,
        }
    }

    fn sig(op: &str, target: &str, conf: f64, gap: f64) -> Signals {
        Signals {
            target: target.into(),
            target_conf: conf,
            target_gap: gap,
            target_entropy: 0.1,
            op: op.into(),
            op_conf: 0.9,
            key: "none".into(),
            key_conf: 0.9,
            ..Default::default()
        }
    }

    #[test]
    fn done_needs_both_signals() {
        let els = [el(0, "button", "OK")];
        let mut s = sig("done", "none", 0.9, 0.9);
        s.goal_reached = 0.95;
        assert_eq!(judge(&s, &els, None, false), Verdict::Done);
        s.goal_reached = 0.4;
        assert!(matches!(
            judge(&s, &els, None, false),
            Verdict::Uncertain { .. }
        ));
        let mut s = sig("click", "e0", 0.9, 0.9);
        s.goal_reached = 0.95;
        assert!(matches!(
            judge(&s, &els, None, false),
            Verdict::Uncertain { .. }
        ));
        let mut s = sig("done", "none", 0.9, 0.9);
        s.goal_reached = 0.95;
        s.op_conf = 0.35;
        assert!(matches!(
            judge(&s, &els, None, false),
            Verdict::Uncertain { .. }
        ));
    }

    #[test]
    fn disabled_target_is_never_acted_on() {
        let mut els = [el(0, "button", "Zapisz")];
        els[0].enabled = false;
        assert!(matches!(
            judge(&sig("click", "e0", 0.95, 0.9), &els, None, false),
            Verdict::Uncertain { .. }
        ));
        match judge(&sig("type", "e0", 0.95, 0.9), &els, Some("x"), false) {
            Verdict::Act {
                action: Action::Type { focus, .. },
            } => assert_eq!(focus, None),
            other => panic!("expected type without focus, got {other:?}"),
        }
    }

    #[test]
    fn destructive_typing_needs_the_irreversible_bar() {
        let els = [el(0, "edit", "Name")];
        let mut s = sig("type", "e0", 0.9, 0.9);
        s.is_destructive = 0.7;
        assert!(matches!(
            judge(&s, &els, Some("x"), false),
            Verdict::Blocked { .. }
        ));
        s.op_conf = 0.7;
        assert!(matches!(
            judge(&s, &els, Some("x"), true),
            Verdict::Uncertain { .. }
        ));
        s.op_conf = 0.9;
        assert!(matches!(
            judge(&s, &els, Some("x"), true),
            Verdict::Act { .. }
        ));
    }

    #[test]
    fn click_gates_on_confidence_gap_and_irreversibility() {
        let els = [el(0, "button", "Save"), el(1, "button", "Delete")];
        match judge(&sig("click", "e0", 0.93, 0.88), &els, None, false) {
            Verdict::Act {
                action: Action::Click { target, x, y, .. },
            } => {
                assert_eq!(target, 0);
                assert_eq!((x, y), (40, 65));
            }
            other => panic!("expected click, got {other:?}"),
        }
        assert!(matches!(
            judge(&sig("click", "e0", 0.55, 0.5), &els, None, false),
            Verdict::Uncertain { .. }
        ));
        assert!(matches!(
            judge(&sig("click", "e0", 0.9, 0.05), &els, None, false),
            Verdict::Uncertain { .. }
        ));
        assert!(matches!(
            judge(&sig("click", "none", 0.9, 0.9), &els, None, false),
            Verdict::Uncertain { .. }
        ));
        // "Delete" is on the list: blocked without the flag, needs 0.85 with it.
        assert!(matches!(
            judge(&sig("click", "e1", 0.95, 0.9), &els, None, false),
            Verdict::Blocked { .. }
        ));
        assert!(matches!(
            judge(&sig("click", "e1", 0.95, 0.9), &els, None, true),
            Verdict::Act { .. }
        ));
        assert!(matches!(
            judge(&sig("click", "e1", 0.7, 0.5), &els, None, true),
            Verdict::Uncertain { .. }
        ));
        // Jev's own destructive signal blocks even a harmless-looking name.
        let mut s = sig("click", "e0", 0.95, 0.9);
        s.is_destructive = 0.7;
        assert!(matches!(
            judge(&s, &els, None, false),
            Verdict::Blocked { .. }
        ));
    }

    #[test]
    fn close_call_between_two_candidates_asks_to_narrow() {
        let els = [
            el(0, "button", "Zamknij"),
            el(1, "menuitem", "Plik"),
            el(2, "button", "OK"),
        ];
        let mut s = sig("click", "e0", 0.47, 0.08);
        s.target_top = vec![
            ("e0".into(), 0.47),
            ("e1".into(), 0.39),
            ("e2".into(), 0.13),
        ];
        match judge(&s, &els, None, false) {
            Verdict::Uncertain { narrow, .. } => {
                assert_eq!(narrow, Some(vec!["e0".to_string(), "e1".to_string()]))
            }
            other => panic!("expected uncertain+narrow, got {other:?}"),
        }
        // `none` as runner-up is not a shortlist.
        s.target_top = vec![
            ("e0".into(), 0.47),
            ("none".into(), 0.39),
            ("e2".into(), 0.13),
        ];
        match judge(&s, &els, None, false) {
            Verdict::Uncertain { narrow, .. } => assert_eq!(narrow, None),
            other => panic!("expected uncertain without narrow, got {other:?}"),
        }
    }

    #[test]
    fn type_needs_dictated_text_and_focuses_the_target() {
        let els = [el(0, "edit", "Text editor")];
        assert_eq!(
            judge(&sig("type", "e0", 0.9, 0.9), &els, None, false),
            Verdict::NeedsText
        );
        match judge(&sig("type", "e0", 0.9, 0.9), &els, Some("hello"), false) {
            Verdict::Act {
                action: Action::Type { text, focus, .. },
            } => {
                assert_eq!(text, "hello");
                assert_eq!(focus, Some((40, 65)));
            }
            other => panic!("expected type, got {other:?}"),
        }
        match judge(&sig("type", "none", 0.9, 0.9), &els, Some("hello"), false) {
            Verdict::Act {
                action: Action::Type { focus, .. },
            } => assert_eq!(focus, None),
            other => panic!("expected type, got {other:?}"),
        }
    }

    #[test]
    fn right_click_and_destructive_keys() {
        let els = [el(0, "listitem", "Pulpit")];
        match judge(&sig("right_click", "e0", 0.9, 0.9), &els, None, false) {
            Verdict::Act {
                action: Action::Click { right, .. },
            } => assert!(right),
            other => panic!("expected right click, got {other:?}"),
        }
        let mut s = sig("key", "none", 0.9, 0.9);
        s.key = "delete".into();
        assert!(matches!(
            judge(&s, &els, None, false),
            Verdict::Blocked { .. }
        ));
        s.key_conf = 0.95;
        assert!(matches!(judge(&s, &els, None, true), Verdict::Act { .. }));
    }

    #[test]
    fn key_and_scroll() {
        let els = [el(0, "list", "Files")];
        let mut s = sig("key", "none", 0.9, 0.9);
        s.key = "ctrl+s".into();
        assert_eq!(
            judge(&s, &els, None, false),
            Verdict::Act {
                action: Action::Key {
                    key: "ctrl+s".into()
                }
            }
        );
        s.key = "none".into();
        assert!(matches!(
            judge(&s, &els, None, false),
            Verdict::Uncertain { .. }
        ));
        assert_eq!(
            judge(&sig("scroll_down", "e0", 0.9, 0.9), &els, None, false),
            Verdict::Act {
                action: Action::Scroll {
                    notches: consts::SCROLL_NOTCHES,
                    at: Some((40, 65))
                }
            }
        );
    }

    #[test]
    fn quoted_text_extraction() {
        assert_eq!(
            extract_quoted("wpisz „hello world” w edytorze"),
            Some("hello world".into())
        );
        assert_eq!(extract_quoted("type \"abc\" then save"), Some("abc".into()));
        assert_eq!(extract_quoted("type 'x y' now"), Some("x y".into()));
        assert_eq!(extract_quoted("no quotes here"), None);
        assert_eq!(extract_quoted("empty \"\" quotes"), None);
        assert_eq!(extract_quoted("Don't save and don't close"), None);
        assert_eq!(extract_quoted("it's 'ok' now"), Some("ok".into()));
    }

    #[test]
    fn bundle_has_seven_questions_and_compiles() {
        let els = [el(0, "button", "Save"), el(1, "edit", "Name")];
        let crit = questions::criteria(&els);
        assert_eq!(crit.len(), 3);
        assert_eq!(crit[0].0, "e0");
        assert_eq!(crit[2].0, "none");
        let b = questions::bundle(&crit);
        let mut names: Vec<&String> = b.keys().collect();
        names.sort();
        assert_eq!(
            names,
            [
                "goal_pending",
                "goal_reached",
                "is_destructive",
                "key",
                "needs_text",
                "op",
                "target"
            ]
        );
        let c = questions::compile(&els);
        assert_eq!(c.names.len(), 7);
    }

    #[test]
    fn slug_is_filename_safe() {
        assert_eq!(
            slug("Wpisz „hello” w Notatniku!"),
            "wpisz-hello-w-notatniku"
        );
    }
}
