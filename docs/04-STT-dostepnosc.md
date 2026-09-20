# ADR-002: Rozpoznawanie mowy (STT) w czasie rzeczywistym — dostępność po polsku

Status: **przyjęty** 2026-09-20 z zadaniami weryfikacyjnymi (R3). Research: 3 równoległe agenty (silniki lokalne, nowe modele 2026 + API Windows, chmura + crate'y Rust), stan na wrzesień 2026. Wymagania: polski jako pierwszy język, partiale w trakcie mówienia, finał ≤ 300 ms po końcu wypowiedzi, odporność na szum, praca w jednym przenośnym `.exe`, praca offline.

## 1. Co jest dostępne (fakty)

### Lokalnie / offline

| Silnik | Polski | Streaming | Latencja | Runtime / licencja | Rozmiar | Werdykt |
|---|---|---|---|---|---|---|
| **whisper.cpp** (`large-v3-turbo`, `distil-whisper-large-v3-pl`) | tak (WER PL bez czystej publikacji) | pseudo: re-dekodowanie bufora; VAD Silero wbudowany (`--vad`) | brak zweryfikowanej liczby na Windows/x86 | C/C++, MIT; `whisper-rs` 0.16 (2026-03, CUDA/Vulkan) | turbo ≈ 1.5 GB f16, q5 ≈ 0.55 GB | **offline domyślny** |
| **NVIDIA Nemotron-3.5-ASR-Streaming-0.6B** (08.2026) | **tak, pl-PL 15.15 % WER @ 1.12 s chunk** | **tak, natywny cache-aware streaming**, chunk 80–1120 ms | architektonicznie < 300 ms przy małym chunku | NeMo-Speech.cpp / ONNX; licencja **OpenMDW-1.1 (do weryfikacji)** | 0.6B | **tor eksperymentalny** — jedyny prawdziwy streaming PL; brak gotowego pakowania |
| Parakeet-TDT-0.6B-v3 | tak (25 jęz., auto-LID) | chunked | brak | NeMo, CC-BY-4.0 | 0.6B | jak wyżej, drugi kandydat |
| sherpa-onnx (k2-fsa) | **brak streamingowego modelu PL** (Zipformer streaming: zh/en/ko/bn; offline CTC multilingual ma `pl`; integracja Canary dekoduje PL jako EN — issue #3961) | tak (dla obsługiwanych) | RTF 0.15–0.46 | C++/ONNX, Apache-2.0; oficjalny crate `sherpa-onnx` 1.13.8 (2026-09-11), `sherpa-rs` zarchiwizowany | — | **obserwować** — najlepszy runtime, brak modelu PL |
| Vosk `vosk-model-small-pl-0.22` | tak, **WER 11.5–18.4 %** | tak, natywny | ~natychmiast | Kaldi, Apache-2.0; `vosk-rs` 0.3.1 (2024) | 50 MB | za słaby do dyktowania; ewentualnie tylko komendy |
| faster-whisper + Silero | jak Whisper | nie (batch 4 s) | — | CTranslate2 + **Python** | — | odpada (Python w exe) |
| Qwen3-ASR, Moonshine, Kyutai STT, Voxtral, Canary-1B-v2, Granite | Qwen3 tak, reszta **bez PL** lub batch | Kyutai ma wbudowane 0.5–2.5 s opóźnienia | — | PyTorch/vLLM | — | odpadają |
| Windows `Windows.Media.SpeechRecognition`, `Microsoft.Windows.AI.Speech` | **brak PL** (Voice Access potwierdzone bez PL); nowe API EN-only preview | frazy, nie tokeny | — | WinRT, **wymaga tożsamości pakietu MSIX** | — | **odpada** dla niepakowanego `.exe` |

### Chmura (WebSocket)

| Usługa | Polski | Partiale | Latencja (deklarowana) | Cena | Klient Rust |
|---|---|---|---|---|---|
| **ElevenLabs Scribe v2 Realtime** | tak, tier „excellent" (≤ 5 % WER) | tak | ~100–150 ms (strony vendora niespójne) | $0.39/h PAYG; **Pro już opłacone** (klucz `JAV_11` / `ELEVENLABS_API_KEY` w rejestrze; JevUse używa Scribe v2 batch) | brak oficjalnego SDK; `tokio-tungstenite` + własny protokół; `elevenlabs_rs` 0.7.1 nieoficjalny |
| **Soniox** | tak, najlepiej udokumentowany PL | **tak, słowo po słowie bez czekania na koniec wypowiedzi** | < 200 ms | ~$0.12/h (najtaniej) | WS, brak crate'a |
| **Deepgram Nova-3** | tak (`pl` monojęzyczny) | tak | 200–300 ms | ~$0.46/h | **oficjalnie linkowany crate `deepgram` 0.11.0 (2026-09-14)** |
| Speechmatics / Azure / Google Chirp 3 | tak | tak | „kilkaset ms" / brak / brak | $0.40/h / ~$1/h / $0.016/min | brak sprawdzenia |
| OpenAI realtime, Gladia | PL niepotwierdzony | tak | brak / < 300 ms | — | — |
| AssemblyAI Universal-Streaming | **brak PL w streamingu** | — | — | — | odpada |

## 2. Decyzja

Warstwowo, z jednym interfejsem `SttEngine` (`crates/uc-voice`), żeby silniki były wymienne bez dotykania pętli:

1. **VAD i bramkowanie zawsze lokalnie** (Silero VAD z whisper.cpp — bez onnxruntime; alternatywnie `ort` + Silero, gdy ONNX i tak będzie w procesie). Bez mowy nic nie jest wysyłane ani dekodowane. Push-to-talk (globalny hotkey) domyślnie; tryb ciągły z VAD dla osób, które nie trafiają w klawisz; słowo-klucz (KWS) w v2.
2. **Online, domyślnie: ElevenLabs Scribe v2 Realtime** — istniejąca subskrypcja, polski w najwyższym tierze, partiale. Własny klient WebSocket (`tokio-tungstenite`), PCM 16 kHz. **Do zmierzenia (R3):** realna latencja z Polski i zużycie kredytów Pro (docs vendora są niespójne).
3. **Offline, zawsze dostępne: whisper.cpp przez `whisper-rs`** — `large-v3-turbo` q5 na GPU (Vulkan/CUDA; ta maszyna: RTX 2080 Ti) lub `small`/`distil-pl` na CPU; pseudo-streaming przez re-dekodowanie 1–1.5 s bufora po VAD; finał po ciszy 300 ms. Modele w `models/` obok exe (pobierane przy pierwszym uruchomieniu z weryfikacją SHA-256), nie w binarce.
4. **Tor eksperymentalny: Nemotron-3.5-ASR-Streaming-0.6B** — jedyny model z prawdziwym streamingiem PL; eksport ONNX + `ort` lub NeMo-Speech.cpp; jeśli licencja OpenMDW-1.1 pozwala na redystrybucję i chunk 160–320 ms daje < 300 ms na GPU, **zastępuje whisper jako offline domyślny**.
5. **Zapasowa chmura: Soniox** (najtańsza, streaming bez EOS) lub **Deepgram** (najlepszy crate Rust) — włączane konfiguracją, bez zmian w pętli.

Odrzucone: API Windows (brak PL, MSIX), AssemblyAI (brak PL), Vosk do dyktowania (WER), faster-whisper (Python), sherpa-onnx do czasu pojawienia się modelu PL streaming (wtedy priorytet — najlepszy runtime).

## 3. Jak STT wchodzi w pętlę decyzyjną

```
audio 16 kHz → VAD → [lokalny whisper: partiale na ekranie natychmiast]
                    → [cloud WS równolegle: partiale + finał wyższej jakości]
      partial (debounce 200 ms) → Jev fan-out: is_command · intent · target · complete · destructive
      complete ≥ 0.7 ∧ conf ≥ próg → akcja bez czekania na finał
      finał chmury zastępuje lokalny draft (tryb dyktowania: tekst do pola przez SetValue / schowek)
      offline → lokalny finał obowiązuje sam; funkcjonalność pełna, latencja gorsza
```

Budżet „ostatnie słowo → akcja": chmura ~150–300 ms + Jev ~300 ms + akcja < 5 ms ≈ **0.45–0.6 s**; offline GPU ≈ 0.6–0.8 s; offline CPU 1–3 s (tryb zdegradowany, komunikowany użytkownikowi).

Dostępność: partiale widoczne natychmiast (informacja zwrotna sama w sobie jest wymaganiem); wolna mowa → dłuższa cisza końcowa (konfigurowalna 300–800 ms); komendy krótkie, zamknięte („klik", „zapisz", „potwierdź", „anuluj", „dyktuj", „koniec", „stop") rozpoznawane lokalnie także bez sieci; każde potwierdzenie akcji nieodwracalnej — głosem lub klawiszem; TTS krótkich potwierdzeń (ElevenLabs lub Windows SAPI offline).

Prywatność: audio opuszcza maszynę **tylko** w trybie online, z widocznym wskaźnikiem; tryb offline jednym przełącznikiem; żadne audio nie jest zapisywane domyślnie.

## 4. Do zmierzenia (R3) — przed zamknięciem decyzji

| # | Pomiar | Kryterium |
|---|---|---|
| R3.1 | ElevenLabs Realtime z Polski: partial p50, finał po EOS p50/p95, 50 wypowiedzi PL (komendy + dyktowanie 10–20 s) | finał ≤ 300 ms p50 → domyślny online |
| R3.2 | whisper.cpp turbo q5 na RTX 2080 Ti i na CPU i9: finał po EOS przy re-dekodowaniu 1 s / 1.5 s bufora; WER na 50 zdaniach PL | GPU ≤ 500 ms; WER ≤ 8 % → offline domyślny |
| R3.3 | Nemotron streaming (ONNX) chunk 160/320 ms: latencja + WER PL | < 300 ms i WER ≤ 12 % → zastępuje whisper offline |
| R3.4 | Soniox / Deepgram jako porównanie na tym samym zestawie | rezerwa |
| R3.5 | Zużycie kredytów ElevenLabs Pro na 1 h realtime | koszt godziny pracy użytkownika |

## Źródła

sherpa-onnx modele: <https://k2-fsa.github.io/sherpa/onnx/pretrained_models> · issue PL→EN: <https://github.com/k2-fsa/sherpa-onnx/issues/3961> · whisper.cpp: <https://github.com/ggml-org/whisper.cpp> · Vosk: <https://alphacephei.com/vosk/models> · Nemotron-3.5-ASR-Streaming: <https://huggingface.co/nvidia/nemotron-3.5-asr-streaming-0.6b> · Windows Speech (MSIX): <https://learn.microsoft.com/windows/ai/apis/speech-recognition> · Voice Access bez PL: Microsoft Support „voice access FAQs" · ElevenLabs realtime: <https://elevenlabs.io/docs/api-reference/speech-to-text/v-1-speech-to-text-realtime> · Deepgram: <https://developers.deepgram.com/docs/models-languages-overview>, <https://crates.io/crates/deepgram> · AssemblyAI (brak PL): <https://assemblyai.com/docs/faq/language-support-for-real-time-transcription> · Soniox PL: <https://soniox.com/speech-to-text/polish>
