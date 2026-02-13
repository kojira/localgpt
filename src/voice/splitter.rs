//! Sentence splitter for LLM streaming output.
//!
//! Accumulates streaming tokens and splits on sentence-ending punctuation
//! (`。！？!?`) and paragraph breaks (`\n\n`).  Each emitted sentence carries
//! a sequence index for downstream ordered playback.

use anyhow::Result;
use futures::{Stream, StreamExt};
use std::pin::Pin;
use tokio::sync::mpsc;

/// Punctuation characters that trigger a sentence split.
const SENTENCE_DELIMITERS: &[char] = &['。', '！', '？', '!', '?'];

/// Default minimum character length before a split is emitted.
const DEFAULT_MIN_LENGTH: usize = 2;

/// A sentence segment with its sequence index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentenceSegment {
    /// Zero-based sequence number.
    pub index: usize,
    /// The sentence text (trimmed, non-empty).
    pub text: String,
}

/// Configurable sentence splitter.
pub struct SentenceSplitter {
    /// Minimum character count for a segment to be emitted.
    /// Segments shorter than this are held until the next delimiter.
    pub min_length: usize,
}

impl Default for SentenceSplitter {
    fn default() -> Self {
        Self {
            min_length: DEFAULT_MIN_LENGTH,
        }
    }
}

impl SentenceSplitter {
    pub fn new(min_length: usize) -> Self {
        Self { min_length }
    }

    /// Convert a token stream into a sentence-segmented stream.
    ///
    /// Tokens are accumulated in an internal buffer.  When a sentence
    /// delimiter is detected **and** the accumulated text meets
    /// `min_length`, the segment is emitted with a monotonic index.
    /// Any remaining buffer content is flushed when the input stream ends.
    pub fn split(
        &self,
        token_stream: Pin<Box<dyn Stream<Item = Result<String>> + Send>>,
    ) -> Pin<Box<dyn Stream<Item = Result<SentenceSegment>> + Send>> {
        let min_len = self.min_length;
        let (tx, rx) = mpsc::channel::<Result<SentenceSegment>>(32);

        tokio::spawn(async move {
            let mut buffer = String::new();
            let mut stream = token_stream;
            let mut seq: usize = 0;

            while let Some(token_result) = stream.next().await {
                let token = match token_result {
                    Ok(t) => t,
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                };

                buffer.push_str(&token);

                // Split on punctuation delimiters.
                loop {
                    let split_pos = buffer.char_indices().find_map(|(i, c)| {
                        if SENTENCE_DELIMITERS.contains(&c) {
                            Some(i + c.len_utf8())
                        } else {
                            None
                        }
                    });

                    match split_pos {
                        Some(pos) => {
                            let sentence: String = buffer.drain(..pos).collect();
                            let trimmed = sentence.trim().to_string();
                            if trimmed.len() >= min_len {
                                let seg = SentenceSegment {
                                    index: seq,
                                    text: trimmed,
                                };
                                seq += 1;
                                if tx.send(Ok(seg)).await.is_err() {
                                    return;
                                }
                            } else if !trimmed.is_empty() {
                                // Below min_length — keep in buffer for next round.
                                buffer.insert_str(0, &trimmed);
                                break;
                            }
                        }
                        None => break,
                    }
                }

                // Split on paragraph break "\n\n".
                while buffer.contains("\n\n") {
                    let parts: Vec<&str> = buffer.splitn(2, "\n\n").collect();
                    let sentence = parts[0].trim().to_string();
                    buffer = parts[1].to_string();
                    if sentence.len() >= min_len {
                        let seg = SentenceSegment {
                            index: seq,
                            text: sentence,
                        };
                        seq += 1;
                        if tx.send(Ok(seg)).await.is_err() {
                            return;
                        }
                    } else if !sentence.is_empty() {
                        // Prepend back — will merge with next content.
                        buffer.insert_str(0, &format!("{sentence} "));
                    }
                }
            }

            // Flush remaining buffer.
            let remaining = buffer.trim().to_string();
            if !remaining.is_empty() {
                let seg = SentenceSegment {
                    index: seq,
                    text: remaining,
                };
                let _ = tx.send(Ok(seg)).await;
            }
        });

        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
    }
}

