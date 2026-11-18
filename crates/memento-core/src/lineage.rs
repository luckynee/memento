use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::error::Error;
use std::fmt;

pub const MAX_LINEAGE_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLink {
    pub session: String,
    pub parent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineageError {
    Cycle { session: String },
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
    start: &str,
    parent_of: impl Fn(&str) -> Option<String>,
) -> Result<Vec<String>, LineageError> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    let mut current = Some(start.to_owned());
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

pub fn descendants(start: &str, links: &[SessionLink]) -> Vec<String> {
    let mut children: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for link in links {
        if let Some(parent) = &link.parent {
            children.entry(parent).or_default().insert(&link.session);
        }
    }
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([start]);
    let mut result = Vec::new();
    seen.insert(start);
    while let Some(session) = queue.pop_front() {
        result.push(session.to_owned());
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
    child: &str,
    parent: &str,
    parent_of: impl Fn(&str) -> Option<String>,
) -> bool {
    let mut seen = HashSet::new();
    let mut current = Some(parent.to_owned());
    while let Some(session) = current {
        if session == child {
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

    #[test]
    fn chain_and_cycle() {
        let parent = |session: &str| match session {
            "c" => Some("b".into()),
            "b" => Some("a".into()),
            _ => None,
        };
        assert_eq!(ancestors("c", parent).unwrap(), ["c", "b", "a"]);
        assert_eq!(
            ancestors("a", |_| Some("a".into())),
            Err(LineageError::Cycle {
                session: "a".into()
            })
        );
    }

    #[test]
    fn depth_limit() {
        let parent = |session: &str| {
            let number: usize = session.parse().unwrap();
            (number > 0).then(|| (number - 1).to_string())
        };
        assert_eq!(ancestors("63", parent).unwrap().len(), 64);
        assert_eq!(
            ancestors("64", parent),
            Err(LineageError::TooDeep { limit: 64 })
        );
    }

    #[test]
    fn descendants_are_breadth_first_sorted_and_cycle_safe() {
        let links = [("b", "a"), ("c", "a"), ("d", "b"), ("a", "d"), ("e", "b")]
            .into_iter()
            .rev()
            .map(|(session, parent)| SessionLink {
                session: session.into(),
                parent: Some(parent.into()),
            })
            .collect::<Vec<_>>();
        assert_eq!(descendants("a", &links), ["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn cycle_prediction_handles_existing_cycles_and_long_chains() {
        let parent = |session: &str| match session {
            "b" => Some("a".into()),
            "c" => Some("b".into()),
            _ => None,
        };
        assert!(would_create_cycle("a", "c", parent));
        assert!(would_create_cycle("a", "a", parent));
        assert!(!would_create_cycle("c", "a", parent));
        assert!(!would_create_cycle("z", "a", |_| Some("a".into())));
        assert!(would_create_cycle("0", "70", |session| {
            let number: usize = session.parse().unwrap();
            (number > 0).then(|| (number - 1).to_string())
        }));
    }
}
