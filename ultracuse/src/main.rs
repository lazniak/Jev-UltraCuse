//! `ultracuse` CLI — the MVP loop (`run`), diagnostics (`doctor`, `probe`) and the
//! measurements that justify the design (R1 UIA scan, R2 Jev round-trip). Voice lands
//! after ADR-002 (TASKS 2.x).

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;

mod ui;

#[derive(Parser)]
#[command(
    name = "ultracuse",
    version,
    about = "Jev-UltraCuse — Computer Use decided by Jev, driven by voice"
)]
struct Cli {
    /// No subcommand = open the window.
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check DPI, UIA, PowerShell session and the Jev key/endpoint (one tiny decision).
    Doctor,
    /// Scan the foreground window (switch to it within `--delay` seconds) and print the GuiState.
    Probe {
        #[arg(long, default_value_t = 3)]
        delay: u64,
        #[arg(long)]
        json: bool,
        /// Also scan static text / custom controls (more context, bigger state).
        #[arg(long)]
        context: bool,
        #[arg(long, default_value_t = 60)]
        max: usize,
        /// Scan this window handle instead of the foreground one (e.g. from `(Get-Process notepad).MainWindowHandle`).
        #[arg(long)]
        hwnd: Option<isize>,
    },
    /// R1: time the UIA scan of the foreground window N times.
    BenchUia {
        #[arg(long, default_value_t = 3)]
        delay: u64,
        #[arg(long, default_value_t = 20)]
        runs: usize,
        #[arg(long)]
        context: bool,
        #[arg(long)]
        hwnd: Option<isize>,
    },
    /// R2: Jev round-trip from Rust with a synthetic UI state of N elements.
    BenchJev {
        #[arg(long, default_value_t = 10)]
        runs: usize,
        #[arg(long, default_value = "12,30,60")]
        sizes: String,
        /// Hedge after this many ms (0 = off).
        #[arg(long, default_value_t = 0)]
        hedge_ms: u64,
        /// typesafe | openrouter (default: first key found, vendor first)
        #[arg(long)]
        provider: Option<String>,
    },
    /// Run one PowerShell command in a warm session and print timing.
    Ps {
        script: String,
        #[arg(long)]
        allow_destructive: bool,
    },
    /// Open the window (the default when no subcommand is given).
    Ui,
    /// MVP loop: perceive the foreground window, ask Jev, act (with --act), repeat.
    Run(RunArgs),
    /// Inject a click at screen coordinates (requires --act).
    Click {
        x: i32,
        y: i32,
        #[arg(long)]
        act: bool,
    },
}

fn main() -> Result<()> {
    uc_win32::ensure_dpi_aware();
    let cli = Cli::parse();
    match cli.cmd {
        None | Some(Cmd::Ui) => ui::run_ui(),
        Some(cmd) => cli_cmd(cmd),
    }
}

fn cli_cmd(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Ui => unreachable!("handled in main"),
        Cmd::Doctor => doctor(),
        Cmd::Probe {
            delay,
            json,
            context,
            max,
            hwnd,
        } => probe(delay, json, context, max, hwnd),
        Cmd::BenchUia {
            delay,
            runs,
            context,
            hwnd,
        } => bench_uia(delay, runs, context, hwnd),
        Cmd::BenchJev {
            runs,
            sizes,
            hedge_ms,
            provider,
        } => bench_jev(runs, &sizes, hedge_ms, provider.as_deref()),
        Cmd::Ps {
            script,
            allow_destructive,
        } => ps(&script, allow_destructive),
        Cmd::Run(a) => run(a),
        Cmd::Click { x, y, act } => {
            if !act {
                println!("would click ({x}, {y}) — pass --act to inject");
                return Ok(());
            }
            uc_input::click(x, y, uc_input::Button::Left, 1)?;
            println!("clicked ({x}, {y})");
            Ok(())
        }
    }
}

