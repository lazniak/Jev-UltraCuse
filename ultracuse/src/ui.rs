//! The window: a goal in, steps out. One eframe/egui viewport (wgpu, AccessKit for
//! screen readers), the loop on its own thread, messages over a channel.
//!
//! The window never becomes the target: the user picks a window (or "the last active
//! one"), the loop brings it to the front and locks onto its process. The loop itself is
//! untouched — this is the same `uc_loop::Runner` the CLI uses.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui;
use uc_loop::policy::{Action, Verdict};
use uc_loop::{Outcome, RunOpts, RunSummary, Runner, StepRecord};
use uc_win32::WindowInfo;

const WINDOW_TITLE: &str = "Jev-UltraCuse";
const POLL_MS: u64 = 250;

pub fn run_ui() -> Result<()> {
    uc_win32::hide_own_console();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(WINDOW_TITLE)
            .with_inner_size([620.0, 720.0])
            .with_min_inner_size([480.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("okno: {e}"))
}

enum Msg {
    Ready { provider: String, warm_ms: f64 },
    Step(Box<StepRecord>),
    Done(Box<RunSummary>),
    Error(String),
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Idle,
    Starting,
    Running,
    Finished,
}

#[derive(Clone, Copy, PartialEq)]
enum ProviderChoice {
    Auto,
    Typesafe,
    OpenRouter,
}

struct App {
    goal: String,
    text: String,
    act: bool,
    allow_irreversible: bool,
    max_steps: usize,
    provider: ProviderChoice,
    windows: Vec<WindowInfo>,
    target: Option<isize>,
    last_active: Option<WindowInfo>,
    use_last_active: bool,
    state: State,
    status: String,
    steps: Vec<StepRecord>,
    summary: Option<RunSummary>,
    error: Option<String>,
    rx: Option<Receiver<Msg>>,
    stop: Arc<AtomicBool>,
    last_poll: Instant,
    show_settings: bool,
    own_pid: u32,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        cc.egui_ctx.all_styles_mut(|s| {
            for font in s.text_styles.values_mut() {
                font.size *= 1.15;
            }
            s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        });
        Self {
            goal: String::new(),
            text: String::new(),
            act: false,
            allow_irreversible: false,
            max_steps: uc_loop::consts::MAX_STEPS_DEFAULT,
            provider: ProviderChoice::Auto,
            windows: uc_win32::list_windows(),
            target: None,
            last_active: None,
            use_last_active: true,
            state: State::Idle,
            status: "gotowe".into(),
            steps: Vec::new(),
            summary: None,
            error: None,
            rx: None,
            stop: Arc::new(AtomicBool::new(false)),
            last_poll: Instant::now(),
            show_settings: false,
            own_pid: std::process::id(),
        }
    }

    fn running(&self) -> bool {
        matches!(self.state, State::Starting | State::Running)
    }

    fn can_start(&self) -> bool {
        !self.running() && !self.goal.trim().is_empty()
    }

    fn chosen_target(&self) -> Option<WindowInfo> {
        self.windows
            .iter()
            .find(|w| Some(w.hwnd) == self.target)
            .cloned()
    }

    fn start(&mut self, ctx: &egui::Context) {
        let Some(win) = self.chosen_target() else {
            self.error = Some(
                "Wybierz okno docelowe (albo kliknij w nie, żeby stało się „ostatnio aktywne”)."
                    .into(),
            );
            return;
        };
        let goal = self.goal.trim().to_string();
        let dictated = if self.text.trim().is_empty() {
            uc_loop::extract_quoted(&goal)
        } else {
            Some(self.text.clone())
        };
        let provider = match self.provider {
            ProviderChoice::Auto => None,
            ProviderChoice::Typesafe => Some(uc_jev::Provider::Typesafe),
            ProviderChoice::OpenRouter => Some(uc_jev::Provider::OpenRouter),
        };
        let opts = RunOpts {
            act: self.act,
            max_steps: self.max_steps,
            allow_irreversible: self.allow_irreversible,
            dictated,
            ledger_dir: Some("runs".into()),
            provider,
            target_pid: Some(win.pid),
            target_hwnd: Some(win.hwnd),
            stop: Some(self.stop.clone()),
        };
        self.stop.store(false, Ordering::Relaxed);
        self.steps.clear();
        self.summary = None;
        self.error = None;
        self.state = State::Starting;
        self.status = format!("start: {} ({})", win.title, win.exe);
        let (tx, rx) = mpsc::channel::<Msg>();
        self.rx = Some(rx);
        let ctx = ctx.clone();
        let hwnd = win.hwnd;
        let spawned = std::thread::Builder::new()
            .name("uc-run".into())
            .spawn(move || {
                let send = |m: Msg| {
                    let _ = tx.send(m);
                    ctx.request_repaint();
                };
                let mut runner = match Runner::new(opts) {
                    Ok(r) => r,
                    Err(e) => return send(Msg::Error(e.to_string())),
                };
                let warm_ms = match runner.warm() {
                    Ok(ms) => ms,
                    Err(e) => return send(Msg::Error(format!("rozgrzewka Jev: {e}"))),
                };
                send(Msg::Ready {
                    provider: format!("{:?}", runner.provider()),
                    warm_ms,
                });
                let tx_step = tx.clone();
                let ctx_step = ctx.clone();
                runner.on_step = Some(Box::new(move |r| {
                    let _ = tx_step.send(Msg::Step(Box::new(r.clone())));
                    ctx_step.request_repaint();
                }));
                if !uc_win32::bring_to_front(uc_win32::HWND(hwnd as *mut core::ffi::c_void)) {
                    return send(Msg::Error(
                        "nie udało się wysunąć okna docelowego na wierzch".into(),
                    ));
                }
                match runner.run(&goal) {
                    Ok(s) => send(Msg::Done(Box::new(s))),
                    Err(e) => send(Msg::Error(e.to_string())),
                }
            });
        if let Err(e) = spawned {
            self.error = Some(format!("wątek: {e}"));
            self.state = State::Finished;
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let mut inbox = Vec::new();
        let mut disconnected = false;
        if let Some(rx) = &self.rx {
            loop {
                match rx.try_recv() {
                    Ok(m) => inbox.push(m),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        for m in inbox {
            self.handle(m, ctx);
        }
        if disconnected {
            self.rx = None;
            if self.running() {
                self.state = State::Finished;
                self.error
                    .get_or_insert_with(|| "wątek pętli zakończył się bez wyniku".into());
            }
        }
        if self.last_poll.elapsed() >= Duration::from_millis(POLL_MS) {
            self.last_poll = Instant::now();
            if !self.running() {
                self.track_last_active();
            }
        }
    }

    /// Remember the last foreground window that is not ours — the natural target
    /// ("do it in the window I was just in").
    fn track_last_active(&mut self) {
        let Some(h) = uc_win32::foreground_hwnd() else {
            return;
        };
        let pid = uc_win32::window_pid(h);
        if pid == 0 || pid == self.own_pid {
            return;
        }
        let title = uc_win32::window_title(h);
        if title.is_empty() {
            return;
        }
        let info = WindowInfo {
            hwnd: h.0 as isize,
            title,
            exe: uc_win32::process_exe(pid).unwrap_or_default(),
            pid,
        };
        if self.use_last_active && self.target != Some(info.hwnd) {
            self.target = Some(info.hwnd);
            self.windows = uc_win32::list_windows();
            if !self.windows.iter().any(|w| w.hwnd == info.hwnd) {
                self.windows.insert(0, info.clone());
            }
        }
        self.last_active = Some(info);
    }

    fn handle(&mut self, m: Msg, ctx: &egui::Context) {
        match m {
            Msg::Ready { provider, warm_ms } => {
                self.state = State::Running;
                self.status = format!("{provider}, rozgrzewka {warm_ms:.0} ms — działa");
            }
            Msg::Step(r) => self.steps.push(*r),
            Msg::Done(s) => {
                self.status = outcome_text(&s.outcome);
                self.summary = Some(*s);
                self.state = State::Finished;
                ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                    egui::UserAttentionType::Informational,
                ));
            }
            Msg::Error(e) => {
                self.error = Some(e);
                self.state = State::Finished;
                self.status = "błąd".into();
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx_owned = root.ctx().clone();
        let ctx = &ctx_owned;
        self.poll(ctx);
        let ctrl_enter = ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter));
        if ctrl_enter && self.can_start() {
            self.start(ctx);
        }
        if self.running() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.stop.store(true, Ordering::Relaxed);
        }

        egui::Panel::top("top").show(root, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading(WINDOW_TITLE);
                ui.label(egui::RichText::new(&self.status).weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⚙ Ustawienia").clicked() {
                        self.show_settings = true;
                    }
                    if self.running() {
                        ui.spinner();
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::Panel::bottom("bottom").show(root, |ui| {
            ui.add_space(4.0);
            match &self.summary {
                Some(s) => {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(egui::RichText::new(outcome_text(&s.outcome)).strong());
                        ui.label(format!(
                            "· {} kroków · {:.1} s · {} wywołań Jev · ${:.5}",
                            s.steps,
                            s.elapsed_ms / 1000.0,
                            s.jev_calls,
                            s.cost_usd
                        ));
                        if let Some(p) = &s.ledger {
                            ui.label(egui::RichText::new(format!("· {}", p.display())).weak());
                        }
                    });
                }
                None => {
                    ui.label(
                        egui::RichText::new(
                            "Ctrl+Enter = start · Esc = stop · Ctrl+Alt+K = kill-switch (zawsze, także poza oknem)",
                        )
                        .weak(),
                    );
                }
            }
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(root, |ui| {
            self.target_section(ui);
            ui.add_space(4.0);
            self.goal_section(ui);
            ui.add_space(4.0);
            self.controls(ui, ctx);
            ui.separator();
            if let Some(e) = &self.error {
                ui.colored_label(egui::Color32::from_rgb(255, 110, 110), e);
            }
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if self.steps.is_empty() && !self.running() {
                        ui.label(
                            egui::RichText::new(
                                "Kroki pojawią się tutaj: co pętla zobaczyła, co zdecydował Jev, co zrobiła.",
                            )
                            .weak(),
                        );
                    }
                    for rec in &self.steps {
                        step_card(ui, rec);
                    }
                });
        });

        if self.show_settings {
            self.settings_modal(ctx);
        }

        ctx.request_repaint_after(Duration::from_millis(if self.running() {
            100
        } else {
            POLL_MS
        }));
    }
}

impl App {
    fn target_section(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.set_width(ui.available_width());
            ui.label(egui::RichText::new("Okno docelowe").strong());
            ui.horizontal(|ui| {
                let selected = self
                    .chosen_target()
                    .map(|w| format!("{} ({})", short(&w.title, 40), w.exe))
                    .unwrap_or_else(|| "— wybierz okno —".into());
                let combo = egui::ComboBox::from_id_salt("target-window")
                    .width(400.0)
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        for w in &self.windows {
                            ui.selectable_value(
                                &mut self.target,
                                Some(w.hwnd),
                                format!("{} ({})", short(&w.title, 52), w.exe),
                            );
                        }
                    });
                if combo.response.clicked() {
                    self.windows = uc_win32::list_windows();
                }
                if ui
                    .button("↻")
                    .on_hover_text("odśwież listę okien")
                    .clicked()
                {
                    self.windows = uc_win32::list_windows();
                }
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.use_last_active, "śledź ostatnio aktywne okno")
                    .on_hover_text("Kliknij w docelowe okno, wróć tutaj — będzie już wybrane.");
                match self.chosen_target() {
                    Some(w) => {
                        ui.label(egui::RichText::new(format!("pid {} · {}", w.pid, w.exe)).weak());
                    }
                    None => {
                        ui.label(
                            egui::RichText::new("kliknij w docelowe okno albo wybierz z listy")
                                .weak(),
                        );
                    }
                }
            });
        });
    }

    fn goal_section(&mut self, ui: &mut egui::Ui) {
        let label = ui.label(egui::RichText::new("Cel").strong());
        ui.add(
            egui::TextEdit::multiline(&mut self.goal)
                .desired_rows(2)
                .desired_width(f32::INFINITY)
                .hint_text(
                    "np. Wpisz „hello” w edytorze tekstu · Zamknij kartę bez zapisywania zmian",
                ),
        )
        .labelled_by(label.id);
        let label = ui.label("Tekst do wpisania (opcjonalnie — inaczej z cudzysłowu w celu)");
        ui.add(
            egui::TextEdit::singleline(&mut self.text)
                .desired_width(f32::INFINITY)
                .hint_text("Jev nigdy nie generuje tekstu; tekst pochodzi od Ciebie"),
        )
        .labelled_by(label.id);
    }

    fn controls(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal_wrapped(|ui| {
            ui.checkbox(&mut self.act, "Uzbrojone (wstrzykuje mysz i klawiaturę)")
                .on_hover_text("Bez tego pętla pokazuje pierwszą decyzję i nic nie robi.");
            ui.checkbox(&mut self.allow_irreversible, "Zezwól na nieodwracalne")
                .on_hover_text("Usuń / wyślij / zapłać… i kroki ocenione jako destrukcyjne. Nadal wymagana pewność ≥ 0.85.");
            ui.label("Kroki");
            ui.add(egui::DragValue::new(&mut self.max_steps).range(1..=40));
        });
        ui.horizontal(|ui| {
            let start = ui.add_enabled(
                self.can_start(),
                egui::Button::new(egui::RichText::new("▶  Start").strong()),
            );
            if start.clicked() {
                self.start(ctx);
            }
            let stop = ui.add_enabled(self.running(), egui::Button::new("■  Stop"));
            if stop.clicked() {
                self.stop.store(true, Ordering::Relaxed);
            }
            if !self.act {
                ui.label(egui::RichText::new("tryb podglądu").weak());
            }
        });
    }

    fn settings_modal(&mut self, ctx: &egui::Context) {
        let modal = egui::Modal::new(egui::Id::new("settings")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading("Ustawienia");
            ui.separator();
            ui.label(egui::RichText::new("Końcówka Jev").strong());
            ui.radio_value(&mut self.provider, ProviderChoice::Auto, "auto (vendor, gdy jest JEV_API_KEY)");
            ui.radio_value(&mut self.provider, ProviderChoice::Typesafe, "TypeSafe (api.typesafe.ai)");
            ui.radio_value(&mut self.provider, ProviderChoice::OpenRouter, "OpenRouter (typesafe/jev-1.13)");
            ui.separator();
            ui.label(egui::RichText::new("System Two (LLM z OpenRouter)").strong());
            ui.label(
                egui::RichText::new(
                    "Równoległy LLM do planu i tekstu, z wyborem modelu — w przygotowaniu (TASKS 3.4).",
                )
                .weak(),
            );
            ui.separator();
            ui.label(egui::RichText::new("Skróty").strong());
            ui.label("Ctrl+Enter start · Esc stop · Ctrl+Alt+K kill-switch");
            ui.add_space(6.0);
            if ui.button("Zamknij").clicked() {
                self.show_settings = false;
            }
        });
        if modal.should_close() {
            self.show_settings = false;
        }
    }
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for path in [
        "C:\\Windows\\Fonts\\segoeui.ttf",
        "C:\\Windows\\Fonts\\arial.ttf",
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert("system".into(), Arc::new(egui::FontData::from_owned(bytes)));
            if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                list.insert(0, "system".into());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn outcome_text(o: &Outcome) -> String {
    match o {
        Outcome::Done => "✔ Cel osiągnięty".into(),
        Outcome::Preview => "Podgląd: pokazano pierwszą decyzję, nic nie wstrzyknięto".into(),
        Outcome::Budget => "Wyczerpany limit kroków".into(),
        Outcome::Uncertain(r) => format!("Niepewne — {r}"),
        Outcome::NeedsText => {
            "Potrzebny tekst do wpisania (cudzysłów w celu albo pole „Tekst”)".into()
        }
        Outcome::Blocked(r) => format!("Zablokowane — {r}"),
        Outcome::Stalled => "Brak zmian na ekranie po akcjach".into(),
        Outcome::Killed => "Kill-switch Ctrl+Alt+K".into(),
        Outcome::Stopped => "Zatrzymane".into(),
        Outcome::FocusLost(r) => format!("Utrata fokusu — {r}"),
        Outcome::TargetGone => "Okno docelowe zniknęło (przy „zamknij” to zwykle sukces)".into(),
    }
}

fn action_text(a: &Action) -> String {
    match a {
        Action::Click {
            name, x, y, right, ..
        } => format!(
            "{} {name} @({x},{y})",
            if *right { "prawy klik" } else { "klik" }
        ),
        Action::Type {
            text, target_name, ..
        } => format!(
            "wpisz „{text}” → {}",
            target_name.as_deref().unwrap_or("aktywna kontrolka")
        ),
        Action::Key { key } => format!("klawisz {key}"),
        Action::Scroll { notches, .. } => {
            if *notches > 0 {
                "przewiń w dół".into()
            } else {
                "przewiń w górę".into()
            }
        }
        Action::Wait => "czekaj".into(),
    }
}

fn step_card(ui: &mut egui::Ui, r: &StepRecord) {
    let (verdict, color) = match &r.verdict {
        Verdict::Done => (
            "✔ cel osiągnięty".to_string(),
            egui::Color32::from_rgb(120, 220, 140),
        ),
        Verdict::Act { action } => (
            format!(
                "{} {}",
                action_text(action),
                if r.executed { "✓" } else { "[podgląd]" }
            ),
            if r.executed {
                egui::Color32::from_rgb(120, 180, 255)
            } else {
                egui::Color32::GRAY
            },
        ),
        Verdict::Uncertain { reason, .. } => (
            format!("niepewne — {reason}"),
            egui::Color32::from_rgb(255, 200, 90),
        ),
        Verdict::NeedsText => (
            "potrzebny tekst".to_string(),
            egui::Color32::from_rgb(255, 200, 90),
        ),
        Verdict::Blocked { reason } => (
            format!("zablokowane — {reason}"),
            egui::Color32::from_rgb(255, 110, 110),
        ),
    };
    let s = &r.signals;
    ui.group(|ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(format!("#{}", r.step)).strong());
            ui.label(format!("{} „{}”", r.app, short(&r.title, 40)));
            if r.changed == Some(false) {
                ui.label(egui::RichText::new("bez zmian").weak());
            }
            ui.label(
                egui::RichText::new(format!(
                    "· skan {:.0} ms ({} el.) · Jev {:.0} ms{} · ${:.5}",
                    r.scan_ms,
                    r.sent_elements,
                    r.jev_ms,
                    if r.narrowed { " (×2, zawężenie)" } else { "" },
                    r.cost_usd
                ))
                .weak(),
            );
        });
        ui.colored_label(color, format!("→ {verdict}"));
        ui.label(
            egui::RichText::new(format!(
                "cel {:.2} · tekst {:.2} · destrukcyjne {:.2} · target {} {} ({:.2}, gap {:.2}) · op {} {:.2}",
                s.goal_reached,
                s.needs_text,
                s.is_destructive,
                s.target,
                s.target_name.as_deref().unwrap_or("-"),
                s.target_conf,
                s.target_gap,
                s.op,
                s.op_conf
            ))
            .weak()
            .small(),
        );
    });
}