/// Convenience wrapper: split a token stream using the default config.
pub fn split_into_sentences(
    token_stream: Pin<Box<dyn Stream<Item = Result<String>> + Send>>,
) -> Pin<Box<dyn Stream<Item = Result<SentenceSegment>> + Send>> {
    SentenceSplitter::default().split(token_stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    /// Helper: create a token stream from a slice of strings.
    fn tokens(texts: &[&str]) -> Pin<Box<dyn Stream<Item = Result<String>> + Send>> {
        let v: Vec<Result<String>> = texts.iter().map(|t| Ok(t.to_string())).collect();
        Box::pin(stream::iter(v))
    }

    /// Collect all segments from the output stream.
    async fn collect_segments(
        stream: Pin<Box<dyn Stream<Item = Result<SentenceSegment>> + Send>>,
    ) -> Vec<SentenceSegment> {
        stream
            .filter_map(|r| async { r.ok() })
            .collect::<Vec<_>>()
            .await
    }

    #[tokio::test]
    async fn split_on_japanese_punctuation() {
        let input = tokens(&["こんにちは。", "元気ですか？"]);
        let segs = collect_segments(split_into_sentences(input)).await;
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].index, 0);
        assert_eq!(segs[0].text, "こんにちは。");
        assert_eq!(segs[1].index, 1);
        assert_eq!(segs[1].text, "元気ですか？");
    }

    #[tokio::test]
    async fn split_on_ascii_punctuation() {
        let input = tokens(&["Hello! How are you? Fine."]);
        let segs = collect_segments(split_into_sentences(input)).await;
        // "Hello!" -> seg0, "How are you?" -> seg1, "Fine." left in buffer -> flushed
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].text, "Hello!");
        assert_eq!(segs[1].text, "How are you?");
        assert_eq!(segs[2].text, "Fine.");
    }

    #[tokio::test]
    async fn split_on_paragraph_break() {
        let input = tokens(&["First paragraph.\n\nSecond paragraph."]);
        let segs = collect_segments(split_into_sentences(input)).await;
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, "First paragraph.");
        assert_eq!(segs[1].text, "Second paragraph.");
    }

    #[tokio::test]
    async fn incremental_tokens() {
        // Simulate token-by-token streaming.
        let input = tokens(&["こん", "にちは", "。", "さよ", "うなら", "！"]);
        let segs = collect_segments(split_into_sentences(input)).await;
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, "こんにちは。");
        assert_eq!(segs[1].text, "さようなら！");
    }

    #[tokio::test]
    async fn flush_remaining() {
        let input = tokens(&["no punctuation here"]);
        let segs = collect_segments(split_into_sentences(input)).await;
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "no punctuation here");
        assert_eq!(segs[0].index, 0);
    }

    #[tokio::test]
    async fn empty_stream() {
        let input = tokens(&[]);
        let segs = collect_segments(split_into_sentences(input)).await;
        assert!(segs.is_empty());
    }

    #[tokio::test]
    async fn whitespace_only_is_discarded() {
        let input = tokens(&["   ", "  \n\n  ", "  "]);
        let segs = collect_segments(split_into_sentences(input)).await;
        assert!(segs.is_empty());
    }

    #[tokio::test]
    async fn min_length_filter() {
        let splitter = SentenceSplitter::new(5);
        let input = tokens(&["Hi! Hello world!"]);
        let segs = collect_segments(splitter.split(input)).await;
        // "Hi!" is below min_length(5), so it merges forward.
        // "Hi!Hello world!" → the first delimiter is at "!" of "Hi!"
        // Since "Hi!" < 5, it stays in buffer and merges with the rest.
        // Then "Hi!Hello world!" is emitted at "!" ending.
        assert!(!segs.is_empty());
        // The final segment should contain all the text.
        let all_text: String = segs.iter().map(|s| s.text.clone()).collect::<Vec<_>>().join("");
        assert!(all_text.contains("Hello world"));
    }

    #[tokio::test]
    async fn sequence_numbers_are_monotonic() {
        let input = tokens(&["一。二。三。四。"]);
        let segs = collect_segments(split_into_sentences(input)).await;
        for (i, seg) in segs.iter().enumerate() {
            assert_eq!(seg.index, i);
        }
    }

    #[tokio::test]
    async fn error_propagation() {
        let items: Vec<Result<String>> = vec![
            Ok("hello".to_string()),
            Err(anyhow::anyhow!("stream error")),
        ];
        let input: Pin<Box<dyn Stream<Item = Result<String>> + Send>> =
            Box::pin(stream::iter(items));
        let results: Vec<Result<SentenceSegment>> =
            split_into_sentences(input).collect::<Vec<_>>().await;
        // Should have exactly one error.
        assert!(results.iter().any(|r| r.is_err()));
    }

    #[tokio::test]
    async fn multiple_delimiters_in_one_token() {
        let input = tokens(&["A!B?C。D"]);
        let segs = collect_segments(split_into_sentences(input)).await;
        // "A!" -> seg0, "B?" -> seg1, "C。" -> seg2, "D" -> flushed as seg3
        assert_eq!(segs.len(), 4);
        assert_eq!(segs[0].text, "A!");
        assert_eq!(segs[1].text, "B?");
        assert_eq!(segs[2].text, "C。");
        assert_eq!(segs[3].text, "D");
    }
}
