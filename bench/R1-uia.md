# R1 — skan UIA: Rust vs Python na tych samych oknach

Data: 2026-09-20 21:50. Maszyna: i9-7960X, 128 GB, Windows 11 26100, DPI per-monitor v2. Metoda: jedno `FindAllBuildCache(TreeScope_Descendants, OR(15 interaktywnych ControlType), CacheRequest{Name, AutomationId, ControlType, BoundingRectangle, IsEnabled, IsOffscreen, Value, HasKeyboardFocus})` na wskazanym `hwnd`, 20 powtórzeń, ten sam zestaw typów w obu implementacjach. Rust: `ultracuse bench-uia --hwnd <h> --runs 20` (release, LTO). Python: `bench/r1_python.py <h…>` (JevUse `uia_fast.UiaScanner`, comtypes). Surowe wyniki: `R1-uia-rust.jsonl`, `R1-uia-python.jsonl`.

| Okno | raw / kept | **Rust p50** | Rust p95 | **Python p50** | Python p95 |
|---|---:|---:|---:|---:|---:|
| Notatnik (Windows 11, WinUI) | 101 / 31 | **380.3 ms** | 394.6 | **387.9 ms** | 432.2 |
| Chrome (strona GitHub Issues) | 145 / 126 | **108.5 ms** | 118.9 | **106.1 ms** | 121.7 |
| Claude Desktop (Electron) | 785 / 161 | **464.0 ms** | 491.9 | **478.1 ms** | 581.7 |
| MLRC Projector (Electron, renderer a11y wyłączone) | 0 / 0 | 217.9 ms | 234.7 | 216.8 ms | 233.5 |

Faza odczytu z cache do struktur w Rust: **0.1–0.3 ms** na 100–800 elementów; `find` (RPC do providera) = 99.9 % czasu.

Z `--context` (dodatkowo `text`, `custom`, `document`): Notatnik 197 raw → 430 ms; Chrome 159 raw → 110.6 ms.

## Odczyt

1. **Język klienta nie ma znaczenia dla UIA** — różnice ≤ 3 % mieszczą się w szumie. Potwierdza tezę z research JevUse (96–97 % czasu po stronie providera). Ogon p95 w Rust jest węższy (Claude: 492 vs 582), co przy pętli z settle ma znaczenie, ale to efekt braku GIL/alokacji, nie „szybszego COM".
2. **Koszt zależy od aplikacji, nie od liczby elementów**: Chrome zwraca 145 elementów w 108 ms, Notatnik Win11 zwraca 101 w 380 ms, pusty Electron bez a11y kosztuje 218 ms za nic. Dźwignie to: cache drzewa + zdarzenia UIA zamiast pełnego skanu co krok (TASKS 1.1), a dla Electron/CEF wymuszenie dostępności renderera (`--force-renderer-accessibility` lub obecność klienta AT) i fallback OCR (TASKS 3.3).
3. Budżet kroku z §7 architektury (percepcja 5–60 ms) jest **nieosiągalny pełnym skanem** na Win11 Notepad/Electron; jest osiągalny tylko z cache + zdarzeniami. To zmienia priorytet zadania 1.1 na najwyższy.

## Do zrobienia

- Ten sam pomiar na: Eksplorator (`CabinetWClass`), Ustawienia, Kalkulator, VS Code, Word — po jednym `hwnd` z `(Get-Process …).MainWindowHandle`.
- Wariant „drugi skan po zdarzeniu StructureChanged" — czy provider odpowiada szybciej, gdy drzewo jest już zbudowane (Chrome sugeruje tak).
