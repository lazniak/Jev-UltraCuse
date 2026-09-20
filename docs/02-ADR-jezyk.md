# ADR-001: Język implementacji — Rust (rdzeń i binarka), Python jako laboratorium

Status: **przyjęty** 2026-09-20. Decyzja obowiązuje dla przenośnej binarki `ultracuse.exe`. Poprzedni research (`D:\code\JevUse\computeruse-dependencies-research.md` §3–5) rekomendował „Python + natywny rdzeń jako DLL"; ta decyzja go **odwraca** z dwóch powodów, których tamten dokument nie ważył: wymóg **jednego przenośnego `.exe`** i **STT w czasie rzeczywistym w tym samym procesie**.

## Kontekst

Aplikacja ma jednocześnie, bez przerwy:

1. nagrywać mikrofon (WASAPI 16 kHz) i transkrybować strumieniowo (silnik STT na CPU/GPU, VAD),
2. skanować UI Automation (COM, wątek STA, RPC do aplikacji docelowej),
3. utrzymywać ciepłe połączenie HTTP/2 do Jev i wysyłać hedged requesty,
4. pompować komunikaty Win32 (globalny hotkey, tray, kill-switch),
5. wstrzykiwać wejście (`SendInput`) i wykonywać komendy PowerShell,

i ma być dystrybuowana jako **jeden plik `.exe`** kopiowany na dowolny Windows 11 (użytkownicy z niepełnosprawnościami; zero instalatora, zero Pythona).

## Kryteria i ocena

Skala: ✅ przewaga · ➖ neutralne · ❌ wada. Liczby: z research JevUse §3.3 i pomiarów 2026-09-19/20.

