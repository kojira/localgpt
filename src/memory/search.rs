//! Memory search types and utilities

use serde::{Deserialize, Serialize};

/// A chunk of memory content returned from search
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryChunk {
    /// File path relative to workspace
    pub file: String,

    /// Starting line number (1-indexed)
    pub line_start: i32,

    /// Ending line number (1-indexed)
    pub line_end: i32,

    /// The actual content
    pub content: String,

    /// Relevance score (higher is better)
    pub score: f64,
}

impl MemoryChunk {
    /// Create a new memory chunk
    pub fn new(file: String, line_start: i32, line_end: i32, content: String, score: f64) -> Self {
        Self {
            file,
            line_start,
            line_end,
            content,
            score,
        }
    }

    /// Get a preview of the content (first N characters)
    pub fn preview(&self, max_len: usize) -> String {
        if self.content.len() <= max_len {
            self.content.clone()
        } else {
            format!("{}...", &self.content[..max_len])
        }
    }

    /// Get the location string (file:line)
    pub fn location(&self) -> String {
        if self.line_start == self.line_end {
            format!("{}:{}", self.file, self.line_start)
        } else {
            format!("{}:{}-{}", self.file, self.line_start, self.line_end)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_chunk_preview() {
        let chunk = MemoryChunk::new(
            "test.md".to_string(),
            1,
            5,
            "This is a long content string that should be truncated".to_string(),
            0.9,
        );

        assert_eq!(chunk.preview(20), "This is a long conte...");
        assert_eq!(chunk.location(), "test.md:1-5");
    }

    #[test]
    fn test_memory_chunk_single_line_location() {
        let chunk = MemoryChunk::new(
            "test.md".to_string(),
            10,
            10,
            "Single line".to_string(),
            0.5,
        );

        assert_eq!(chunk.location(), "test.md:10");
    }
}