fn countdown(secs: u64, what: &str) {
    if secs == 0 {
        return;
    }
    eprintln!("{what} in {secs} s — bring the target window to the front…");
    std::thread::sleep(Duration::from_secs(secs));
}

fn doctor() -> Result<()> {
    println!("dpi_aware      : per-monitor v2 requested");
    let vs = uc_win32::virtual_screen();
    println!(
        "virtual_screen : x={} y={} w={} h={}",
        vs[0], vs[1], vs[2], vs[3]
    );
    let t = Instant::now();
    let scanner = uc_uia::UiaScanner::new().context("UIA init")?;
    println!(
        "uia_init       : ok ({:.1} ms)",
        t.elapsed().as_secs_f64() * 1000.0
    );
    let scan = scanner.scan_foreground(false);
    println!(
        "uia_scan(fg)   : {} raw / {} kept, find {:.1} ms, read {:.1} ms{}",
        scan.raw_count,
        scan.elements.len(),
        scan.find_ms,
        scan.read_ms,
        scan.error
            .as_deref()
            .map(|e| format!(" ERROR {e}"))
            .unwrap_or_default()
    );
    let t = Instant::now();
    match uc_shell::PowerShell::start() {
        Ok(mut ps) => {
            let start_ms = t.elapsed().as_secs_f64() * 1000.0;
            let out = ps.run(
                "$PSVersionTable.PSVersion.ToString()",
                Duration::from_secs(10),
                false,
            )?;
            println!(
                "pwsh           : {} (start {:.0} ms, first command {:.1} ms incl. JIT)",
                out.text,
                start_ms,
                out.elapsed.as_secs_f64() * 1000.0
            );
            let mut warm = Vec::new();
            for _ in 0..3 {
                let out = ps.run("Get-Date -Format o", Duration::from_secs(10), false)?;
                warm.push(format!("{:.1}", out.elapsed.as_secs_f64() * 1000.0));
            }
            let out = ps.run("Get-ChildItem $env:USERPROFILE | Measure-Object | Select-Object -ExpandProperty Count", Duration::from_secs(10), false)?;
            println!(
                "pwsh warm cmds : Get-Date ×3 = {} ms; Get-ChildItem ~/ = {:.1} ms ({} entries)",
                warm.join(" / "),
                out.elapsed.as_secs_f64() * 1000.0,
                out.text.trim()
            );
        }
        Err(e) => println!("pwsh           : ERROR {e}"),
    }
    match uc_jev::discover() {
        None => println!("jev_key        : NOT FOUND (JEV_API_KEY / TYPESAFE_API_KEY / OPENROUTER_API_KEY / JEVUSE_API_KEY)"),
        Some((p, key, name)) => {
            println!("jev_key        : {name} ({} chars) → provider {p:?} {}", key.len(), p.url());
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                let cfg = uc_jev::Config::for_provider(p)?;
                let client = uc_jev::Client::new(cfg)?;
                let cold = client.warm().await?;
                let warm = client.warm().await?;
                println!("jev_rtt        : cold {cold:.0} ms, warm {warm:.0} ms (one noul, ~320 tokens each)");
                Ok::<(), anyhow::Error>(())
            })?;
        }
    }
    Ok(())
}

/// Scene of the foreground window, or of an explicit handle (for unattended benchmarks).
fn target_scene(hwnd: Option<isize>) -> Result<uc_win32::Scene> {
    match hwnd {
        None => uc_win32::scene().context("no foreground window"),
        Some(h) => {
            let hw = windows_hwnd(h);
            let pid = uc_win32::window_pid(hw);
            let exe = uc_win32::process_exe(pid).unwrap_or_default();
            Ok(uc_win32::Scene {
                hwnd: h,
                app: exe.trim_end_matches(".exe").to_string(),
                title: uc_win32::window_title(hw),
                class: uc_win32::window_class(hw),
                exe,
                pid,
                rect: uc_win32::window_rect(hw).unwrap_or([0, 0, 0, 0]),
                cursor: uc_win32::cursor_pos(),
                fg: false,
            })
        }
    }
}

