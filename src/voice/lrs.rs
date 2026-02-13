//! Least-Recently-Spoken (LRS) eviction logic.
//!
//! When the number of concurrent STT sessions exceeds the configured
//! maximum, the user who has been silent the longest is evicted to make
//! room for a new speaker.

use dashmap::DashMap;
use std::time::Instant;

/// Per-user activity record.
#[derive(Debug, Clone)]
pub struct UserActivity {
    pub user_id: u64,
    pub ssrc: u32,
    pub last_spoken_at: Instant,
}

/// Tracks last-spoken timestamps and provides LRS eviction queries.
pub struct LrsTracker {
    activities: DashMap<u32, UserActivity>,
}

impl LrsTracker {
    pub fn new() -> Self {
        Self {
            activities: DashMap::new(),
        }
    }

    /// Register or refresh a user's last-spoken timestamp.
    pub fn update_activity(&self, ssrc: u32, user_id: u64) {
        self.activities.insert(
            ssrc,
            UserActivity {
                user_id,
                ssrc,
                last_spoken_at: Instant::now(),
            },
        );
    }

    /// Find the SSRC of the user who has been silent the longest.
    pub fn find_least_recently_spoken(&self) -> Option<u32> {
        self.activities
            .iter()
            .min_by_key(|entry| entry.last_spoken_at)
            .map(|entry| entry.ssrc)
    }

    /// Remove a user from tracking (after eviction or disconnect).
    pub fn remove(&self, ssrc: u32) {
        self.activities.remove(&ssrc);
    }

    /// Number of tracked users.
    pub fn len(&self) -> usize {
        self.activities.len()
    }

    /// Whether the tracker is empty.
    pub fn is_empty(&self) -> bool {
        self.activities.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn empty_tracker_returns_none() {
        let tracker = LrsTracker::new();
        assert!(tracker.find_least_recently_spoken().is_none());
        assert!(tracker.is_empty());
    }

    #[test]
    fn single_user() {
        let tracker = LrsTracker::new();
        tracker.update_activity(1001, 100);

        assert_eq!(tracker.len(), 1);
        assert_eq!(tracker.find_least_recently_spoken(), Some(1001));
    }

    #[test]
    fn least_recently_spoken_is_oldest() {
        let tracker = LrsTracker::new();

        tracker.update_activity(1001, 100);
        // Small sleep so timestamps differ
        thread::sleep(Duration::from_millis(10));
        tracker.update_activity(1002, 200);
        thread::sleep(Duration::from_millis(10));
        tracker.update_activity(1003, 300);

        // User 100 (SSRC 1001) spoke first → least recently spoken
        assert_eq!(tracker.find_least_recently_spoken(), Some(1001));
    }

    #[test]
    fn update_refreshes_timestamp() {
        let tracker = LrsTracker::new();

        tracker.update_activity(1001, 100);
        thread::sleep(Duration::from_millis(10));
        tracker.update_activity(1002, 200);
        thread::sleep(Duration::from_millis(10));

        // User 100 speaks again → refreshed
        tracker.update_activity(1001, 100);

        // Now user 200 (SSRC 1002) is oldest
        assert_eq!(tracker.find_least_recently_spoken(), Some(1002));
    }

    #[test]
    fn remove_clears_entry() {
        let tracker = LrsTracker::new();
        tracker.update_activity(1001, 100);
        tracker.update_activity(1002, 200);

        tracker.remove(1001);
        assert_eq!(tracker.len(), 1);
        assert_eq!(tracker.find_least_recently_spoken(), Some(1002));
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let tracker = LrsTracker::new();
        tracker.update_activity(1001, 100);
        tracker.remove(9999);
        assert_eq!(tracker.len(), 1);
    }

    #[test]
    fn eviction_scenario() {
        // Simulate 4 active + 1 new → evict LRS
        let tracker = LrsTracker::new();

        tracker.update_activity(1001, 100);
        thread::sleep(Duration::from_millis(5));
        tracker.update_activity(1002, 200);
        thread::sleep(Duration::from_millis(5));
        tracker.update_activity(1003, 300);
        thread::sleep(Duration::from_millis(5));
        tracker.update_activity(1004, 400);

        assert_eq!(tracker.len(), 4);

        // Evict LRS to make room
        let lrs = tracker.find_least_recently_spoken().unwrap();
        assert_eq!(lrs, 1001);
        tracker.remove(lrs);

        // Add new user
        tracker.update_activity(1005, 500);
        assert_eq!(tracker.len(), 4);

        // User 200 is now LRS
        assert_eq!(tracker.find_least_recently_spoken(), Some(1002));
    }
}
