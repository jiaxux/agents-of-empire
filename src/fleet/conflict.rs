use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use super::SessionKey;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileConflict {
    pub path: PathBuf,
    pub sessions: Vec<SessionKey>,
}

/// Reverse index of repository-relative paths to the sessions changing them.
#[derive(Debug, Default)]
pub struct ConflictRadar {
    by_path: HashMap<PathBuf, BTreeSet<SessionKey>>,
    by_session: HashMap<SessionKey, BTreeSet<PathBuf>>,
}

impl ConflictRadar {
    pub fn replace_paths<I, P>(&mut self, session: SessionKey, paths: I)
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        let replacement: BTreeSet<_> = paths.into_iter().map(Into::into).collect();
        let previous = self.by_session.remove(&session).unwrap_or_default();

        for path in previous.difference(&replacement) {
            let became_empty = self
                .by_path
                .get_mut(path)
                .map(|owners| {
                    owners.remove(&session);
                    owners.is_empty()
                })
                .unwrap_or(false);
            if became_empty {
                self.by_path.remove(path);
            }
        }

        for path in replacement.difference(&previous) {
            self.by_path
                .entry(path.clone())
                .or_default()
                .insert(session.clone());
        }

        if !replacement.is_empty() {
            self.by_session.insert(session, replacement);
        }
    }

    pub fn remove_session(&mut self, session: &SessionKey) {
        self.replace_paths(session.clone(), std::iter::empty::<PathBuf>());
    }

    pub fn conflicts(&self) -> Vec<FileConflict> {
        let mut conflicts: Vec<_> = self
            .by_path
            .iter()
            .filter(|(_, sessions)| sessions.len() > 1)
            .map(|(path, sessions)| FileConflict {
                path: path.clone(),
                sessions: sessions.iter().cloned().collect(),
            })
            .collect();
        conflicts.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        conflicts
    }

    pub fn conflicts_for(&self, session: &SessionKey) -> Vec<FileConflict> {
        self.conflicts()
            .into_iter()
            .filter(|conflict| conflict.sessions.contains(session))
            .collect()
    }

    pub fn overlap_count(&self, left: &SessionKey, right: &SessionKey) -> usize {
        match (self.by_session.get(left), self.by_session.get(right)) {
            (Some(left), Some(right)) => left.intersection(right).count(),
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_only_changed_path_ownership() {
        let mut radar = ConflictRadar::default();
        let alpha = SessionKey::from("alpha");
        let beta = SessionKey::from("beta");
        radar.replace_paths(alpha.clone(), ["src/a.rs", "src/shared.rs"]);
        radar.replace_paths(beta.clone(), ["src/b.rs", "src/shared.rs"]);

        assert_eq!(radar.overlap_count(&alpha, &beta), 1);
        assert_eq!(radar.conflicts()[0].path, PathBuf::from("src/shared.rs"));

        radar.replace_paths(beta.clone(), ["src/b.rs"]);
        assert!(radar.conflicts().is_empty());
        radar.remove_session(&beta);
        assert_eq!(radar.overlap_count(&alpha, &beta), 0);
    }
}