fn windows_hwnd(h: isize) -> windows_hwnd_type::HWND {
    windows_hwnd_type::HWND(h as *mut core::ffi::c_void)
}

mod windows_hwnd_type {
    pub use uc_win32::HWND;
}

fn probe(delay: u64, as_json: bool, context: bool, max: usize, hwnd: Option<isize>) -> Result<()> {
    if hwnd.is_none() {
        countdown(delay, "Scanning");
    }
    let scanner = uc_uia::UiaScanner::new().context("UIA init")?;
    let scene = target_scene(hwnd)?;
    let scan = scanner.scan_hwnd(scene.hwnd(), context);
    let reduced = uc_uia::reduce(
        &scan.elements,
        uc_uia::ReduceOpts {
            max_n: max,
            near: Some(scene.cursor),
            viewport: Some(scene.rect),
            ..Default::default()
        },
    );
    let hash = uc_uia::tree_hash(&reduced);
    let state = uc_uia::GuiState {
        goal: "<goal>",
        scene: &scene,
        elements: &reduced,
        last: None,
        dictated: None,
    };
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"state": state.to_json(), "hash": format!("{hash:016x}"), "timing": {"find_ms": scan.find_ms, "read_ms": scan.read_ms, "total_ms": scan.total_ms}, "raw_count": scan.raw_count, "est_tokens": state.estimate_tokens()})
            )?
        );
        return Ok(());
    }
    println!(
        "{} [{}] pid={} rect={:?} cursor={:?}",
        scene.title, scene.exe, scene.pid, scene.rect, scene.cursor
    );
    println!("scan: {} raw → {} kept → {} reduced | find {:.1} ms + read {:.1} ms = {:.1} ms | hash {:016x} | ~{} tokens", scan.raw_count, scan.elements.len(), reduced.len(), scan.find_ms, scan.read_ms, scan.total_ms, hash, state.estimate_tokens());
    if let Some(e) = &scan.error {
        println!("ERROR: {e}");
    }
    for e in &reduced {
        println!(
            "  e{:<3} {:<11} {:<48} box={:?}{}{}",
            e.i,
            e.role,
            e.name,
            e.bbox,
            e.val
                .as_deref()
                .map(|v| format!(" val={v:?}"))
                .unwrap_or_default(),
            if e.focused { " *focus*" } else { "" }
        );
    }
    Ok(())
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn bench_uia(delay: u64, runs: usize, context: bool, hwnd: Option<isize>) -> Result<()> {
    if hwnd.is_none() {
        countdown(delay, "Benchmarking UIA");
    }
    let scanner = uc_uia::UiaScanner::new().context("UIA init")?;
    let scene = target_scene(hwnd)?;
    let mut totals = Vec::with_capacity(runs);
    let mut finds = Vec::with_capacity(runs);
    let mut last = uc_uia::Scan::default();
    for _ in 0..runs {
        last = scanner.scan_hwnd(scene.hwnd(), context);
        totals.push(last.total_ms);
        finds.push(last.find_ms);
    }
    totals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    finds.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let reduced = uc_uia::reduce(
        &last.elements,
        uc_uia::ReduceOpts {
            viewport: Some(scene.rect),
            ..Default::default()
        },
    );
    let out = json!({
        "bench": "R1-uia-rust", "ts_unix": ts_unix(), "app": scene.exe, "title": scene.title, "runs": runs, "context": context,
        "raw_count": last.raw_count, "kept": last.elements.len(), "reduced": reduced.len(),
        "total_ms": {"min": totals[0], "p50": percentile(&totals, 0.5), "p95": percentile(&totals, 0.95), "max": totals[runs-1]},
        "find_ms":  {"min": finds[0],  "p50": percentile(&finds, 0.5),  "p95": percentile(&finds, 0.95)},
        "error": last.error,
    });
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

fn synthetic_state(
    n: usize,
) -> (
    serde_json::Value,
    serde_json::Map<String, serde_json::Value>,
) {
    let names = [
        "File",
        "Edit",
        "View",
        "Search",
        "Font",
        "Bold",
        "Italic",
        "Zoom",
        "Help",
        "Cancel",
        "Don't Save",
        "Save",
        "Save As",
        "Print",
        "Close",
        "Settings",
        "Undo",
        "Redo",
        "Cut",
        "Copy",
        "Paste",
        "Find",
        "Replace",
        "Insert",
        "Table",
        "Image",
        "Link",
        "Comment",
        "Share",
        "Export",
    ];
    let kinds = ["button", "edit", "menuitem", "checkbox", "link", "tabitem"];
    let mut elements = Vec::with_capacity(n);
    let mut criteria: Vec<(String, String)> = Vec::with_capacity(n + 1);
    for i in 0..n {
        let name = if i < names.len() {
            names[i].to_string()
        } else {
            format!("{} {i}", names[i % names.len()])
        };
        let role = if matches!(
            names[i % names.len()],
            "Save" | "Cancel" | "Don't Save" | "Close"
        ) {
            "button"
        } else {
            kinds[i % kinds.len()]
        };
        elements.push(json!({"i": i, "role": role, "name": name, "box": [40 + (i % 8) * 120, 60 + (i / 8) * 48, 110, 32]}));
        criteria.push((format!("e{i}"), format!("{role} „{name}”")));
    }
    criteria.push((
        "none".into(),
        "No listed element advances `goal`; a key, scroll or wait is needed.".into(),
    ));
    let state = json!({"goal": "Save the current document and close the dialog", "scene": {"app": "editor", "title": "Untitled - Editor - Save changes?", "fg": true}, "last": {"op": "key", "key": "Ctrl+W"}, "elements": elements});
    let q = uc_loop::questions::bundle(&criteria);
    (state, q)
}

fn bench_jev(runs: usize, sizes: &str, hedge_ms: u64, provider: Option<&str>) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let mut cfg = match provider {
            Some("typesafe") => uc_jev::Config::for_provider(uc_jev::Provider::Typesafe)?,
            Some("openrouter") => uc_jev::Config::for_provider(uc_jev::Provider::OpenRouter)?,
            Some(other) => anyhow::bail!("unknown provider {other}"),
            None => uc_jev::Config::discover()?,
        };
        if hedge_ms > 0 {
            cfg.hedge_after = Some(Duration::from_millis(hedge_ms));
        }
        let provider = cfg.provider;
        let client = uc_jev::Client::new(cfg)?;
        let warm = client.warm().await?;
        eprintln!("provider {provider:?} warm {warm:.0} ms");
        for n in sizes.split(',').filter_map(|s| s.trim().parse::<usize>().ok()) {
            let (state, q) = synthetic_state(n);
            let state_bytes = serde_json::to_vec(&state)?;
            let compiled = uc_jev::Compiled::new(&q);
            let mut http = Vec::with_capacity(runs);
            let mut totals = Vec::with_capacity(runs);
            let mut hedge_wins = 0u32;
            let mut first: Option<uc_jev::Decision> = None;
            for _ in 0..runs {
                let d = client.decide(&state_bytes, &compiled).await?;
                http.push(d.timing.http_ms);
                totals.push(d.timing.total_ms);
                if d.timing.winner == 1 {
                    hedge_wins += 1;
                }
                if first.is_none() {
                    first = Some(d);
                }
            }
            http.sort_by(|a, b| a.partial_cmp(b).unwrap());
            totals.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let d = first.expect("at least one run");
            let out = json!({
                "bench": "R2-jev-rust", "ts_unix": ts_unix(), "provider": provider, "model": d.model, "n_elements": n, "runs": runs,
                "hedge_ms": hedge_ms, "hedge_wins": hedge_wins, "requests_sent": client.requests_sent.load(std::sync::atomic::Ordering::Relaxed),
                "input_tokens": d.usage.input_tokens, "cost_per_call_usd": d.cost_usd,
                "http_ms": {"min": http[0], "p50": percentile(&http, 0.5), "p95": percentile(&http, 0.95), "max": http[runs-1]},
                "total_ms": {"p50": percentile(&totals, 0.5), "p95": percentile(&totals, 0.95)},
                "target": d.top("target", 3), "target_conf": d.choice("target").map(|c| c.1), "target_entropy": d.entropy_norm("target"),
                "op": d.choice("op").map(|c| c.0.to_string()), "goal_reached": d.noul("goal_reached"), "needs_text": d.noul("needs_text"), "is_destructive": d.noul("is_destructive"),
            });
            println!("{}", serde_json::to_string(&out)?);
        }
        Ok::<(), anyhow::Error>(())
    })
}

