//! Voice input for accessibility. The engine is chosen in ADR-002; this crate fixes
//! the interface the loop depends on so engines are swappable.

use serde::Serialize;

/// A transcript event from a streaming recogniser.
#[derive(Clone, Debug, Serialize)]
pub enum Transcript {
    /// Interim hypothesis; may change. Emitted every 100–200 ms while speaking.
    Partial { text: String, t_ms: u64 },
    /// Final text for an utterance, after end-of-speech silence.
    Final {
        text: String,
        t_ms: u64,
        audio_ms: u64,
    },
}

/// Streaming speech-to-text engine. Implementations run on their own thread and push
/// [`Transcript`]s into a channel owned by the loop.
pub trait SttEngine: Send {
    fn name(&self) -> &'static str;
    fn language(&self) -> &str;
    /// Feed 16 kHz mono f32 samples.
    fn push_audio(&mut self, samples: &[f32]);
    /// Drain events produced since the last call.
    fn poll(&mut self) -> Vec<Transcript>;
}
