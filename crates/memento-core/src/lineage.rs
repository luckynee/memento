use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};

pub const MAX_LINEAGE_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionKey {
    pub harness: String,
    pub session: String,
}

impl fmt::Display for SessionKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.harness, self.session)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLink {
    pub key: SessionKey,
    pub parent: Option<SessionKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineageError {
    Cycle { session: SessionKey },
    TooDeep { limit: usize },
}

impl fmt::Display for LineageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cycle { session } => write!(formatter, "lineage cycle at session {session}"),
            Self::TooDeep { limit } => write!(formatter, "lineage exceeds depth limit {limit}"),
        }
    }
}

impl Error for LineageError {}

pub fn ancestors(
    start: &SessionKey,
    parent_of: impl Fn(&SessionKey) -> Option<SessionKey>,
) -> Result<Vec<SessionKey>, LineageError> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    let mut current = Some(start.clone());
    while let Some(session) = current {
        if !seen.insert(session.clone()) {
            return Err(LineageError::Cycle { session });
        }
        if result.len() == MAX_LINEAGE_DEPTH {
            return Err(LineageError::TooDeep {
                limit: MAX_LINEAGE_DEPTH,
            });
        }
        current = parent_of(&session);
        result.push(session);
    }
    Ok(result)
}

pub fn descendants(start: &SessionKey, links: &[SessionLink]) -> Vec<SessionKey> {
    let mut children: BTreeMap<&SessionKey, BTreeSet<&SessionKey>> = BTreeMap::new();
    for link in links {
        if let Some(parent) = &link.parent {
            children.entry(parent).or_default().insert(&link.key);
        }
    }
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([start]);
    let mut result = Vec::new();
    seen.insert(start);
    while let Some(session) = queue.pop_front() {
        result.push(session.clone());
        if let Some(siblings) = children.get(session) {
            for child in siblings {
                if seen.insert(*child) {
                    queue.push_back(child);
                }
            }
        }
    }
    result
}

pub fn would_create_cycle(
    child: &SessionKey,
    parent: &SessionKey,
    parent_of: impl Fn(&SessionKey) -> Option<SessionKey>,
) -> bool {
    let mut seen = HashSet::new();
    let mut current = Some(parent.clone());
    while let Some(session) = current {
        if &session == child {
            return true;
        }
        if !seen.insert(session.clone()) {
            return false;
        }
        current = parent_of(&session);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(harness: &str, session: &str) -> SessionKey {
        SessionKey {
            harness: harness.into(),
            session: session.into(),
        }
    }

    #[test]
    fn chain_and_cycle_across_harnesses() {
        let parent =
            |session: &SessionKey| match (session.harness.as_str(), session.session.as_str()) {
                ("claude", "same") => Some(key("opencode", "same")),
                ("opencode", "same") => Some(key("codex", "root")),
                _ => None,
            };
        let start = key("claude", "same");
        assert_eq!(
            ancestors(&start, parent).unwrap(),
            [start.clone(), key("opencode", "same"), key("codex", "root")]
        );
        assert_eq!(start.to_string(), "claude/same");
        assert_eq!(
            ancestors(&start, |_| Some(start.clone())),
            Err(LineageError::Cycle { session: start })
        );
    }

    #[test]
    fn depth_limit() {
        let parent = |session: &SessionKey| {
            let number: usize = session.session.parse().unwrap();
            (number > 0).then(|| key("h", &(number - 1).to_string()))
        };
        assert_eq!(ancestors(&key("h", "63"), parent).unwrap().len(), 64);
        assert_eq!(
            ancestors(&key("h", "64"), parent),
            Err(LineageError::TooDeep { limit: 64 })
        );
    }

    #[test]
    fn descendants_are_breadth_first_sorted_and_cycle_safe() {
        let links = [
            (key("b", "same"), key("a", "root")),
            (key("a", "same"), key("a", "root")),
            (key("c", "child"), key("a", "same")),
            (key("a", "root"), key("c", "child")),
        ]
        .into_iter()
        .rev()
        .map(|(key, parent)| SessionLink {
            key,
            parent: Some(parent),
        })
        .collect::<Vec<_>>();
        assert_eq!(
            descendants(&key("a", "root"), &links),
            [
                key("a", "root"),
                key("a", "same"),
                key("b", "same"),
                key("c", "child")
            ]
        );
    }

    #[test]
    fn cycle_prediction_handles_cross_harnesses_existing_cycles_and_long_chains() {
        let parent =
            |session: &SessionKey| match (session.harness.as_str(), session.session.as_str()) {
                ("b", "s") => Some(key("a", "s")),
                ("c", "s") => Some(key("b", "s")),
                _ => None,
            };
        assert!(would_create_cycle(&key("a", "s"), &key("c", "s"), parent));
        assert!(would_create_cycle(&key("a", "s"), &key("a", "s"), parent));
        assert!(!would_create_cycle(&key("c", "s"), &key("a", "s"), parent));
        assert!(!would_create_cycle(&key("z", "s"), &key("a", "s"), |_| {
            Some(key("a", "s"))
        }));
        assert!(would_create_cycle(
            &key("h", "0"),
            &key("h", "70"),
            |session| {
                let number: usize = session.session.parse().unwrap();
                (number > 0).then(|| key("h", &(number - 1).to_string()))
            }
        ));
    }
}
