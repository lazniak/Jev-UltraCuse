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
    /// Iterative widening on uncertainty (ADR-004), one rung per uncertain step:
    /// 0 = the window's interactive controls; `WIDEN_CONTEXT` = + labels/static text
    /// and `WIDEN_MAX_CANDIDATES`; `WIDEN_SURVEY` = a survey of every open window
    /// (switch, or show the desktop); `WIDEN_TWO` = System Two rescue (when enabled);
    /// beyond that the run gives up. `target = none` skips straight to the survey.
    pub const WIDEN_CONTEXT: u8 = 1;
    pub const WIDEN_SURVEY: u8 = 2;
    pub const WIDEN_TWO: u8 = 3;
    pub const WIDEN_MAX_CANDIDATES: usize = 120;
    /// Foreground changes the loop did not cause (the user keeps taking the mouse
    /// back) before the run stops.
    pub const DISPLACED_STRIKES: u32 = 3;
    /// `Switch` visits per window and run: a second visit allows a round trip (copy
    /// there, paste back); a third is a ping-pong the survey refuses.
    pub const SWITCH_MAX_VISITS: u32 = 2;
    /// Polls (of `SETTLE_CAP_MS`) tolerated with our own window in front before the
    /// run ends: long enough for the user to reach Stop after restoring the window.
    pub const OWN_WINDOW_STRIKES: u32 = 10;
    /// Windows minimized at most to reach the desktop.
    pub const SHOW_DESKTOP_MAX: usize = 8;
    /// Windows listed in a survey (front-most first).
    pub const SURVEY_MAX_WINDOWS: usize = 24;
    /// Consecutive actions with no visible change before the run stops.
    pub const STALL_STRIKES: u32 = 2;
    pub const MAX_STEPS_DEFAULT: usize = 12;
    /// Max candidates sent to Jev (measured stable to 60).
    pub const MAX_CANDIDATES: usize = 60;
    /// Pop-ups of the target process scanned in addition to the foreground window
    /// (a menu and its sub-menu, a drop-down, an owned dialog).
    pub const MAX_POPUPS: usize = 3;
    /// Name of the synthetic element that stands for an empty spot of the target
    /// window: the only way to say "right-click the background" (desktop → New…).
    pub const BACKGROUND_NAME: &str = "background (empty area)";
    /// Caption / tab strip height skipped when looking for an empty spot, px at
    /// 96 DPI (scaled by the window's DPI; the process is per-monitor aware).
    pub const BACKGROUND_TOP_SKIP: i32 = 48;
    /// Margin kept from the window edges (resize frame) and the halo around every
    /// element, px at 96 DPI.
    pub const BACKGROUND_MARGIN: i32 = 16;
    pub const BACKGROUND_HALO: i32 = 12;
    /// System Two (OpenRouter chat model) consultations per run: one plan + rescues.
    pub const TWO_MAX_CALLS: u32 = 3;
    /// How long the loop waits for a rescue when Jev is stuck (kill switch and stop
    /// are polled meanwhile). Only then: a flowing loop never waits for System Two.
    pub const TWO_WAIT_MS: u64 = 15_000;
    /// One consultation's HTTP timeout — longer than `TWO_WAIT_MS` on purpose: a plan
    /// is never waited for and is still useful when it lands two steps later, and a
    /// late rescue still contributes its plan/text (its `next` is discarded as stale).
    pub const TWO_TIMEOUT_MS: u64 = 25_000;
    pub const TWO_DEFAULT_MODEL: &str = "google/gemini-2.5-flash-lite";
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
        pub const PLACE: &str = "In which of `windows` should the work toward `goal` continue? `current` is the window in front now; `last` says what was just done.";
        pub const PLACE_DESKTOP: &str = "The desktop itself (its icons and background) — every open window is in the way and should be minimized.";
        pub const PLACE_DESKTOP_HERE: &str =
            "The desktop itself, which is in front now: stay here.";
        pub const PLACE_NONE: &str =
            "No open window fits `goal`; the application needed is not running.";
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
        /// Bring another open window to the front (a survey decision). No input is
        /// injected: `SetForegroundWindow`.
        Switch {
            hwnd: isize,
            title: String,
            exe: String,
        },
        /// Minimize whatever covers the desktop, then work there (a survey decision).
        ShowDesktop,
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
        // The synthetic background stands for a spot to (right-)click; typing or
        // scrolling "at" it would land on whatever is really there. (`key` and `wait`
        // do not use the target, so they pass.)
        if target_el.is_some_and(|e| e.name == BACKGROUND_NAME)
            && matches!(s.op.as_str(), "type" | "scroll_up" | "scroll_down")
        {
            return unsure(format!("op {} on the background", s.op));
        }
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

    /// A System Two proposal through the same gates as a Jev decision. There is no
    /// calibrated confidence behind it, so anything irreversible needs the explicit
    /// `--allow-irreversible`; unknown ops, keys and targets are refused.
    /// `is_destructive` is Jev's reading of the same state: control names reach the
    /// LLM verbatim, so a planted "click Discard" must still meet Jev's guard.
    pub fn from_advice(
        n: &uc_two::Next,
        els: &[Element],
        dictated: Option<&str>,
        allow_irreversible: bool,
        is_destructive: f64,
    ) -> Verdict {
        let target_el = n
            .target
            .as_deref()
            .and_then(parse_target)
            .and_then(|i| find(els, i));
        let key = n.key.as_deref().unwrap_or("none");
        let destructive = is_destructive >= DESTRUCTIVE_BLOCK
            || target_el.is_some_and(|e| is_irreversible_name(&e.name))
            || (n.op == "key" && DESTRUCTIVE_KEYS.contains(&key));
        let blocked = |what: &str| Verdict::Blocked {
            reason: format!(
                "System Two proposed {what}, which looks irreversible; pass --allow-irreversible"
            ),
        };
        match n.op.as_str() {
            "done" => Verdict::Done,
            "wait" => Verdict::Act {
                action: Action::Wait,
            },
            "scroll_down" | "scroll_up" => Verdict::Act {
                action: Action::Scroll {
                    notches: if n.op == "scroll_down" {
                        SCROLL_NOTCHES
                    } else {
                        -SCROLL_NOTCHES
                    },
                    at: target_el.map(Element::center),
                },
            },
            "key" => {
                if !q::KEYS.iter().any(|(k, _)| *k == key) || key == "none" {
                    return unsure(format!("System Two proposed unknown key {key}"));
                }
                if destructive && !allow_irreversible {
                    return blocked(&format!("key {key}"));
                }
                Verdict::Act {
                    action: Action::Key {
                        key: key.to_string(),
                    },
                }
            }
            "type" => {
                let Some(text) = n.text.as_deref().or(dictated) else {
                    return Verdict::NeedsText;
                };
                if destructive && !allow_irreversible {
                    return blocked("typing here");
                }
                let focus = target_el.filter(|e| e.enabled);
                Verdict::Act {
                    action: Action::Type {
                        text: text.to_string(),
                        focus: focus.map(Element::center),
                        target_name: focus.map(describe),
                    },
                }
            }
            "click" | "right_click" => {
                let Some(el) = target_el else {
                    return unsure("System Two proposed a click without a listed target".into());
                };
                if !el.enabled {
                    return unsure(format!("target {} is disabled", describe(el)));
                }
                if destructive && !allow_irreversible {
                    return blocked(&format!("click {}", describe(el)));
                }
                let (x, y) = el.center();
                Verdict::Act {
                    action: Action::Click {
                        target: el.i,
                        name: describe(el),
                        x,
                        y,
                        right: n.op == "right_click",
                    },
                }
            }
            other => unsure(format!("System Two proposed unknown op {other}")),
        }
    }
}

