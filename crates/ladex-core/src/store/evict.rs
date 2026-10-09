//! Which cached files to evict, least recently used first, to make room.

pub struct Candidate {
    pub id: String,
    pub size: u64,
    pub last_access_ms: u64,
}

/// The oldest-accessed candidates whose removal brings `used` down to at most `target`; none if it is already there.
pub fn pick_victims(mut candidates: Vec<Candidate>, used: u64, target: u64) -> Vec<String> {
    candidates.sort_by_key(|candidate| candidate.last_access_ms);
    let mut remaining = used;
    let mut victims = Vec::new();
    for candidate in candidates {
        if remaining <= target {
            break;
        }
        remaining = remaining.saturating_sub(candidate.size);
        victims.push(candidate.id);
    }
    victims
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, size: u64, last_access_ms: u64) -> Candidate {
        Candidate { id: id.into(), size, last_access_ms }
    }

    #[test]
    fn nothing_is_evicted_while_under_the_target() {
        assert!(pick_victims(vec![candidate("a", 10, 1)], 50, 50).is_empty());
    }

    #[test]
    fn the_least_recently_used_go_first() {
        let candidates = vec![candidate("new", 10, 30), candidate("old", 10, 10), candidate("mid", 10, 20)];
        assert_eq!(pick_victims(candidates, 100, 85), ["old", "mid"]);
    }

    #[test]
    fn eviction_stops_as_soon_as_enough_is_freed() {
        let candidates = vec![candidate("a", 50, 1), candidate("b", 50, 2)];
        assert_eq!(pick_victims(candidates, 100, 60), ["a"]);
    }

    #[test]
    fn everything_is_offered_when_it_still_is_not_enough() {
        let candidates = vec![candidate("a", 10, 1), candidate("b", 10, 2)];
        assert_eq!(pick_victims(candidates, 100, 0), ["a", "b"]);
    }
}