fn ps(script: &str, allow_destructive: bool) -> Result<()> {
    let t = Instant::now();
    let mut sh = uc_shell::PowerShell::start()?;
    eprintln!("session start {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);
    let out = sh.run(script, Duration::from_secs(30), allow_destructive)?;
    println!("{}", out.text);
    eprintln!(
        "ok={} elapsed {:.1} ms",
        out.ok,
        out.elapsed.as_secs_f64() * 1000.0
    );
    Ok(())
}

fn ts_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(clap::Args)]
struct RunArgs {
    /// The goal, in any language. Text to type goes in quotes: „…”, "…" or '…' (or --text).
    goal: String,
    /// Inject input. Without it the loop shows the first decision and stops.
    #[arg(long)]
    act: bool,
    #[arg(long, default_value_t = uc_loop::consts::MAX_STEPS_DEFAULT)]
    max_steps: usize,
    /// Text for `type` steps (overrides the quoted part of the goal).
    #[arg(long)]
    text: Option<String>,
    /// Allow actions on controls named delete/send/pay… and steps Jev rates destructive.
    #[arg(long)]
    allow_irreversible: bool,
    /// Seconds to switch to the target window before the first step (ignored with --hwnd).
    #[arg(long, default_value_t = 3)]
    delay: u64,
    /// Bring this window to the front first (from `(Get-Process notepad).MainWindowHandle`).
    #[arg(long)]
    hwnd: Option<isize>,
    /// typesafe | openrouter (default: vendor first).
    #[arg(long)]
    provider: Option<String>,
    /// One JSON line per step instead of text.
    #[arg(long)]
    json: bool,
    /// Where the JSONL ledger goes (one file per run).
    #[arg(long, default_value = "runs")]
    ledger_dir: String,
}

