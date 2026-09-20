# R4 (wstęp) — pierwsze przebiegi pętli MVP `ultracuse run`

Data: 2026-09-20 22:50–23:15, ta maszyna, Notatnik Windows 11 (z kartami), końcówka vendora `api.typesafe.ai`, model `jev-1.13.0`, klient `uc-jev` (h2, hedge-straż 600 ms). Surowe ledgery (kopie z `runs/`): `R4-mvp-type.jsonl`, `R4-mvp-close.jsonl` — jeden wiersz na krok (`signals`, `verdict`, czasy etapów) + wiersz podsumowania. To **nie** jest jeszcze pełne R4 (10 zadań × 3 runy, TASKS 1.7) — to dowód, że pętla domyka się na żywym oknie i ile to kosztuje.

| zadanie | kroki | czas | wywołania Jev | koszt | wynik |
|---|---:|---:|---:|---:|---|
| „Wpisz „hello ultracuse” w edytorze tekstu” — świeży Notatnik, `--act` | 2 | **1 036 ms** | 2 | $0.00022 | **Done** |
| „Zamknij kartę bez zapisywania zmian” — `--act --allow-irreversible` | 5 | **2 738 ms** | 5 (1 zawężenie) | $0.00048 | **TargetGone** (karta zamknięta bez zapisu, okno zniknęło) |

Krok po kroku:

- **Wpisz** — #1 scan 79 ms (23 el.), Jev 318 ms → `type` do `document „Edytor tekstów”` (target 0.60, op 0.97, needs_text 0.93); #2 tytuł `*hello ultracuse — Notatnik`, scan 63 ms, Jev 278 ms → goal 0.85 (0.83 / pending 0.12), op `done` 0.70 → **DONE**.
- **Zamknij** — #1 scan 101 ms, Jev 587 ms w dwóch wywołaniach: `choice` rozdarty między `button „Zamknij”` a `menuitem „Plik”`, potem bramki per finalista: Plik **0.86** → klik; #2 `menuitem „Zamknij kartę”` 0.97; #3 dialog, `button „Nie zapisuj”` 0.94 (is_destructive 0.50 → bez flagi byłoby BLOCKED); #4 okno w trakcie zamykania (0 el.) → uncertain; #5 `IsWindow` = false → **TargetGone**.

## Co się okazało (i co z tym zrobiono w tym samym PR)

1. **Bez `document` (50030) w stanie** Notatnik nie ma pola, do którego można pisać — `target none`, model zgaduje „focused control”. Dodane do `INTERACTIVE_IDS` (`uc-uia`).
2. **Blokada na proces okna docelowego jest obowiązkowa.** W pierwszym przebiegu (przed blokadą) fokus przeszedł do Chrome i pętla wpisała tekst 4× w obce okno (YouTube). Teraz: inny pid na wierzchu → `FocusLost` bez akcji; okno docelowe znika → `TargetGone`. Po przeglądzie PR #1: fokus i `IsWindow` sprawdzane **ponownie tuż przed wstrzyknięciem**, bo między skanem a akcją mija 0.3–2 s (skan + 1–2 wywołania Jev).
3. **Pojedynczy noul o cel jest słaby** (0.34 tuż po udanym wpisaniu). Dwa sformułowania w tym samym wywołaniu, uśrednione (`goal_reached`, `1 − goal_pending`) → 0.85.
4. **Dwie poprawne drogi rozdzierają `choice` na stałe.** „Zamknij” (przycisk) vs „Plik” (menu): 0.44–0.80 vs 0.18–0.39 zależnie od przebiegu; zawężony wybór 2-way = rzut monetą (0.53/0.47, H ≈ 1.0); **niezależne bramki noul per finalista** („czy e1 to poprawny krok?”) rozstrzygają: Plik 0.86. Reguła 2 z docs Jev („dekomponuj na atomowe sygnały") w praktyce.
5. **`confidence` dla `choice` ≈ margines top-2**, nie p(top): przy {0.59, 0.41} API zwraca 0.18. Progi na `confidence` i na `gap` to prawie to samo — kalibracja (R6) ma to uwzględnić.
6. **Wartość dokumentu (RichEdit) nie jest widoczna** przez `ValuePattern` — model widzi tylko tytuł karty `hello ultr. Zmodyfikowany.`. Odczyt `TextPattern` → TASKS 1.8.
7. **Próg `done`**: to samo zadanie („Wpisz…”) dało `goal_reached` 0.85 i 0.84 (0.83/0.12 i 0.81/0.13) w dwóch przebiegach — przy progu 0.85 drugi skończył się `Uncertain` po 3 krokach zamiast `Done` po 2. Fałszywe „done” kosztuje powtórkę, nie dane, więc `DONE_THRESHOLD` = 0.75; 0.85 zostaje progiem dla nieodwracalnych.
8. **Koszt kroku dziś**: scan 63–203 ms (Notatnik), Jev 265–318 ms na wywołanie, settle = stały sleep 200 ms → ~0.5–0.6 s/krok. Settle po zdarzeniach UIA (1.1) i cache makr (1.5) to następne −200…−300 ms.
