//! SQLite-backed TTS audio cache.
//!
//! Stores synthesized audio as BLOBs keyed by SHA-256 of the synthesis
//! parameters.  Uses LRU eviction based on `last_used_at` when the
//! total cached audio size exceeds a configurable limit.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tracing::debug;

/// Parameters used to generate the cache key.
#[derive(Debug, Clone, Serialize)]
pub struct TtsCacheParams<'a> {
    pub text: &'a str,
    pub model: &'a str,
    pub speed: f64,
    pub style_id: Option<i64>,
    pub speaker_id: Option<i64>,
    pub pitch: Option<f64>,
}

/// A cached TTS entry returned on cache hit.
#[derive(Debug, Clone)]
pub struct CachedAudio {
    pub audio_data: Vec<u8>,
    pub audio_format: String,
    pub duration_ms: i64,
}

/// SQLite BLOB cache for TTS audio.
pub struct TtsCache {
    conn: Connection,
    /// Maximum total size of cached audio in bytes.
    max_total_bytes: u64,
}

impl TtsCache {
    /// Open (or create) a cache database at the given path.
    pub fn open(db_path: &str) -> Result<Self> {
        Self::open_with_limit(db_path, 500)
    }

    /// Open with a custom max total size in megabytes.
    pub fn open_with_limit(db_path: &str, max_total_size_mb: u64) -> Result<Self> {
        let conn = Connection::open(db_path).context("failed to open TTS cache database")?;
        let cache = Self {
            conn,
            max_total_bytes: max_total_size_mb * 1024 * 1024,
        };
        cache.init_schema()?;
        Ok(cache)
    }

    /// Create an in-memory cache (for testing).
    pub fn in_memory(max_total_size_mb: u64) -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let cache = Self {
            conn,
            max_total_bytes: max_total_size_mb * 1024 * 1024,
        };
        cache.init_schema()?;
        Ok(cache)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "
            CREATE TABLE IF NOT EXISTS tts_cache (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                cache_key TEXT NOT NULL UNIQUE,
                text TEXT NOT NULL,
                model TEXT NOT NULL,
                speed REAL NOT NULL DEFAULT 1.0,
                style_id INTEGER,
                speaker_id INTEGER,
                pitch REAL,
                audio_format TEXT NOT NULL DEFAULT 'raw',
                audio_data BLOB NOT NULL,
                duration_ms INTEGER,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                last_used_at TEXT NOT NULL DEFAULT (datetime('now')),
                use_count INTEGER NOT NULL DEFAULT 1,
                access_seq INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_tts_cache_key ON tts_cache(cache_key);
            CREATE INDEX IF NOT EXISTS idx_tts_cache_last_used ON tts_cache(last_used_at);
            ",
            )
            .context("failed to initialize TTS cache schema")?;
        Ok(())
    }

    /// Look up a cached entry by synthesis parameters.
    ///
    /// On hit, updates `last_used_at` and `use_count`.
    pub fn lookup(&self, params: &TtsCacheParams<'_>) -> Result<Option<CachedAudio>> {
        let key = generate_cache_key(params);

        let mut stmt = self.conn.prepare(
            "SELECT audio_data, audio_format, duration_ms FROM tts_cache WHERE cache_key = ?1",
        )?;

        let result = stmt.query_row(params![key], |row| {
            let audio_data: Vec<u8> = row.get(0)?;
            let audio_format: String = row.get(1)?;
            let duration_ms: i64 = row.get(2)?;
            Ok(CachedAudio {
                audio_data,
                audio_format,
                duration_ms,
            })
        });

        match result {
            Ok(entry) => {
                // Update access metadata.
                self.conn.execute(
                    "UPDATE tts_cache SET last_used_at = datetime('now'), use_count = use_count + 1, access_seq = (SELECT COALESCE(MAX(access_seq), 0) + 1 FROM tts_cache) WHERE cache_key = ?1",
                    params![key],
                )?;
                debug!(cache_key = %key, "TTS cache hit");
                Ok(Some(entry))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                debug!(cache_key = %key, "TTS cache miss");
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Insert a new entry into the cache.
    ///
    /// Runs LRU eviction afterwards if total size exceeds the limit.
    pub fn insert(
        &self,
        params: &TtsCacheParams<'_>,
        audio_format: &str,
        audio_data: &[u8],
        duration_ms: i64,
    ) -> Result<()> {
        let key = generate_cache_key(params);

        self.conn.execute(
            "INSERT OR REPLACE INTO tts_cache
             (cache_key, text, model, speed, style_id, speaker_id, pitch, audio_format, audio_data, duration_ms, access_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, (SELECT COALESCE(MAX(access_seq), 0) + 1 FROM tts_cache))",
            params![
                key,
                params.text,
                params.model,
                params.speed,
                params.style_id,
                params.speaker_id,
                params.pitch,
                audio_format,
                audio_data,
                duration_ms,
            ],
        )?;

        debug!(cache_key = %key, bytes = audio_data.len(), "TTS cache insert");

        self.evict_if_needed()?;
        Ok(())
    }

    /// Return the total size of cached audio data in bytes.
    pub fn total_size_bytes(&self) -> Result<u64> {
        let size: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(SUM(length(audio_data)), 0) FROM tts_cache",
                [],
                |row| row.get(0),
            )
            .context("failed to query total cache size")?;
        Ok(size as u64)
    }

    /// Return the number of cached entries.
    pub fn entry_count(&self) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM tts_cache", [], |row| row.get(0))?;
        Ok(count as u64)
    }

    /// Evict least-recently-used entries until total size is within the limit.
    fn evict_if_needed(&self) -> Result<()> {
        loop {
            let total = self.total_size_bytes()?;
            if total <= self.max_total_bytes {
                break;
            }

            // Delete the least-recently-used entry.
            let deleted = self.conn.execute(
                "DELETE FROM tts_cache WHERE id = (
                    SELECT id FROM tts_cache ORDER BY access_seq ASC LIMIT 1
                )",
                [],
            )?;

            if deleted == 0 {
                break; // safety: no rows left
            }

            debug!(
                total_bytes = total,
                limit_bytes = self.max_total_bytes,
                "TTS cache: evicted LRU entry"
            );
        }
        Ok(())
    }

    /// Remove all cached entries.
    pub fn clear(&self) -> Result<()> {
        self.conn.execute("DELETE FROM tts_cache", [])?;
        Ok(())
    }
}

