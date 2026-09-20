# R2 — round-trip Jev z Rusta (klient `uc-jev`)

Data: 2026-09-20 21:48, łącze domowe z Polski, końcówka vendora `https://api.typesafe.ai/v1/systemone`, model `jev-1.13.0` (klucz `JEV_API_KEY`). Komenda: `ultracuse bench-jev --runs 5 --sizes 12,30,60 --provider typesafe` (+ `--hedge-ms 400`). Stan syntetyczny: drzewo UI N elementów + `goal` + `last`; 5 pytań w jednym POST: `target` (choice N+`none`), `op` (choice 6), `goal_reached`, `needs_text`, `is_destructive` (noul). Klient: reqwest/rustls, HTTP/2, jedno ciepłe połączenie, rozgrzewka mini-decyzją. Surowe: `R2-jev-rust.jsonl`. Koszt całego pomiaru: 22 wywołania ≈ $0.003.

| N | tokeny | koszt/wyw. | **http p50** | min | p95 | `target` | conf | Hn | `op` | `goal_reached` | `needs_text` | `is_destructive` |
|---:|---:|---:|---:|---:|---:|---|---:|---:|---|---:|---:|---:|
| 12 | 1 469 | $0.000062 | **274.5 ms** | 265 | 394 | e11 „Save" 0.99 | 0.98 | 0.02 | click | 0.04 | 0.08 | 0.24 |
| 30 | 2 613 | $0.000110 | **287.9 ms** | 258 | 417 | e11 0.98 | 0.97 | 0.03 | click | 0.04 | 0.10 | 0.21 |
| 60 | 4 701 | $0.000197 | **297.5 ms** | 275 | 324 | e11 0.88 | 0.87 | 0.12 | click | 0.04 | 0.09 | 0.19 |
| 30, hedge 400 ms | 2 613 | $0.000110 (+1 hedge/5) | 310.6 ms | 275 | 335 | e11 0.98 | 0.97 | 0.03 | click | 0.05 | 0.09 | 0.21 |

Rozgrzewka (zimne połączenie, mini-decyzja ~320 tok.): 708–754 ms. `doctor` po rozgrzewce: 357 ms dla jednego `noul`.

Porównanie z laboratorium Python tego samego dnia (`jevskill/bench/cu_bench.py`, ta sama końcówka, 4 pytania): N=12 **304 ms**, N=30 **292 ms**, N=60 **320 ms** → Rust −10…−30 ms na p50 (serializacja + brak GIL; reszta to sieć i inferencja).

## Odczyt

1. **Floor ≈ 260–275 ms** z Polski na końcówce vendora; latencja płaska względem N (12 → 60 elementów: +23 ms). Inferencja + sieć dominują; klient jest już poza równaniem.
2. **Wybór celu stabilny do N=60** (0.88), entropia rośnie 0.02 → 0.12 — sygnał do kaskady region→element powyżej ~60.
3. **`is_destructive` 0.19–0.24 dla „Save"** — model nie jest pewny, że zapis jest bezpieczny; potwierdza regułę „Jev doradza, lista w kodzie decyduje".
4. **Hedging przy 400 ms nie wygrał ani razu** (ogon p95 tu 324–417 ms). Próg hedgingu trzeba ustawić z rozkładu (≈ p90, dziś ~330 ms), nie na sztywno; koszt 1 dodatkowe wywołanie na 5.
5. OpenRouter **niezmierzony z Rusta**: klucz `JEVUSE_API_KEY` wygasł (401 „API key expired"). Wczorajszy pomiar Python: +33…+63 ms względem vendora.

## Do zrobienia

- `--hedge-ms` z percentyla bieżącego okna (adaptacyjny), pomiar p95 hedged vs plain na 50 wywołaniach.
- N=120, 240 (limit jev-browser) i kryteria PL vs EN (R5).
- OpenRouter po odświeżeniu klucza.