| Kryterium | Python 3.13 | **Rust** | C++ (MSVC) | C# .NET NativeAOT |
|---|---|---|---|---|
| **Latencja gorącej pętli** — `SendInput`/Win32 | ➖ µs–ms (ctypes) | ➖ µs | ➖ µs | ➖ µs |
| **UIA** — koszt zdominowany przez RPC providera (96–97%, 110–200 ms na Electron, JevUse) | ➖ comtypes: +typelib gen ~1–2 s przy pierwszym uruchomieniu, marshalling per property | ➖ windows-rs COM, zero narzutu | ➖ zero narzutu | ➖ FlaUI/UIA3 |
| **Klient HTTP/2 + TLS** (RTT Jev ~300 ms; klient dodaje) | ➖ httpx/h2 ~1–3 ms/req | ➖ reqwest/hyper <0.5 ms | ❌ brak wygodnej h2+TLS (WinHTTP lub libcurl+schannel, ręcznie) | ➖ HttpClient |
| **Równoległość** (audio + STT + COM STA + HTTP + pompa komunikatów) | ❌ GIL; wątki rywalizują; free-threading 3.14 bez gwarancji dla comtypes/pywin32 | ✅ wątki natywne, `Send`/`Sync` pilnują COM STA w typach | ✅ | ✅ |
| **Jeden przenośny `.exe`** | ❌ PyInstaller onefile: ~26 MB skromna apka, **rozpakowanie do %TEMP% ≈1.8 s na zimno**, główne źródło fałszywych alarmów AV (issue #6754); Nuitka onefile: ≈22 MB, start ≈0.08 s, ale znane problemy z bundlowaniem onnxruntime+numpy (issue #1740); PyOxidizer porzucony (2023); comtypes generuje typelib w runtime | ✅ 3–10 MB własnego kodu (+10–20 MB z reqwest/tokio), start <100 ms, statyczny CRT, jeden podpis | ✅ jak Rust | ➖ AOT: 1–31 MB; **UIA przez klasyczny COM interop nie działa pod NativeAOT** (FlaUI #672 otwarte od 2025-02) |
| **STT w procesie** (sherpa-onnx / whisper.cpp = C/C++) | ➖ wheels `sherpa-onnx` są; onnxruntime.dll + model obok exe; GIL zwalniany tylko w inferencji | ✅ `whisper-rs` 0.16 (2026-03, CUDA), oficjalne bindingi Rust w repo k2-fsa/sherpa-onnx (**`sherpa-rs` deprecated** — nie używać), `ort` 2.0-rc13 — linkowane statycznie; **DLL runtime'u STT (15–40 MB) dominuje rozmiar w każdym języku** | ✅ natywnie | ➖ P/Invoke |
| **Dostępność zależności** (pytanie użytkownika) | ✅ najbogatszy ekosystem; `uiautomation` 2.0.29 (2025-08), `comtypes`, `pywin32` build 312, `dxcam` (2026), `sherpa-onnx`; uwaga: `httpx` przeszło pod Pydantic jako `httpx2`, `pydirectinput(-rgx)` nieaktywne | ✅ `windows` 0.62.2 (oficjalne MS; 0.100 = przyszły rewrite z `windows-clang`, nie blokuje), `uiautomation` 0.25.1 (2026-09-04, **ma `CacheRequest` i handlery zdarzeń**), `windows-capture` 2.0.1 (2026-08, dirty-rects, cursor toggle), `reqwest` (h2 domyślnie; `ureq` h2 niejasne), `tokio-tungstenite` 0.29+, `cpal` 0.18 / `wasapi` 0.24, `global-hotkey` 0.8 / `tray-icon` 0.25 (zespół Tauri); `rdev` martwe → `SendInput` własny na `windows`; `scrap` porzucone | ➖ vcpkg ma `cppwinrt`, `cpp-httplib`, `curl`, ale brak „klocków" pod tray/hotkey/GUI/async; ręczne zarządzanie COM | ➖ FlaUI/UIA3 świetne pod JIT, ale nie pod AOT; NAudio; STT przez natywne DLL; SDK PowerShell nie działa pod AOT |
| **HID jak urządzenie** (`SendInput` ze scan-code; driver Interception opcjonalnie) | ➖ | ➖ | ➖ | ➖ |
| **PowerShell** | ➖ spawn `pwsh` | ➖ spawn `pwsh` (jedna ciepła sesja przez potok) | ➖ | ✅ hosting in-proc (ale nie pod AOT) |
| **Bezpieczeństwo pamięci przy 24/7 działaniu u osoby zależnej od narzędzia** | ✅ | ✅ | ❌ | ✅ |
| **Szybkość iteracji / prototypowania** | ✅ najlepsza | ➖ | ❌ | ➖ |
| **Koszt utrzymania** | niski | średni | wysoki | średni |
| **Dowód, że gorąca pętla działa (JevUse E2E: 4 kroki, 3.9 s, PASS)** | ✅ istnieje | ➖ do przeniesienia | ➖ | ➖ |

### Co NIE decyduje (uczciwie)

- **Nie surowa latencja wywołań.** Różnica Python vs natywny na pojedynczej akcji to 0.1–1 ms przy kroku ~350–500 ms zdominowanym przez inferencję Jev (~250–300 ms) i RPC UIA. Gdyby chodziło tylko o ms, Python z JevUse wystarczyłby.
- **Nie UIA.** Provider a11y aplikacji docelowej kosztuje tyle samo z każdego języka; jedyna dźwignia to mniej wywołań (CacheRequest, zdarzenia), nie szybszy klient.

### Co decyduje

1. **Portable `.exe`** — Python onefile jest ciężki, wolny na zimno i podejrzany dla AV; dla osoby z niepełnosprawnością „kliknij i działa w 100 ms" ma znaczenie. Rust daje to natywnie.
2. **STT + UIA + HTTP w jednym procesie bez GIL** — przewidywalna latencja od końca wypowiedzi do akcji (~300 ms od ostatniego słowa jak w jev-voice-browser) wymaga równoległości bez rywalizacji; w Pythonie osiągalne, ale kruche.
3. **Zależności są** — każdy element stosu ma utrzymywany crate (tabela), a tam, gdzie crate jest niepewny (sherpa-rs), jest C API i `bindgen`.
4. **C++ nie daje nic ponad Rust** w tym zadaniu, a kosztuje więcej (HTTP/2+TLS, async, tray, audio — wszystko ręcznie lub przez ciężkie frameworki).

## Decyzja

- **Rust** (`x86_64-pc-windows-msvc`, MSVC 14.50 + Windows SDK 26100 obecne) dla całej binarki: percepcja (Win32 + UIA COM), executor (`SendInput`, PowerShell), klient Jev (h2, hedging), STT (silnik z ADR-002), UI (tray + globalny hotkey + minimalne okno statusu), ledger.
- **Python (`D:\code\JevUse`) pozostaje laboratorium**: benchmarki E1–E6, eksperymenty z pytaniami i progami, porównania PL/EN. Binarka Rust wystawia lokalny interfejs (nazwany potok, JSON) `--serve`, żeby te same skrypty pomiarowe mogły sterować rdzeniem Rust i porównać liczby 1:1.
- **C++ odrzucony** (ten sam sufit wydajności, wyższy koszt). **C#/.NET AOT odrzucony** (STT natywne + AOT + brak hostingu PS pod AOT = te same ograniczenia co Rust bez jego ekosystemu pod nasze klocki).
- Driver **Interception** (input poniżej Raw Input) — poza zakresem v1; `SendInput` ze scan-code'ami widzi każda aplikacja poza anti-cheatami.

## Pierwsze pomiary (2026-09-20, ta maszyna — `bench/R1-uia.md`, `bench/R2-jev.md`)

| Miara | Rust (`ultracuse.exe`) | Python (JevUse / PyInstaller) |
|---|---|---|
| Rozmiar binarki | **3.19 MB** (reqwest+rustls+tokio, LTO, strip) | ~26 MB onefile (literatura) |
| Zimny / ciepły start | **46 ms / 20–22 ms** | ~1.8 s onefile (literatura) |
| Skan UIA Notatnik / Chrome / Claude(Electron), p50 | 380 / 108 / 464 ms | 388 / 106 / 478 ms — **to samo** |
| Jev http p50, vendor, N=12/30/60 | **274 / 288 / 297 ms** | 304 / 292 / 320 ms |
| Ciepła komenda PowerShell | 0.6–19 ms (sesja 140 ms, pierwsza komenda ~500 ms JIT) | spawn per komenda 300–600 ms |

Werdykt bez upiększeń: UIA kosztuje tyle samo w obu językach (provider aplikacji); Rust wygrywa tam, gdzie miał wygrać — binarka, start, klient HTTP, brak GIL.

## Konsekwencje

- Wolniejszy start niż „dopisać do JevUse": tydzień na przeniesienie percepcji/executora/klienta, zanim pojawi się pierwsza liczba E2E z Rust. Kompensacja: JevUse daje gotowe kształty pytań, redukcję i zmierzone progi.
- Ryzyka crate'ów (zweryfikowane 2026-09-20 przez research): (a) gorąca ścieżka UIA na **surowym COM z `windows` 0.62** (`FindAllBuildCache` + `CacheRequest`, jak w referencji JevUse), `uiautomation` 0.25 tylko do zdarzeń/wygody; (b) STT: `sherpa-rs` porzucony → oficjalne bindingi z repo k2-fsa/sherpa-onnx albo `whisper-rs`; wybór silnika w ADR-002; (c) crate'y jedno-/dwuosobowe (`uiautomation`, `windows-capture`, `enigo`, `wasapi`) — pinować wersje, gotowość do vendorowania; (d) `windows` 0.100 to przyszła migracja łamiąca — pin 0.62.
- `SendInput`/UIA to mechanizmy systemowe: widzi je anti-cheat, blokuje bezpieczny pulpit (UAC); „jak urządzenie HID" oznacza scan-code'y i pełną sekwencję w jednym wywołaniu, nie niewidzialność. Driver Interception (kernel) dopiero gdy zajdzie realna potrzeba.
- CI: `cargo build --release` + `cargo clippy` + `cargo test` na Windows runnerze; artefakt = jeden `.exe` + katalog modeli STT.
- Podpisywanie kodu (certyfikat) potrzebne przed dystrybucją — nie blokuje developmentu.

## Odrzucone alternatywy — jednym zdaniem

- **Python + Rust DLL (PyO3)** — rozwiązuje latencję, nie rozwiązuje portable exe ani GIL w pętli audio.
- **Electron/Tauri + web UI** — ciężar i zbędna warstwa; UI to tray i kilka etykiet.
- **Node.js** (jak kofanlabs) — dwa runtime'y (Python+Node), brak przenośnej binarki.