// ------------------------------------------------------------------ survey

/// The window survey (widening rung `WIDEN_SURVEY`, and every displacement): one cheap
/// Jev call over the titles of the open windows — no UIA — answering "where should the
/// work go on?". The desktop is its own option, because reaching it means minimizing
/// what covers it, not activating Explorer's hidden window.
pub mod survey {
    use super::consts::{q, CONF_FLOOR, SWITCH_MAX_VISITS, TOP2_GAP_MIN};
    use serde_json::{json, Value};
    use uc_win32::WindowInfo;

    #[derive(Clone, Debug, PartialEq)]
    pub enum Choice {
        /// The window in front is the right place.
        Stay,
        Switch(WindowInfo),
        Desktop,
        /// No open window fits.
        Nothing(String),
        /// Jev could not tell.
        Unsure(String),
    }

    /// The window in front when the survey is asked.
    #[derive(Clone, Copy, Debug)]
    pub struct Current<'a> {
        pub hwnd: isize,
        pub app: &'a str,
        pub title: &'a str,
        /// The user moved the foreground here; the loop did not.
        pub displaced: bool,
        /// `Progman`/`WorkerW` in front: "desktop" means stay.
        pub on_desktop: bool,
    }

    /// State + question over the switchable windows (the desktop excluded from the
    /// list, present as the `desktop` option). Returns the candidate list the ids refer to.
    pub fn build(
        goal: &str,
        windows: &[WindowInfo],
        current: &Current<'_>,
        last: Option<Value>,
    ) -> (Value, uc_jev::Compiled, Vec<WindowInfo>) {
        let Current {
            hwnd: current_hwnd,
            app: current_app,
            title: current_title,
            displaced,
            on_desktop,
        } = *current;
        let candidates: Vec<WindowInfo> = windows
            .iter()
            .filter(|w| !w.is_desktop())
            .cloned()
            .collect();
        let list: Vec<Value> = candidates
            .iter()
            .enumerate()
            .map(|(i, w)| {
                json!({
                    "id": format!("w{i}"),
                    "title": w.title,
                    "exe": w.exe,
                    "minimized": w.minimized,
                    "in_front": w.hwnd == current_hwnd,
                })
            })
            .collect();
        let mut state = json!({
            "goal": goal,
            "current": {
                "app": current_app,
                "title": current_title,
                "displaced_by_user": displaced,
                "is_desktop": on_desktop,
            },
            "windows": list,
        });
        if let Some(l) = last {
            state["last"] = l;
        }
        let mut criteria: Vec<(String, String)> = candidates
            .iter()
            .enumerate()
            .map(|(i, w)| {
                (
                    format!("w{i}"),
                    format!(
                        "„{}” ({}){}{}",
                        w.title,
                        w.exe,
                        if w.minimized { ", minimized" } else { "" },
                        if w.hwnd == current_hwnd {
                            ", in front now"
                        } else {
                            ""
                        }
                    ),
                )
            })
            .collect();
        criteria.push((
            "desktop".into(),
            if on_desktop {
                q::PLACE_DESKTOP_HERE.into()
            } else {
                q::PLACE_DESKTOP.into()
            },
        ));
        criteria.push(("none".into(), q::PLACE_NONE.into()));
        let pairs: Vec<(&str, String)> = criteria
            .iter()
            .map(|(k, v)| (k.as_str(), v.clone()))
            .collect();
        let mut qs = serde_json::Map::new();
        qs.insert("place".into(), uc_jev::choice(q::PLACE, &pairs));
        (state, uc_jev::Compiled::new(&qs), candidates)
    }

    /// Floors: the winner's probability ≥ `CONF_FLOOR` and top-2 gap ≥ `TOP2_GAP_MIN`.
    /// (The element gate uses the model's `confidence` ≈ top-2 margin against the same
    /// floor, which is stricter; with twenty windows a margin floor would starve the
    /// survey.) `exhausted`: windows switched to `SWITCH_MAX_VISITS` times already (0 =
    /// the desktop) — choosing one again is a ping-pong, reported as unsure so the
    /// ladder moves on.
    pub fn judge(
        d: &uc_jev::Decision,
        candidates: &[WindowInfo],
        current_hwnd: isize,
        on_desktop: bool,
        exhausted: &[isize],
    ) -> (Choice, Value) {
        let by_id = |id: &str| {
            id.strip_prefix('w')
                .and_then(|n| n.parse::<usize>().ok())
                .and_then(|i| candidates.get(i))
        };
        let top = d.top("place", 3);
        // The ids mean nothing after the step: keep the titles next to them.
        let labelled: Vec<Value> = top
            .iter()
            .map(|(id, p)| {
                let label = by_id(id)
                    .map(|w| format!("{} ({})", w.title, w.exe))
                    .unwrap_or_else(|| id.clone());
                json!([id, p, label])
            })
            .collect();
        let note = json!({"top": labelled});
        let Some((id, p)) = top.first().cloned() else {
            return (Choice::Unsure("survey: no answer".into()), note);
        };
        let id = id.as_str();
        let gap = d.top2_gap("place").unwrap_or(p);
        if p < CONF_FLOOR || gap < TOP2_GAP_MIN {
            return (
                Choice::Unsure(format!("survey: {id} at {p:.2}, gap {gap:.2}")),
                note,
            );
        }
        let choice = match id {
            "desktop" if on_desktop => Choice::Stay,
            "desktop" if exhausted.contains(&0) => Choice::Unsure(format!(
                "survey: the desktop was shown {SWITCH_MAX_VISITS}× already"
            )),
            "desktop" => Choice::Desktop,
            "none" => Choice::Nothing("survey: no open window fits the goal".into()),
            w => match by_id(w) {
                Some(win) if win.hwnd == current_hwnd => Choice::Stay,
                Some(win) if exhausted.contains(&win.hwnd) => Choice::Unsure(format!(
                    "survey: „{}” was switched to {SWITCH_MAX_VISITS}× already",
                    win.title
                )),
                Some(win) => Choice::Switch(win.clone()),
                None => Choice::Unsure(format!("survey: unknown window {w}")),
            },
        };
        (choice, note)
    }
}

