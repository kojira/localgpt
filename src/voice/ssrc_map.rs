//! SSRC → Discord UserID mapping.
//!
//! Tracks the association between RTP SSRCs (assigned per voice connection)
//! and Discord user identities. Updated via songbird Speaking events.

use dashmap::DashMap;

/// SSRC → Discord UserID bidirectional mapping.
pub struct SsrcUserMap {
    /// SSRC → (user_id, username)
    ssrc_to_user: DashMap<u32, (u64, String)>,
    /// user_id → SSRC (reverse lookup)
    user_to_ssrc: DashMap<u64, u32>,
}

impl SsrcUserMap {
    pub fn new() -> Self {
        Self {
            ssrc_to_user: DashMap::new(),
            user_to_ssrc: DashMap::new(),
        }
    }

    /// Update mapping from a songbird SpeakingUpdate event.
    pub fn update_from_speaking(&self, ssrc: u32, user_id: u64, username: String) {
        // If this user already had a different SSRC, remove old entry
        if let Some((_, old_ssrc)) = self.user_to_ssrc.remove(&user_id) {
            if old_ssrc != ssrc {
                self.ssrc_to_user.remove(&old_ssrc);
            }
        }
        self.ssrc_to_user.insert(ssrc, (user_id, username));
        self.user_to_ssrc.insert(user_id, ssrc);
    }

    /// Look up user by SSRC. Returns `(user_id, username)`.
    pub fn get_user(&self, ssrc: u32) -> Option<(u64, String)> {
        self.ssrc_to_user.get(&ssrc).map(|r| r.value().clone())
    }

    /// Remove a user (e.g. on voice-channel leave).
    pub fn remove_user(&self, user_id: u64) {
        if let Some((_, ssrc)) = self.user_to_ssrc.remove(&user_id) {
            self.ssrc_to_user.remove(&ssrc);
        }
    }

    /// Number of users currently tracked.
    pub fn active_count(&self) -> usize {
        self.ssrc_to_user.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_lookup() {
        let map = SsrcUserMap::new();
        map.update_from_speaking(1001, 100, "Alice".into());

        let user = map.get_user(1001);
        assert_eq!(user, Some((100, "Alice".into())));
        assert_eq!(map.active_count(), 1);
    }

    #[test]
    fn remove_user_clears_both_directions() {
        let map = SsrcUserMap::new();
        map.update_from_speaking(1001, 100, "Alice".into());
        map.remove_user(100);

        assert!(map.get_user(1001).is_none());
        assert_eq!(map.active_count(), 0);
    }

    #[test]
    fn remove_nonexistent_user_is_noop() {
        let map = SsrcUserMap::new();
        map.remove_user(999); // should not panic
        assert_eq!(map.active_count(), 0);
    }

    #[test]
    fn ssrc_change_replaces_old_mapping() {
        let map = SsrcUserMap::new();
        map.update_from_speaking(1001, 100, "Alice".into());
        // Same user, new SSRC (e.g. reconnect)
        map.update_from_speaking(2002, 100, "Alice".into());

        assert!(map.get_user(1001).is_none(), "old SSRC should be gone");
        assert_eq!(map.get_user(2002), Some((100, "Alice".into())));
        assert_eq!(map.active_count(), 1);
    }

    #[test]
    fn multiple_users() {
        let map = SsrcUserMap::new();
        map.update_from_speaking(1001, 100, "Alice".into());
        map.update_from_speaking(1002, 200, "Bob".into());
        map.update_from_speaking(1003, 300, "Charlie".into());

        assert_eq!(map.active_count(), 3);
        assert_eq!(map.get_user(1002), Some((200, "Bob".into())));

        map.remove_user(200);
        assert_eq!(map.active_count(), 2);
        assert!(map.get_user(1002).is_none());
    }

    #[test]
    fn update_same_ssrc_same_user() {
        let map = SsrcUserMap::new();
        map.update_from_speaking(1001, 100, "Alice".into());
        map.update_from_speaking(1001, 100, "Alice (updated)".into());

        assert_eq!(map.active_count(), 1);
        assert_eq!(map.get_user(1001), Some((100, "Alice (updated)".into())));
    }
}
