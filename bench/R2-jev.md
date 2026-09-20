# R2 — round-trip Jev z Rusta (klient `uc-jev`)

## Seria A — vendor, 21:48

Data: 2026-09-20 21:48, łącze domowe z Polski, końcówka vendora `https://api.typesafe.ai/v1/systemone`, model `jev-1.13.0` (klucz `JEV_API_KEY`). Komenda: `ultracuse bench-jev --runs 5 --sizes 12,30,60 --provider typesafe` (+ `--hedge-ms 400`). Stan syntetyczny: drzewo UI N elementów + `goal` + `last`; 5 pytań w jednym POST: `target` (choice N+`none`), `op` (choice 6), `goal_reached`, `needs_text`, `is_destructive` (noul). Klient: reqwest/rustls, HTTP/2, jedno ciepłe połączenie, rozgrzewka mini-decyzją. Surowe: `R2-jev-rust.jsonl`. Koszt całego pomiaru: 22 wywołania ≈ $0.003.

| N | tokeny | koszt/wyw. | **http p50** | min | p95 | `target` | conf | Hn | `op` | `goal_reached` | `needs_text` | `is_destructive` |
|---:|---:|---:|---:|---:|---:|---|---:|---:|---|---:|---:|---:|
| 12 | 1 469 | $0.000062 | **274.5 ms** | 265 | 394 | e11 „Save" 0.99 | 0.98 | 0.02 | click | 0.04 | 0.08 | 0.24 |
| 30 | 2 613 | $0.000110 | **287.9 ms** | 258 | 417 | e11 0.98 | 0.97 | 0.03 | click | 0.04 | 0.10 | 0.21 |
| 60 | 4 701 | $0.000197 | **297.5 ms** | 275 | 324 | e11 0.88 | 0.87 | 0.12 | click | 0.04 | 0.09 | 0.19 |
| 30, hedge 400 ms | 2 613 | $0.000110 (+1 hedge/5) | 310.6 ms | 275 | 335 | e11 0.98 | 0.97 | 0.03 | click | 0.05 | 0.09 | 0.21 |

Rozgrzewka (zimne połączenie, mini-decyzja ~320 tok.): 708–754 ms. `doctor` po rozgrzewce: 357 ms dla jednego `noul`.

Porównanie z laboratorium Python tego samego dnia (`jevskill/bench/cu_bench.py`, ta sama końcówka, 4 pytania): N=12 **304 ms**, N=30 **292 ms**, N=60 **320 ms** → Rust −10…−30 ms na p50 (serializacja + brak GIL; reszta to sieć i inferencja).

## Seria B — vendor vs OpenRouter w tym samym oknie, 22:02–22:07

Komenda: `ultracuse bench-jev --runs 10 --sizes 12,30,60,120 --provider {typesafe|openrouter}` (+ `--hedge-ms 350` dla N=30), oba providery jeden po drugim w ciągu 5 minut, ten sam klient, to samo łącze. OpenRouter: klucz `OPEN_ROUTER_API_KEY` (odświeżony), model w odpowiedzi `typesafe/jev-1.13-20260917`, `usage.cost` raportowany (344 tok → $0.0000144 = dokładnie $0.042/Mtok). Vendor: `jev-1.13.0`. Łącznie ~120 wywołań ≈ $0.02.

| N | tokeny | **vendor p50** | min | p95 | **OpenRouter p50** | min | p95 | Δ p50 | Δ p95 | `target` (oba) | Hn |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|
| 12 | 1 469 | **302.3 ms** | 267 | 338 | **293.7 ms** | 267 | 334 | −9 | −4 | e11 0.99 | 0.02 |
| 30 | 2 613 | **286.8 ms** | 258 | 319 | **313.5 ms** | 280 | 373 | +27 | +54 | e11 0.97 | 0.04 |
| 60 | 4 701 | **285.6 ms** | 270 | 340 | **322.7 ms** | 308 | 425 | +37 | +85 | e11 0.88 / 0.90 | 0.13 / 0.11 |
| 120 | 8 958 | **327.8 ms** | 291 | 366 | **366.8 ms** | 327 | 402 | +39 | +36 | e11 0.98 | 0.02 |
| 30, hedge 350 ms | 2 613 | 302.4 ms (3 hedge'e, 0 wygranych) | 268 | 351 | 318.5 ms (2 hedge'e, 0 wygranych) | 276 | 334 | +16 | −17 | e11 0.98 | 0.03 |
| 30, kontrola vendora 22:07 | 2 613 | 309.5 ms | 261 | 354 | — | | | | | e11 0.98 | 0.03 |

Rozgrzewka zimnego połączenia mini-decyzją: vendor 662–764 ms, OpenRouter 364–658 ms. Pozostałe pytania identyczne na obu końcówkach (`op` click, `goal_reached` 0.04–0.05, `needs_text` 0.09–0.10, `is_destructive` 0.17–0.27) — to ten sam model za dwoma proxy.

## Odczyt

1. **Floor ≈ 260–275 ms** z Polski na końcówce vendora; latencja płaska względem N (12 → 60 elementów: +23 ms). Inferencja + sieć dominują; klient jest już poza równaniem.
2. **Wybór celu stabilny do N=60** (0.88), entropia rośnie 0.02 → 0.12 — sygnał do kaskady region→element powyżej ~60.
3. **`is_destructive` 0.19–0.24 dla „Save"** — model nie jest pewny, że zapis jest bezpieczny; potwierdza regułę „Jev doradza, lista w kodzie decyduje".
4. **Hedging nie wygrał w 0/8 przypadkach** (próg 400 ms w serii A, 350 ms w serii B). Arytmetyka: hedge wystrzelony po 350 ms kończy się najwcześniej po 350 + floor 267 ≈ 620 ms, więc wygrywa tylko przy zawieszeniu pierwszego żądania > 620 ms — a maksimum w 110 wywołaniach to 425 ms. Wniosek: próg z p90 (~330 ms) tylko podwaja koszt; hedge ma być **strażą przed zawieszeniem** (≈ 2×p50 = 600 ms, `uc-loop::consts::HEDGE_AFTER_MS`), a docelowo adaptacyjny z p99 okna.
5. **OpenRouter = ten sam floor (267 ms), ale +27…+39 ms na p50 od N=30 wzwyż i cięższy ogon** (p95 +54…+85 ms przy N=30/60). Narzut rośnie z ładunkiem — proxy przesyła ciało dalej. Przy N=12 różnica w szumie (−9 ms). Vendor zostaje domyślny; OpenRouter opcjonalny (`--provider openrouter` / `UC_PROVIDER=openrouter`) — daje `usage.cost` i `session_id`, kosztuje jeden hop.
6. **Wybór celu nie jest monotoniczny względem N** (N=60: 0.88, N=120: 0.98) — o pewności decydują dystraktory w drzewie (przy N=60 syntetyczny „e41" jest bliski celowi), nie sama liczba kandydatów. Próg kaskady ustawiać na entropii, nie na N.

## Do zrobienia

- Hedge adaptacyjny: straż 600 ms teraz, próg z p99 okna później; test z wymuszonym zawieszeniem (proxy z opóźnieniem), 50 wywołań hedged vs plain (TASKS 0.6).
- N=240 (limit jev-browser) i kryteria PL vs EN (R5).