/// Generate a SHA-256 cache key from synthesis parameters.
pub fn generate_cache_key(params: &TtsCacheParams<'_>) -> String {
    let canonical = serde_json::to_string(params).expect("cache key serialization should not fail");
    let hash = Sha256::digest(canonical.as_bytes());
    hex::encode(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_params(text: &str) -> TtsCacheParams<'_> {
        TtsCacheParams {
            text,
            model: "test-model",
            speed: 1.0,
            style_id: None,
            speaker_id: None,
            pitch: None,
        }
    }

    #[test]
    fn cache_key_deterministic() {
        let p = test_params("hello");
        let k1 = generate_cache_key(&p);
        let k2 = generate_cache_key(&p);
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 64); // SHA-256 hex = 64 chars
    }

    #[test]
    fn cache_key_differs_for_different_text() {
        let k1 = generate_cache_key(&test_params("hello"));
        let k2 = generate_cache_key(&test_params("world"));
        assert_ne!(k1, k2);
    }

    #[test]
    fn cache_key_differs_for_different_speed() {
        let p1 = TtsCacheParams {
            text: "hello",
            model: "m",
            speed: 1.0,
            style_id: None,
            speaker_id: None,
            pitch: None,
        };
        let p2 = TtsCacheParams {
            text: "hello",
            model: "m",
            speed: 1.5,
            style_id: None,
            speaker_id: None,
            pitch: None,
        };
        assert_ne!(generate_cache_key(&p1), generate_cache_key(&p2));
    }

    #[test]
    fn miss_then_hit() {
        let cache = TtsCache::in_memory(100).unwrap();
        let params = test_params("hello");

        // Miss.
        assert!(cache.lookup(&params).unwrap().is_none());

        // Insert.
        let audio = vec![1u8, 2, 3, 4, 5];
        cache.insert(&params, "raw", &audio, 100).unwrap();

        // Hit.
        let hit = cache.lookup(&params).unwrap().unwrap();
        assert_eq!(hit.audio_data, audio);
        assert_eq!(hit.audio_format, "raw");
        assert_eq!(hit.duration_ms, 100);
    }

    #[test]
    fn use_count_increments() {
        let cache = TtsCache::in_memory(100).unwrap();
        let params = test_params("count-test");
        let audio = vec![0u8; 10];

        cache.insert(&params, "raw", &audio, 50).unwrap();

        // Three lookups.
        for _ in 0..3 {
            cache.lookup(&params).unwrap().unwrap();
        }

        // Check use_count (1 initial + 3 lookups = 4).
        let key = generate_cache_key(&params);
        let count: i64 = cache
            .conn
            .query_row(
                "SELECT use_count FROM tts_cache WHERE cache_key = ?1",
                params![key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 4);
    }

    #[test]
    fn total_size_tracking() {
        let cache = TtsCache::in_memory(100).unwrap();

        assert_eq!(cache.total_size_bytes().unwrap(), 0);

        let audio = vec![0u8; 1000];
        cache
            .insert(&test_params("a"), "raw", &audio, 10)
            .unwrap();

        assert_eq!(cache.total_size_bytes().unwrap(), 1000);

        cache
            .insert(&test_params("b"), "raw", &audio, 10)
            .unwrap();
        assert_eq!(cache.total_size_bytes().unwrap(), 2000);
    }

    #[test]
    fn entry_count() {
        let cache = TtsCache::in_memory(100).unwrap();
        assert_eq!(cache.entry_count().unwrap(), 0);

        cache
            .insert(&test_params("a"), "raw", &[0; 10], 10)
            .unwrap();
        cache
            .insert(&test_params("b"), "raw", &[0; 10], 10)
            .unwrap();
        assert_eq!(cache.entry_count().unwrap(), 2);
    }

    #[test]
    fn lru_eviction() {
        // Max 1 MB = 1_048_576 bytes.  We'll use a tiny limit instead.
        // open_with_limit takes MB, so use in_memory with small byte limit.
        let conn = Connection::open_in_memory().unwrap();
        let cache = TtsCache {
            conn,
            max_total_bytes: 100, // 100 bytes limit
        };
        cache.init_schema().unwrap();

        // Insert entries that exceed the limit.
        let audio_50 = vec![0u8; 50];

        // Entry "a" (50 bytes).
        cache
            .insert(&test_params("a"), "raw", &audio_50, 10)
            .unwrap();
        assert_eq!(cache.entry_count().unwrap(), 1);

        // Entry "b" (50 bytes) — total = 100, at limit.
        cache
            .insert(&test_params("b"), "raw", &audio_50, 10)
            .unwrap();
        assert_eq!(cache.entry_count().unwrap(), 2);

        // Touch "a" to make it recently used.
        cache.lookup(&test_params("a")).unwrap();

        // Entry "c" (50 bytes) — total would be 150 > 100.
        // Should evict "b" (LRU).
        cache
            .insert(&test_params("c"), "raw", &audio_50, 10)
            .unwrap();

        assert!(cache.total_size_bytes().unwrap() <= 100);

        // "b" should be evicted, "a" and "c" remain.
        assert!(cache.lookup(&test_params("b")).unwrap().is_none());
        assert!(cache.lookup(&test_params("a")).unwrap().is_some());
        assert!(cache.lookup(&test_params("c")).unwrap().is_some());
    }

    #[test]
    fn clear_removes_all() {
        let cache = TtsCache::in_memory(100).unwrap();
        cache
            .insert(&test_params("a"), "raw", &[0; 10], 10)
            .unwrap();
        cache
            .insert(&test_params("b"), "raw", &[0; 10], 10)
            .unwrap();

        cache.clear().unwrap();
        assert_eq!(cache.entry_count().unwrap(), 0);
        assert_eq!(cache.total_size_bytes().unwrap(), 0);
    }

    #[test]
    fn insert_or_replace_same_key() {
        let cache = TtsCache::in_memory(100).unwrap();
        let params = test_params("replace-test");

        cache.insert(&params, "raw", &[1, 2, 3], 30).unwrap();
        cache.insert(&params, "raw", &[4, 5, 6, 7], 40).unwrap();

        assert_eq!(cache.entry_count().unwrap(), 1);
        let hit = cache.lookup(&params).unwrap().unwrap();
        assert_eq!(hit.audio_data, vec![4, 5, 6, 7]);
        assert_eq!(hit.duration_ms, 40);
    }

    #[test]
    fn style_and_speaker_in_key() {
        let p1 = TtsCacheParams {
            text: "hello",
            model: "m",
            speed: 1.0,
            style_id: Some(1),
            speaker_id: Some(10),
            pitch: Some(0.5),
        };
        let p2 = TtsCacheParams {
            text: "hello",
            model: "m",
            speed: 1.0,
            style_id: Some(2),
            speaker_id: Some(10),
            pitch: Some(0.5),
        };
        assert_ne!(generate_cache_key(&p1), generate_cache_key(&p2));
    }
}
