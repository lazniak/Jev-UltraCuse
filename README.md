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

Wymagania: Rust 1.85+ (MSVC), Windows 11, klucz Jev (`JEV_API_KEY` vendora lub `OPENROUTER_API_KEY`) w env lub `HKCU\Environment`.

```bash
cargo build --release
target\release\ultracuse.exe doctor
target\release\ultracuse.exe probe --delay 3
target\release\ultracuse.exe bench-uia --runs 20
target\release\ultracuse.exe bench-jev --runs 10 --sizes 12,30,60 --hedge-ms 400
target\release\ultracuse.exe ps "Get-ChildItem $env:USERPROFILE\Desktop | Select-Object -First 5 Name"
```

`click` i każda akcja wymagają `--act`; globalny kill-switch **Ctrl+Alt+K**.

## Liczby

Każda liczba w tym README pochodzi ze skryptu w `bench/` z datą i maszyną. Do czasu pierwszych pomiarów R1–R4 (patrz [TASKS.md](TASKS.md)) jedyne liczby to te odziedziczone z laboratorium Python — w [docs/01](docs/01-jev-computer-use-analiza.md) §3.