fn run(a: RunArgs) -> Result<()> {
    let provider = match a.provider.as_deref() {
        None => None,
        Some("typesafe") => Some(uc_jev::Provider::Typesafe),
        Some("openrouter") => Some(uc_jev::Provider::OpenRouter),
        Some(other) => anyhow::bail!("unknown provider {other}"),
    };
    let dictated = a.text.clone().or_else(|| uc_loop::extract_quoted(&a.goal));
    let opts = uc_loop::RunOpts {
        act: a.act,
        max_steps: a.max_steps,
        allow_irreversible: a.allow_irreversible,
        dictated: dictated.clone(),
        ledger_dir: Some(a.ledger_dir.clone().into()),
        provider,
        target_pid: a
            .hwnd
            .map(|h| uc_win32::window_pid(uc_win32::HWND(h as *mut core::ffi::c_void))),
        target_hwnd: a.hwnd,
        stop: None,
    };
    let mut runner = uc_loop::Runner::new(opts).context("runner init")?;
    let warm = runner.warm().context("jev warm-up")?;
    eprintln!(
        "provider {:?} warm {warm:.0} ms | mode {} | text {}",
        runner.provider(),
        if a.act {
            "ACT"
        } else {
            "preview (pass --act to inject)"
        },
        dictated
            .as_deref()
            .map(|t| format!("„{t}”"))
            .unwrap_or_else(|| "-".into())
    );
    let json_out = a.json;
    runner.on_step = Some(Box::new(move |rec| {
        if json_out {
            println!("{}", serde_json::to_string(rec).unwrap_or_default());
        } else {
            println!("{}", format_step(rec));
        }
    }));
    match a.hwnd {
        Some(h) => {
            let hwnd = uc_win32::HWND(h as *mut core::ffi::c_void);
            if !uc_win32::bring_to_front(hwnd) {
                anyhow::bail!("could not bring hwnd {h} to the front");
            }
        }
        None => countdown(a.delay, "run"),
    }
    let summary = runner.run(&a.goal).context("run")?;
    if json_out {
        println!("{}", serde_json::to_string(&summary)?);
    } else {
        println!(
            "outcome: {:?} | steps {} | {:.0} ms | jev calls {} | ${:.5}{}",
            summary.outcome,
            summary.steps,
            summary.elapsed_ms,
            summary.jev_calls,
            summary.cost_usd,
            summary
                .ledger
                .as_ref()
                .map(|p| format!(" | ledger {}", p.display()))
                .unwrap_or_default()
        );
    }
    // 0 = goal reached / preview shown; 2 = stopped (uncertain, blocked, needs text,
    // budget, focus lost); 3 = the target window is gone (often the goal, not provable).
    match summary.outcome {
        uc_loop::Outcome::Done | uc_loop::Outcome::Preview => {}
        uc_loop::Outcome::TargetGone => std::process::exit(3),
        _ => std::process::exit(2),
    }
    Ok(())
}