// ------------------------------------------------------------------ exec

pub mod exec {
    use std::time::Duration;

    use uc_input::Button;

    use crate::consts::{FOCUS_SETTLE_MS, SHOW_DESKTOP_MAX, WAIT_MS};
    use crate::policy::Action;
    use crate::{hwnd_of, LoopError};

    /// Inject one action. Every branch is a single `SendInput` batch (plus a clipboard
    /// paste for long text); no allocation happens between the call and the syscall
    /// beyond what `uc-input` already does.
    pub fn perform(a: &Action) -> Result<Vec<String>, LoopError> {
        match a {
            Action::Switch { hwnd, title, .. } => {
                if !uc_win32::bring_to_front(hwnd_of(*hwnd)) {
                    return Err(LoopError::Window(format!(
                        "could not bring „{title}” to the front"
                    )));
                }
                std::thread::sleep(Duration::from_millis(FOCUS_SETTLE_MS));
            }
            Action::ShowDesktop => {
                let minimized = uc_win32::show_desktop(SHOW_DESKTOP_MAX);
                std::thread::sleep(Duration::from_millis(FOCUS_SETTLE_MS));
                // `Progman`/`WorkerW` only: the taskbar in front is not the desktop.
                let on_desktop = uc_win32::foreground_hwnd()
                    .is_some_and(|h| uc_win32::is_desktop_class(&uc_win32::window_class(h)));
                if !on_desktop {
                    return Err(LoopError::Window("could not reach the desktop".into()));
                }
                return Ok(minimized);
            }
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
        Ok(Vec::new())
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
    #[error("System Two: {0}")]
    Two(String),
    #[error("window: {0}")]
    Window(String),
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
    let mut reduced = uc_uia::reduce(
        &scan.elements,
        uc_uia::ReduceOpts {
            max_n,
            near: Some(scene.cursor),
            viewport: Some(viewport),
            ..Default::default()
        },
    );
    // The background is a target too (context menu of the desktop or of an empty
    // canvas); UIA has no element for it, so one is synthesised at a free spot of the
    // main window — after `reduce`, so the candidate cap never drops it, but checked
    // against *every* scanned element (the cap hides most of a busy window), inside
    // the monitor's work area (the desktop window runs under the taskbar).
    // No monitor info: trust the window rect; no overlap with the work area: the window
    // is off every monitor, so there is no background to click.
    let area = match uc_win32::work_area_of(scene.hwnd()) {
        Some(wa) => uc_win32::rect_intersect(scene.rect, wa),
        None => Some(scene.rect),
    };
    if let Some((x, y)) =
        area.and_then(|a| empty_spot(a, &scan.elements, uc_win32::dpi_of(scene.hwnd())))
    {
        reduced.push(uc_uia::Element {
            i: reduced.len(),
            role: "pane".into(),
            name: consts::BACKGROUND_NAME.into(),
            bbox: [x - 8, y - 8, 16, 16],
            enabled: true,
            val: None,
            focused: false,
            auto_id: None,
        });
    }
    Perception {
        scan,
        reduced,
        popups,
    }
}

/// A point inside `rect` (below the caption, inside the margins) that no element
/// covers — the centre first, then a coarse grid ordered by distance from the centre.
/// `None` when the window is fully covered (a maximised document, a list view).
/// `dpi` scales the 96-DPI constants (caption, margin, halo, grid step).
pub fn empty_spot(rect: uc_win32::Rect, els: &[uc_uia::Element], dpi: u32) -> Option<(i32, i32)> {
    let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
    let px = |v: i32| (v as f32 * scale).round() as i32;
    let [rx, ry, rw, rh] = rect;
    let m = px(consts::BACKGROUND_MARGIN);
    let (x0, y0) = (rx + m, ry + px(consts::BACKGROUND_TOP_SKIP));
    let (x1, y1) = (rx + rw - m, ry + rh - m);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let halo = px(consts::BACKGROUND_HALO);
    let covered = |x: i32, y: i32| {
        els.iter().any(|e| {
            let [ex, ey, ew, eh] = e.bbox;
            x >= ex - halo && x < ex + ew + halo && y >= ey - halo && y < ey + eh + halo
        })
    };
    let (cx, cy) = ((x0 + x1) / 2, (y0 + y1) / 2);
    if !covered(cx, cy) {
        return Some((cx, cy));
    }
    let step_x = ((x1 - x0) / 12).max(px(48));
    let step_y = ((y1 - y0) / 12).max(px(48));
    let mut grid: Vec<(i32, i32)> = Vec::new();
    let mut y = y0;
    while y < y1 {
        let mut x = x0;
        while x < x1 {
            grid.push((x, y));
            x += step_x;
        }
        y += step_y;
    }
    grid.sort_by_key(|(x, y)| (x - cx).abs() + (y - cy).abs());
    grid.into_iter().find(|&(x, y)| !covered(x, y))
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
    /// Where the work starts: this window is brought to the front and adopted as the
    /// place (`None` = the foreground window at step 1). From there the loop follows
    /// its own actions — dialogs, new windows, a survey's switch, the desktop.
    pub start_hwnd: Option<isize>,
    /// Cooperative stop from another thread (a Stop button); checked wherever the
    /// kill switch is.
    pub stop: Option<Arc<AtomicBool>>,
    /// System Two: an OpenRouter chat model consulted beside the loop (plan at the
    /// start, rescue when Jev is stuck, text on demand). `None` = Jev only.
    pub two: Option<uc_two::Config>,
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
            start_hwnd: None,
            stop: None,
            two: None,
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
    /// Widening rung this step was perceived at (`consts::WIDEN_*`; 0 = plain).
    pub widen: u8,
    /// A window survey ran this step (displacement or rung `WIDEN_SURVEY`): top-3.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub survey: Option<Value>,
    /// The sub-goal Jev was asked about (`k/n: text`) when a System Two plan is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subgoal: Option<String>,
    /// Titles a `ShowDesktop` step minimized, front first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub minimized: Vec<String>,
    /// The window manager refused the step's action; nothing changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
    /// System Two consultations applied or received during this step.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub two: Vec<uc_two::Note>,
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
    /// Someone else kept moving the foreground (`DISPLACED_STRIKES`); nothing was
    /// injected into those windows.
    FocusLost(String),
    /// The place's window vanished without an action of the loop (closed by someone
    /// else); reported neutrally, the caller decides.
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
    pub two_model: Option<String>,
    pub two_calls: u32,
    pub two_cost_usd: f64,
    /// The plan System Two produced, if any was adopted.
    pub plan: Option<Vec<String>>,
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
    two: Option<uc_two::Advisor>,
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
        let two = match opts.two.clone() {
            Some(cfg) => {
                Some(uc_two::Advisor::spawn(cfg).map_err(|e| LoopError::Two(e.to_string()))?)
            }
            None => None,
        };
        Ok(Self {
            opts,
            scanner,
            rt,
            client,
            on_step: None,
            two,
        })
    }

    pub fn provider(&self) -> uc_jev::Provider {
        self.client.provider()
    }

    /// System Two's model id, when enabled.
    pub fn two_model(&self) -> Option<&str> {
        self.two.as_ref().map(|t| t.model())
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
        let mut dictated = self.opts.dictated.clone();
        let stop_flag = self.opts.stop.clone();
        let allow_irreversible = self.opts.allow_irreversible;
        // System Two's plan: `goal` becomes the current sub-goal until the last is done.
        let mut plan: Option<Vec<String>> = None;
        let mut plan_i = 0usize;
        let mut last: Option<Value> = None;
        let mut last_hash: Option<u64> = None;
        let mut entropy_strikes = 0u32;
        let mut stall = 0u32;
        let mut waits = 0u32;
        let mut last_was_wait = false;
        let mut steps = 0usize;
        let mut jev_calls = 0u64;
        let mut cost = 0.0f64;
        // Where the work happens (ADR-004): the caller's `start_hwnd` or the foreground
        // at step 1, then wherever the loop's own actions lead (dialogs, windows it
        // opened, a survey's switch, the desktop). A change of process the loop did
        // not cause is a displacement: nothing is injected there; a survey decides.
        let mut place_hwnd = self.opts.start_hwnd;
        let mut place_pid: Option<u32> = place_hwnd
            .map(|h| uc_win32::window_pid(hwnd_of(h)))
            .filter(|p| *p != 0);
        let mut expect_change = false;
        let mut displaced = 0u32;
        // Widening rung for the coming step (0 = plain; `consts::WIDEN_*`).
        let mut widen: u8 = 0;
        let mut survey_only = false;
        // `Switch` targets and how often: a round trip is fine, a ping-pong is not.
        let mut visits: std::collections::HashMap<isize, u32> = Default::default();
        let own_pid = std::process::id();
        let mut own_strikes = 0u32;

        let outcome = loop {
            if steps >= self.opts.max_steps {
                break Outcome::Budget;
            }
            if uc_win32::kill_switch_pressed() {
                break Outcome::Killed;
            }
            if stop_set(&stop_flag) {
                break Outcome::Stopped;
            }
            let t0 = Instant::now();

            // 1. Perceive: coarse scene in µs, UIA tree in ms (the app's provider decides).
            let scene = uc_win32::scene().ok_or(LoopError::NoForeground)?;
            if scene.pid == own_pid {
                // Our own window in front (the GUI not yet minimized, or restored by the
                // user): never the place — AccessKit would offer our own Start/Stop to
                // Jev. Not a step: wait, and give up when it stays.
                own_strikes += 1;
                if own_strikes >= consts::OWN_WINDOW_STRIKES {
                    break Outcome::FocusLost("own window in front".into());
                }
                std::thread::sleep(Duration::from_millis(consts::SETTLE_CAP_MS));
                continue;
            }
            own_strikes = 0;
            steps += 1;
            let mut displaced_now = false;
            match (place_hwnd, place_pid) {
                (Some(h), Some(pid)) if scene.pid != pid => {
                    let alive = uc_win32::is_window(hwnd_of(h));
                    if expect_change {
                        // Our own action opened, closed or switched something: follow it.
                        if let Some(l) = last.as_mut() {
                            l["effect"] = json!(format!(
                                "focus moved to {} „{}”{}",
                                scene.app,
                                scene.title,
                                if alive {
                                    ""
                                } else {
                                    "; the previous window is gone"
                                }
                            ));
                        }
                        place_hwnd = Some(scene.hwnd);
                        place_pid = Some(scene.pid);
                        last_hash = None;
                    } else if !alive {
                        break Outcome::TargetGone;
                    } else {
                        displaced += 1;
                        if displaced >= consts::DISPLACED_STRIKES {
                            break Outcome::FocusLost(format!(
                                "foreground moved to {} (pid {}) {displaced}× without the loop's doing",
                                scene.app, scene.pid
                            ));
                        }
                        // Not our window: no element decision, no injection — only the
                        // survey may say "continue here", "go back" or "desktop".
                        displaced_now = true;
                        survey_only = true;
                    }
                }
                // A dialog or another window of the same app: the place moves with it.
                (Some(_), Some(_)) => {
                    place_hwnd = Some(scene.hwnd);
                    displaced = 0;
                }
                _ => {
                    place_hwnd = Some(scene.hwnd);
                    place_pid = Some(scene.pid);
                    displaced = 0;
                }
            }
            expect_change = false;
            let survey_step = survey_only;
            let include_context = widen >= consts::WIDEN_CONTEXT;
            // A survey step asks about windows, not elements: no UIA scan (titles and
            // exes are all that leaves the machine), and the window the user moved to
            // is not inspected.
            let Perception {
                scan,
                reduced,
                popups,
            } = if survey_step {
                Perception {
                    scan: uc_uia::Scan::default(),
                    reduced: Vec::new(),
                    popups: 0,
                }
            } else {
                perceive(
                    &self.scanner,
                    &scene,
                    include_context,
                    if include_context {
                        consts::WIDEN_MAX_CANDIDATES
                    } else {
                        consts::MAX_CANDIDATES
                    },
                )
            };
            let hash = uc_uia::tree_hash(&reduced);
            let scan_ms = ms(t0);

            // 2. Did the last action change anything? Code compares states, not Jev.
            let changed = if survey_step {
                None
            } else {
                last_hash.map(|h| h != hash)
            };
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

            // 3. Decide: one request, seven questions — or, on a survey step, one
            //    question over the open windows.
            let goal_now: String = plan
                .as_ref()
                .map(|p| p[plan_i].clone())
                .unwrap_or_else(|| goal.to_string());
            let subgoal_label = plan
                .as_ref()
                .map(|p| format!("{}/{}: {}", plan_i + 1, p.len(), p[plan_i]));
            let step_widen = widen;
            let mut two_notes: Vec<uc_two::Note> = Vec::new();
            let mut two_outcome: Option<Outcome> = None;
            let mut two_decided = false;
            let mut survey_note: Option<Value> = None;
            let mut adopt_here = false;
            let (
                signals,
                verdict,
                step_tokens,
                step_cost,
                jev_ms,
                narrowed,
                signals_first,
                hedged_winner,
                est_tokens,
            ) = if survey_only {
                survey_only = false;
                let t_jev = Instant::now();
                let mut windows = uc_win32::list_windows();
                windows.truncate(consts::SURVEY_MAX_WINDOWS);
                let on_desktop =
                    uc_win32::is_desktop_class(&uc_win32::window_class(hwnd_of(scene.hwnd)));
                let (state, compiled, candidates) = survey::build(
                    &goal_now,
                    &windows,
                    &survey::Current {
                        hwnd: scene.hwnd,
                        app: &scene.app,
                        title: &scene.title,
                        displaced: displaced_now,
                        on_desktop,
                    },
                    last.clone(),
                );
                let bytes = serde_json::to_vec(&state)?;
                let d = self.rt.block_on(self.client.decide(&bytes, &compiled))?;
                jev_calls += 1;
                cost += d.cost_usd;
                let (choice, note) = {
                    let exhausted: Vec<isize> = visits
                        .iter()
                        .filter(|(_, n)| **n >= consts::SWITCH_MAX_VISITS)
                        .map(|(h, _)| *h)
                        .collect();
                    survey::judge(&d, &candidates, scene.hwnd, on_desktop, &exhausted)
                };
                survey_note = Some(note);
                let verdict = match choice {
                    survey::Choice::Stay => {
                        adopt_here = displaced_now;
                        policy::Verdict::Uncertain {
                            reason: "survey: the window in front is the right place".into(),
                            narrow: None,
                        }
                    }
                    survey::Choice::Switch(w) => policy::Verdict::Act {
                        action: policy::Action::Switch {
                            hwnd: w.hwnd,
                            title: w.title,
                            exe: w.exe,
                        },
                    },
                    survey::Choice::Desktop => policy::Verdict::Act {
                        action: policy::Action::ShowDesktop,
                    },
                    survey::Choice::Nothing(r) | survey::Choice::Unsure(r) => {
                        policy::Verdict::Uncertain {
                            reason: r,
                            narrow: None,
                        }
                    }
                };
                (
                    policy::Signals {
                        op: "survey".into(),
                        target: "none".into(),
                        ..Default::default()
                    },
                    verdict,
                    d.usage.input_tokens,
                    d.cost_usd,
                    ms(t_jev),
                    false,
                    None,
                    d.timing.winner,
                    bytes.len() / 4,
                )
            } else {
                let state = uc_uia::GuiState {
                    goal: &goal_now,
                    scene: &scene,
                    elements: &reduced,
                    last: last.clone(),
                    dictated: dictated.as_deref(),
                    plan: plan
                        .as_ref()
                        .map(|p| json!({"overall": goal, "steps": p, "current": plan_i})),
                };
                let est_tokens = state.estimate_tokens();
                let state_bytes = serde_json::to_vec(&state)?;
                // System Two, between the lines: the plan request goes out with the first
                // state and is never waited for; the loop keeps deciding with Jev.
                if steps == 1 {
                    if let Some(two) = self.two.as_mut() {
                        two.ask(uc_two::Request {
                            kind: uc_two::Kind::Plan,
                            tag: steps as u64,
                            goal: goal.to_string(),
                            subgoal: None,
                            state: serde_json::from_slice(&state_bytes)?,
                            jev: None,
                            why: None,
                            need_text: dictated.is_none(),
                        });
                    }
                }
                let compiled = questions::compile(&reduced);
                let t_jev = Instant::now();
                let decision = self
                    .rt
                    .block_on(self.client.decide(&state_bytes, &compiled))?;
                let mut jev_ms = ms(t_jev);
                jev_calls += 1;
                cost += decision.cost_usd;

                // 4. Gate in code — and on a close call between listed candidates, ask
                //    once more over the top two only (shortlist), still inside this step.
                let mut step_tokens = decision.usage.input_tokens;
                let mut step_cost = decision.cost_usd;
                let mut signals = policy::Signals::from_decision(&decision, &reduced);
                let mut verdict =
                    policy::judge(&signals, &reduced, dictated.as_deref(), allow_irreversible);
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
                    scored
                        .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
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
                    verdict =
                        policy::judge(&signals, &reduced, dictated.as_deref(), allow_irreversible);
                    narrowed = true;
                }

                // 4b. System Two, between the lines: fold in whatever arrived meanwhile
                //     (a plan, text — a late rescue's `next` is stale and never
                //     executed). A rescue is the last widening rung: when Jev is still
                //     stuck after context and the survey — or needs text nobody
                //     dictated — ask and wait, since the step would otherwise end empty.
                if let Some(two) = self.two.as_mut() {
                    while let Some(reply) = two.try_recv() {
                        two_notes.push(apply_reply(
                            &reply,
                            goal,
                            &mut plan,
                            &mut plan_i,
                            &mut dictated,
                            false,
                        ));
                    }
                    let needs_text = matches!(verdict, policy::Verdict::NeedsText);
                    let stuck = (matches!(verdict, policy::Verdict::Uncertain { .. })
                        && widen >= consts::WIDEN_TWO)
                        || (needs_text && dictated.is_none());
                    if stuck && two.remaining() > 0 {
                        let why = match &verdict {
                            policy::Verdict::Uncertain { reason, .. } => reason.clone(),
                            _ => "the goal needs text and none was dictated".to_string(),
                        };
                        let tag = steps as u64;
                        let asked = two.ask(uc_two::Request {
                            kind: uc_two::Kind::Rescue,
                            tag,
                            goal: goal.to_string(),
                            subgoal: plan.as_ref().map(|p| p[plan_i].clone()),
                            state: serde_json::from_slice(&state_bytes)?,
                            jev: Some(json!({
                                "target_top": signals.target_top,
                                "op": signals.op,
                                "op_conf": signals.op_conf,
                                "goal_reached": signals.goal_reached,
                                "needs_text": signals.needs_text,
                            })),
                            why: Some(why),
                            need_text: needs_text || signals.needs_text >= 0.5,
                        });
                        if asked {
                            let t_wait = Instant::now();
                            let deadline = t_wait + Duration::from_millis(consts::TWO_WAIT_MS);
                            let mut reply = None;
                            while Instant::now() < deadline {
                                if uc_win32::kill_switch_pressed() {
                                    two_outcome = Some(Outcome::Killed);
                                    break;
                                }
                                if stop_set(&stop_flag) {
                                    two_outcome = Some(Outcome::Stopped);
                                    break;
                                }
                                match two.wait(Duration::from_millis(100)) {
                                    uc_two::Wait::Reply(r)
                                        if r.kind == uc_two::Kind::Rescue && r.tag == tag =>
                                    {
                                        reply = Some(r);
                                        break;
                                    }
                                    // The worker is FIFO: a plan still in flight answers
                                    // first. Take it and keep waiting for ours.
                                    uc_two::Wait::Reply(r) => two_notes.push(apply_reply(
                                        &r,
                                        goal,
                                        &mut plan,
                                        &mut plan_i,
                                        &mut dictated,
                                        false,
                                    )),
                                    uc_two::Wait::Timeout => {}
                                    uc_two::Wait::Gone => break,
                                }
                            }
                            match reply {
                                Some(r) => {
                                    let mut note = apply_reply(
                                        &r,
                                        goal,
                                        &mut plan,
                                        &mut plan_i,
                                        &mut dictated,
                                        true,
                                    );
                                    let next_step =
                                        r.advice.as_ref().ok().and_then(|a| a.next.clone());
                                    if let Some(n) = next_step {
                                        let v = policy::from_advice(
                                            &n,
                                            &reduced,
                                            dictated.as_deref(),
                                            allow_irreversible,
                                            signals.is_destructive,
                                        );
                                        match &v {
                                            policy::Verdict::Act { .. } | policy::Verdict::Done => {
                                                note.applied = true;
                                                two_decided = true;
                                                verdict = v;
                                            }
                                            policy::Verdict::Blocked { reason }
                                            | policy::Verdict::Uncertain { reason, .. } => {
                                                note.note = format!("{} — {reason}", note.note);
                                            }
                                            policy::Verdict::NeedsText => {}
                                        }
                                    } else if needs_text && dictated.is_some() {
                                        // Text arrived: the original decision can go ahead.
                                        verdict = policy::judge(
                                            &signals,
                                            &reduced,
                                            dictated.as_deref(),
                                            allow_irreversible,
                                        );
                                        note.applied =
                                            matches!(verdict, policy::Verdict::Act { .. });
                                        two_decided = note.applied;
                                    }
                                    two_notes.push(note);
                                }
                                None if two_outcome.is_none() => two_notes.push(uc_two::Note {
                                    kind: uc_two::Kind::Rescue,
                                    model: two.model().to_string(),
                                    ms: ms(t_wait),
                                    cost_usd: 0.0,
                                    note: "no reply within the wait budget".into(),
                                    applied: false,
                                }),
                                None => {}
                            }
                        }
                    }
                }
                (
                    signals,
                    verdict,
                    step_tokens,
                    step_cost,
                    jev_ms,
                    narrowed,
                    signals_first,
                    decision.timing.winner,
                    est_tokens,
                )
            };

            let mut rec = StepRecord {
                step: steps,
                ts_unix: ts_unix(),
                app: scene.app.clone(),
                title: scene.title.clone(),
                raw_elements: scan.raw_count,
                popups,
                widen: step_widen,
                survey: survey_note,
                subgoal: subgoal_label.clone(),
                minimized: Vec::new(),
                refused: None,
                two: two_notes,
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
                hedged_winner,
                narrowed,
                signals_first,
                signals,
                verdict: verdict.clone(),
                executed: false,
            };

            // 5. Act (or not).
            let mut next: Option<Outcome> = two_outcome;
            match &verdict {
                _ if next.is_some() => {}
                policy::Verdict::Done => match &plan {
                    Some(p) if plan_i + 1 < p.len() => {
                        // Sub-goal done: on to the next one, counters fresh.
                        last = Some(json!({
                            "op": "subgoal_done",
                            "subgoal": p[plan_i],
                            "effect": "moving on to the next sub-goal",
                        }));
                        plan_i += 1;
                        last_hash = None;
                        last_was_wait = false;
                        entropy_strikes = 0;
                        stall = 0;
                        waits = 0;
                        widen = 0;
                    }
                    _ => next = Some(Outcome::Done),
                },
                policy::Verdict::NeedsText => next = Some(Outcome::NeedsText),
                policy::Verdict::Blocked { reason } => {
                    next = Some(Outcome::Blocked(reason.clone()))
                }
                policy::Verdict::Uncertain { reason, .. } => {
                    if adopt_here {
                        // The displacement survey says the window in front is the right
                        // place after all: continue there.
                        place_hwnd = Some(scene.hwnd);
                        place_pid = Some(scene.pid);
                        displaced = 0;
                        last = Some(json!({
                            "op": "look",
                            "effect": format!("continuing in {} „{}”", scene.app, scene.title),
                        }));
                        last_hash = None;
                    } else {
                        // Widen, one rung per uncertain step: context → survey →
                        // System Two → give up. "No listed element helps" skips to the
                        // survey: the right place is probably another window.
                        let two_left = self.two.as_ref().is_some_and(|t| t.remaining() > 0);
                        let mut rung =
                            if rec.signals.target == "none" && widen < consts::WIDEN_SURVEY {
                                consts::WIDEN_SURVEY
                            } else {
                                widen + 1
                            };
                        if rung == consts::WIDEN_TWO && !two_left {
                            rung += 1;
                        }
                        if rung > consts::WIDEN_TWO {
                            next = Some(Outcome::Uncertain(reason.clone()));
                        } else {
                            widen = rung;
                            survey_only = rung == consts::WIDEN_SURVEY;
                            last = Some(json!({
                                "op": "look",
                                "effect": format!(
                                    "uncertain: {reason}; widening to {}",
                                    widen_name(rung)
                                ),
                            }));
                            last_hash = None;
                        }
                    }
                }
                policy::Verdict::Act { action } => {
                    // The narrowed round reports the gates' entropy (0); the guard must
                    // look at the first, full distribution.
                    let entropy = rec
                        .signals_first
                        .as_ref()
                        .unwrap_or(&rec.signals)
                        .target_entropy;
                    // Jev's entropy says nothing about a step System Two chose.
                    if matches!(action, policy::Action::Click { .. }) && !two_decided {
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
                    } else if stop_set(&stop_flag) {
                        next = Some(Outcome::Stopped);
                    } else if !focus_still_ours(scene.hwnd) {
                        // Deciding took 0.3–2 s; a toast, a UAC prompt or an Alt-Tab may
                        // have moved the foreground meanwhile. Never inject blind: skip
                        // this step, the next one sees where the focus went.
                        last = Some(json!({
                            "op": "look",
                            "effect": "foreground changed while deciding; nothing injected",
                        }));
                        last_hash = None;
                    } else {
                        let t_act = Instant::now();
                        match exec::perform(action) {
                            Ok(minimized) => {
                                rec.act_ms = ms(t_act);
                                rec.executed = true;
                                let mut summary = action_summary(action);
                                if !minimized.is_empty() {
                                    rec.minimized = minimized.clone();
                                    summary["minimized"] = json!(minimized);
                                }
                                last = Some(summary);
                                let window_op = matches!(
                                    action,
                                    policy::Action::Switch { .. } | policy::Action::ShowDesktop
                                );
                                // Visit counts; the desktop is key 0 (never a window handle).
                                match action {
                                    policy::Action::Switch { hwnd, .. } => {
                                        *visits.entry(*hwnd).or_default() += 1;
                                    }
                                    policy::Action::ShowDesktop => {
                                        *visits.entry(0).or_default() += 1;
                                    }
                                    _ => {}
                                }
                                // A new window is a new tree: no stall comparison across it.
                                last_hash = if window_op { None } else { Some(hash) };
                                // Only input and window ops can move the foreground; a
                                // wait or a scroll followed by a new window in front is
                                // the user's doing, and must read as a displacement.
                                expect_change = !matches!(
                                    action,
                                    policy::Action::Wait | policy::Action::Scroll { .. }
                                );
                                widen = 0;
                                displaced = 0;
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
                            Err(LoopError::Window(msg)) => {
                                // The window manager refused (foreground lock, an elevated
                                // or a closed window): a step without effect, not the end
                                // of the run. The rung stays, so the ladder moves on.
                                rec.act_ms = ms(t_act);
                                rec.refused = Some(msg.clone());
                                last = Some(json!({
                                    "op": "look",
                                    "effect": format!("{msg}; nothing changed"),
                                }));
                                last_hash = None;
                            }
                            Err(e) => return Err(e),
                        }
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
            two_model: self.two.as_ref().map(|t| t.model().to_string()),
            two_calls: self.two.as_ref().map_or(0, |t| t.calls()),
            two_cost_usd: self.two.as_ref().map_or(0.0, |t| t.cost_usd()),
            plan,
        };
        if let Some((f, _)) = ledger.as_mut() {
            writeln!(f, "{}", serde_json::to_string(&summary)?)?;
        }
        Ok(summary)
    }
}

/// Is the window the step scanned still the one in front (and alive)? Checked right
/// before injecting: the decision took 0.3–2 s and the coordinates belong to that tree.
fn focus_still_ours(scanned_hwnd: isize) -> bool {
    let h = hwnd_of(scanned_hwnd);
    uc_win32::is_window(h) && uc_win32::foreground_hwnd().is_some_and(|fg| fg == h)
}

fn stop_set(flag: &Option<Arc<AtomicBool>>) -> bool {
    flag.as_ref().is_some_and(|f| f.load(Ordering::Relaxed))
}

/// Fold one System Two reply into the run: a plan is adopted once (never replaced),
/// a rephrased sub-goal overrides the current one (only when `allow_subgoal`: a reply
/// answering an older step must not rewrite the current one), text fills an empty
/// dictation. A proposed step (`next`) is left to the caller, which has the elements
/// to vet it. Without a plan, a sub-goal becomes step one *before* the overall goal,
/// so finishing it can never end the run as "goal reached".
fn apply_reply(
    r: &uc_two::Reply,
    goal: &str,
    plan: &mut Option<Vec<String>>,
    plan_i: &mut usize,
    dictated: &mut Option<String>,
    allow_subgoal: bool,
) -> uc_two::Note {
    let mut note = uc_two::Note {
        kind: r.kind,
        model: r.model.clone(),
        ms: r.ms,
        cost_usd: r.cost_usd,
        note: String::new(),
        applied: false,
    };
    match &r.advice {
        Err(e) => note.note = format!("error: {e}"),
        Ok(a) => {
            note.note = a.note.clone().unwrap_or_default();
            if let (Some(steps), None) = (&a.plan, &*plan) {
                *plan = Some(steps.clone());
                *plan_i = 0;
                note.applied = true;
                note.note = format!(
                    "plan ({}): {} | {}",
                    steps.len(),
                    steps.join(" → "),
                    note.note
                );
            }
            if let (Some(sg), true) = (&a.subgoal, allow_subgoal) {
                match plan.as_mut() {
                    Some(p) => p[*plan_i] = sg.clone(),
                    None => *plan = Some(vec![sg.clone(), goal.to_string()]),
                }
                note.applied = true;
            }
            if let (Some(t), None) = (&a.text, &*dictated) {
                *dictated = Some(t.clone());
                note.applied = true;
            }
        }
    }
    note
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
        policy::Action::Switch { title, exe, .. } => {
            json!({"op": "switch", "window": title, "exe": exe})
        }
        policy::Action::ShowDesktop => json!({"op": "show_desktop"}),
    }
}

fn hwnd_of(h: isize) -> uc_win32::HWND {
    uc_win32::HWND(h as *mut core::ffi::c_void)
}

/// Human name of a widening rung, for `last` and the log.
fn widen_name(rung: u8) -> &'static str {
    match rung {
        r if r == consts::WIDEN_CONTEXT => "context (labels, more candidates)",
        r if r == consts::WIDEN_SURVEY => "window survey",
        r if r == consts::WIDEN_TWO => "System Two",
        _ => "none",
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
    fn advice_goes_through_the_gates() {
        use uc_two::Next;
        let els = [el(0, "button", "Save"), el(1, "button", "Delete")];
        let next = |op: &str, target: Option<&str>| Next {
            op: op.into(),
            target: target.map(str::to_string),
            key: None,
            text: None,
        };
        let judge =
            |n: &Next, allow: bool, destr: f64| policy::from_advice(n, &els, None, allow, destr);
        assert!(matches!(
            judge(&next("click", Some("e0")), false, 0.0),
            Verdict::Act {
                action: Action::Click { target: 0, .. }
            }
        ));
        // Jev's own destructive reading gates the LLM's click on a harmless-looking name
        assert!(matches!(
            judge(&next("click", Some("e0")), false, 0.9),
            Verdict::Blocked { .. }
        ));
        assert!(matches!(
            judge(&next("click", Some("e1")), false, 0.0),
            Verdict::Blocked { .. }
        ));
        assert!(matches!(
            judge(&next("click", Some("e1")), true, 0.0),
            Verdict::Act { .. }
        ));
        assert!(matches!(
            judge(&next("click", Some("e7")), false, 0.0),
            Verdict::Uncertain { .. }
        ));
        assert!(matches!(
            judge(&next("type", None), false, 0.0),
            Verdict::NeedsText
        ));
        let mut typed = next("type", Some("e0"));
        typed.text = Some("hi".into());
        assert!(matches!(
            judge(&typed, false, 0.0),
            Verdict::Act {
                action: Action::Type { .. }
            }
        ));
        let mut del = next("key", None);
        del.key = Some("delete".into());
        assert!(matches!(judge(&del, false, 0.0), Verdict::Blocked { .. }));
        let mut odd = next("key", None);
        odd.key = Some("ctrl+alt+del".into());
        assert!(matches!(judge(&odd, true, 0.0), Verdict::Uncertain { .. }));
        assert!(matches!(
            judge(&next("done", None), false, 0.0),
            Verdict::Done
        ));
    }

    #[test]
    fn replies_fold_into_plan_text_and_subgoal() {
        let reply = |advice: uc_two::Advice| uc_two::Reply {
            kind: uc_two::Kind::Plan,
            tag: 1,
            advice: Ok(advice),
            model: "m".into(),
            ms: 1.0,
            cost_usd: 0.0,
            prompt_tokens: 0,
            completion_tokens: 0,
        };
        let mut plan = None;
        let mut i = 0usize;
        let mut dictated = None;
        let n = apply_reply(
            &reply(uc_two::Advice {
                plan: Some(vec!["a".into(), "b".into()]),
                text: Some("t".into()),
                ..Default::default()
            }),
            "overall",
            &mut plan,
            &mut i,
            &mut dictated,
            true,
        );
        assert!(n.applied);
        assert_eq!(
            plan.as_deref(),
            Some(&["a".to_string(), "b".to_string()][..])
        );
        assert_eq!(dictated.as_deref(), Some("t"));
        // a second plan never replaces the first; a sub-goal rewrites the current one
        i = 1;
        let n = apply_reply(
            &reply(uc_two::Advice {
                plan: Some(vec!["x".into()]),
                subgoal: Some("b2".into()),
                text: Some("ignored".into()),
                ..Default::default()
            }),
            "overall",
            &mut plan,
            &mut i,
            &mut dictated,
            true,
        );
        assert!(n.applied);
        assert_eq!(plan.as_ref().unwrap()[1], "b2");
        assert_eq!(dictated.as_deref(), Some("t"));
        // a late reply may not rewrite the current sub-goal
        let n = apply_reply(
            &reply(uc_two::Advice {
                subgoal: Some("late".into()),
                ..Default::default()
            }),
            "overall",
            &mut plan,
            &mut i,
            &mut dictated,
            false,
        );
        assert!(!n.applied);
        assert_eq!(plan.as_ref().unwrap()[1], "b2");
        let n = apply_reply(
            &uc_two::Reply {
                advice: Err("boom".into()),
                ..reply(uc_two::Advice::default())
            },
            "overall",
            &mut plan,
            &mut i,
            &mut dictated,
            true,
        );
        assert!(!n.applied);
        assert!(n.note.starts_with("error"));
        // without a plan, a sub-goal is step one and the overall goal stays last
        let mut plan2 = None;
        let mut i2 = 0usize;
        let mut d2 = None;
        apply_reply(
            &reply(uc_two::Advice {
                subgoal: Some("open the menu".into()),
                ..Default::default()
            }),
            "overall",
            &mut plan2,
            &mut i2,
            &mut d2,
            true,
        );
        assert_eq!(
            plan2.as_deref(),
            Some(&["open the menu".to_string(), "overall".to_string()][..])
        );
    }

    #[test]
    fn empty_spot_prefers_centre_then_grid_then_gives_up() {
        let rect = [0, 0, 1000, 800];
        // Margin 16 / caption 48 at 96 DPI: the free area is 16..984 × 48..784.
        assert_eq!(empty_spot(rect, &[], 96), Some((500, 416)));
        // Doubled at 192 DPI: 32..968 × 96..768.
        assert_eq!(empty_spot(rect, &[], 192), Some((500, 432)));
        let mut mid = el(0, "button", "Mid");
        mid.bbox = [400, 350, 200, 150];
        let (x, y) = empty_spot(rect, &[mid], 96).expect("a free grid point");
        assert!(
            (16..984).contains(&x) && (48..784).contains(&y),
            "inside the area"
        );
        assert!(
            !(388..612).contains(&x) || !(338..512).contains(&y),
            "outside Mid and its halo: ({x}, {y})"
        );
        let mut all = el(1, "document", "Doc");
        all.bbox = [-20, -20, 1040, 840];
        assert_eq!(empty_spot(rect, &[all], 96), None);
        assert_eq!(empty_spot([0, 0, 20, 20], &[], 96), None);
    }

    #[test]
    fn survey_builds_ids_and_judges_thresholds() {
        use uc_win32::WindowInfo;
        let win = |hwnd: isize, title: &str, exe: &str| WindowInfo {
            hwnd,
            title: title.into(),
            exe: exe.into(),
            pid: 1,
            minimized: false,
        };
        let windows = vec![
            win(10, "Program Manager", "explorer.exe"),
            win(11, "Notatnik", "notepad.exe"),
            win(12, "Poczta", "olk.exe"),
        ];
        let (state, _compiled, cands) = survey::build(
            "x",
            &windows,
            &survey::Current {
                hwnd: 12,
                app: "olk",
                title: "Poczta",
                displaced: true,
                on_desktop: false,
            },
            None,
        );
        assert_eq!(cands.len(), 2, "the desktop is an option, not a window");
        assert_eq!(state["windows"][1]["in_front"], true);
        assert_eq!(state["current"]["displaced_by_user"], true);
        let dec = |probs: &[(&str, f64)]| {
            let mut m = std::collections::HashMap::new();
            for (k, v) in probs {
                m.insert(k.to_string(), *v);
            }
            uc_jev::Decision::from_probs("place", m)
        };
        let judge = |probs: &[(&str, f64)], on_desktop: bool, visited: &[isize]| {
            survey::judge(&dec(probs), &cands, 12, on_desktop, visited)
        };
        let (choice, note) = judge(&[("w0", 0.9), ("desktop", 0.1)], false, &[]);
        assert!(matches!(choice, survey::Choice::Switch(w) if w.hwnd == 11));
        assert_eq!(note["top"][0][2], "Notatnik (notepad.exe)", "titles kept");
        assert_eq!(
            judge(&[("w1", 0.8), ("w0", 0.2)], false, &[]).0,
            survey::Choice::Stay
        );
        assert_eq!(
            judge(&[("desktop", 0.7), ("w0", 0.3)], false, &[]).0,
            survey::Choice::Desktop
        );
        assert_eq!(
            judge(&[("desktop", 0.7), ("w0", 0.3)], true, &[]).0,
            survey::Choice::Stay,
            "already on the desktop: no ShowDesktop no-op"
        );
        assert!(matches!(
            judge(&[("w0", 0.9), ("desktop", 0.1)], false, &[11]).0,
            survey::Choice::Unsure(_)
        ));
        assert!(matches!(
            judge(&[("w0", 0.5), ("w1", 0.5)], false, &[]).0,
            survey::Choice::Unsure(_)
        ));
        assert!(matches!(
            judge(&[("none", 0.9), ("w0", 0.1)], false, &[]).0,
            survey::Choice::Nothing(_)
        ));
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
