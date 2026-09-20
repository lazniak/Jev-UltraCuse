# Jev-UltraCuse

Sterowanie komputerem (Windows 11) głosem, klawiaturą i myszą „jak urządzenie HID", z decyzjami modelu **Jev 1.13** (TypeSafe System One) w pętli — jako **jeden przenośny `ultracuse.exe`**. Cel: najszybszy Computer Use, dostępny także dla osób z niepełnosprawnościami (dyktowanie po polsku, zero precyzji myszy).

Dokumentacja decyzyjna (czytać w tej kolejności):

1. [docs/01-jev-computer-use-analiza.md](docs/01-jev-computer-use-analiza.md) — co Jev robi i czego nie robi w Computer Use; wszystkie zmierzone liczby; 11 słabości modelu → reguły; luka względem istniejących implementacji.
2. [docs/02-ADR-jezyk.md](docs/02-ADR-jezyk.md) — **Rust** dla binarki, Python (`D:\code\JevUse`) jako laboratorium; dlaczego nie C++ ani C#.
3. [docs/03-architektura.md](docs/03-architektura.md) — wątki, pętla kroku, budżet latencji, tor głosowy, executor, PowerShell, klient Jev, bezpieczeństwo, plan pomiarów.
4. [docs/04-STT-dostepnosc.md](docs/04-STT-dostepnosc.md) — **ADR-002**: ElevenLabs Scribe v2 Realtime online, whisper.cpp offline, Nemotron streaming jako tor eksperymentalny.
5. [TASKS.md](TASKS.md) — fazy i zadania.

## Zasada jednego zdania

Jev odpowiada tylko na *które / czy / jak bardzo* (jeden POST, wiele pytań, ~300 ms); tekst pochodzi z dyktowania; porównania stanów, liczenie i guardy robi kod; screenshot nigdy nie trafia do modelu.

## Budowa

Wymagania: Rust 1.85+ (MSVC), Windows 11, klucz Jev w env lub `HKCU\Environment`: domyślnie końcówka vendora (`JEV_API_KEY`); opcjonalnie OpenRouter (`OPENROUTER_API_KEY` lub `OPEN_ROUTER_API_KEY`), wymuszany przez `--provider openrouter` albo `UC_PROVIDER=openrouter`.

```bash
cargo build --release
target\release\ultracuse.exe doctor
target\release\ultracuse.exe probe --delay 3
target\release\ultracuse.exe bench-uia --runs 20
target\release\ultracuse.exe bench-jev --runs 10 --sizes 12,30,60 --hedge-ms 400
target\release\ultracuse.exe ps "Get-ChildItem $env:USERPROFILE\Desktop | Select-Object -First 5 Name"
```

`click` i każda akcja wymagają `--act`; globalny kill-switch **Ctrl+Alt+K**.

## Szybki start (MVP-1)

**Okno** — `ultracuse.exe` bez argumentów (dwuklik) otwiera okno: wybór okna docelowego (lista albo „śledź ostatnio aktywne”: klikasz w docelową aplikację, wracasz, jest wybrana), pole celu, opcjonalny tekst do wpisania, przełączniki *Uzbrojone* / *Zezwól na nieodwracalne*, limit kroków, Start (Ctrl+Enter) / Stop (Esc), log kroków na żywo (co pętla zobaczyła, co zdecydował Jev, co zrobiła) i podsumowanie z kosztem. Okno nigdy nie jest celem: pętla wysuwa wybrane okno na wierzch i blokuje się na jego procesie. Renderowanie: egui/wgpu (DX12), czytniki ekranu przez AccessKit.

**CLI** — te same możliwości z terminala:

```powershell
ultracuse doctor                                                      # DPI, UIA, pwsh, klucz Jev + jedna mini-decyzja
ultracuse run "Wpisz „hello ultracuse” w edytorze tekstu"            # podgląd: 3 s na przełączenie okna, pokazuje pierwszą decyzję, nic nie wstrzykuje
ultracuse run "Wpisz „hello ultracuse” w edytorze tekstu" --act      # uzbrojone: wpisuje, sprawdza cel, kończy
ultracuse run "Zamknij kartę bez zapisywania zmian" --act --allow-irreversible --hwnd 23399772
```

Co robi jeden krok: skan UIA okna na wierzchu → redukcja do ≤ 60 kandydatów → **jedno** wywołanie Jev z siedmioma pytaniami (`target`, `op`, `key`, `goal_reached`, `goal_pending`, `needs_text`, `is_destructive`) → bramka w kodzie (progi, zgodność sygnałów, lista nieodwracalnych) → `SendInput` → settle. Tekst do wpisania bierze z cudzysłowu w celu („…”, "…", '…') albo z `--text`; Jev nigdy nie generuje tekstu.

Bezpieczniki: domyślnie podgląd (`--act` uzbraja); pętla działa tylko, gdy na wierzchu jest **proces okna, na którym wystartowała** — sprawdzane przy skanie **i ponownie tuż przed wstrzyknięciem**, bo decyzja trwa 0.3–2 s (inny proces → `FocusLost`, nic nie wstrzyknięte; okno znika → `TargetGone`); kontrolki z listy nieodwracalnych i kroki, które Jev ocenia jako destrukcyjne (≥ 0.5), wymagają `--allow-irreversible` **i** pewności ≥ 0.85; kill-switch **Ctrl+Alt+K**. Każdy przebieg zostawia ledger JSONL w `runs/` (krok po kroku: sygnały, werdykt, czasy, koszt). Kody wyjścia: 0 = cel osiągnięty / podgląd, 2 = zatrzymane (niepewność, blokada, brak tekstu, budżet kroków, utrata fokusu), 3 = okno docelowe zniknęło (`TargetGone` — przy celach typu „zamknij” to zwykle sukces, ale kod tego nie rozstrzyga).

Zmierzone (`bench/R4-mvp-run.md`): wpisanie tekstu do Notatnika — 2 kroki, 1.04 s, $0.00022; zamknięcie karty bez zapisu przez menu i dialog — 5 kroków, 2.7 s, $0.00048.

## Liczby

Każda liczba w tym README pochodzi ze skryptu w `bench/` z datą i maszyną. Do czasu pierwszych pomiarów R1–R4 (patrz [TASKS.md](TASKS.md)) jedyne liczby to te odziedziczone z laboratorium Python — w [docs/01](docs/01-jev-computer-use-analiza.md) §3.