fn describe_action(a: &uc_loop::policy::Action) -> String {
    use uc_loop::policy::Action;
    match a {
        Action::Click {
            target,
            name,
            x,
            y,
            right,
        } => format!(
            "{} e{target} {name} @({x},{y})",
            if *right { "right-click" } else { "click" }
        ),
        Action::Type {
            text, target_name, ..
        } => format!(
            "type „{text}” into {}",
            target_name.as_deref().unwrap_or("the focused control")
        ),
        Action::Key { key } => format!("key {key}"),
        Action::Scroll { notches, .. } => format!("scroll {notches}"),
        Action::Wait => "wait".into(),
    }
}

fn format_step(r: &uc_loop::StepRecord) -> String {
    use uc_loop::policy::Verdict;
    let s = &r.signals;
    let verdict = match &r.verdict {
        Verdict::Done => "DONE".to_string(),
        Verdict::Act { action } => format!(
            "{}{}",
            describe_action(action),
            if r.executed { " ✓" } else { " [preview]" }
        ),
        Verdict::Uncertain { reason, .. } => format!("UNCERTAIN: {reason}"),
        Verdict::NeedsText => "NEEDS TEXT (quote it in the goal or pass --text)".to_string(),
        Verdict::Blocked { reason } => format!("BLOCKED: {reason}"),
    };
    let title: String = r.title.chars().take(40).collect();
    let changed = match r.changed {
        None => "",
        Some(true) => " changed",
        Some(false) => " NO CHANGE",
    };
    format!(
        "#{:<2} {} „{}”{} | scan {:.0} ms ({}→{}) | jev {:.0} ms{} {} tok | target {} {} {:.2} gap {:.2} H {:.2} | op {} {:.2} key {} {:.2} | goal {:.2} ({:.2}/{:.2}) text {:.2} destr {:.2}\n    → {}",
        r.step,
        r.app,
        title,
        changed,
        r.scan_ms,
        r.raw_elements,
        r.sent_elements,
        r.jev_ms,
        if r.narrowed { " (narrowed ×2)" } else { "" },
        r.input_tokens,
        s.target,
        s.target_name.as_deref().unwrap_or("-"),
        s.target_conf,
        s.target_gap,
        s.target_entropy,
        s.op,
        s.op_conf,
        s.key,
        s.key_conf,
        s.goal_reached,
        s.goal_a,
        s.goal_pending,
        s.needs_text,
        s.is_destructive,
        verdict
    )
}
