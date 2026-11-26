//! `git blame`: the last commit that touched each line of a committed file.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use gix::bstr::ByteSlice as _;

use super::repo::GitRepository;
use super::{GitError, GitResult};

/// Commit information attached to blamed lines.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlameInfo {
    /// Full hexadecimal commit id.
    pub commit_id: String,
    /// Author name.
    pub author: String,
    /// Author e-mail.
    pub email: String,
    /// Author time, in seconds since the Unix epoch.
    pub time_seconds: i64,
    /// Author UTC offset, in seconds.
    pub utc_offset_seconds: i32,
    /// First line of the commit message.
    pub summary: String,
}

impl BlameInfo {
    /// Returns the 7-character abbreviated commit id.
    #[must_use]
    pub fn short_id(&self) -> &str {
        self.commit_id.get(..7).unwrap_or(&self.commit_id)
    }

    /// Returns the author date in the author's time zone as `YYYY-MM-DD`.
    #[must_use]
    pub fn date_label(&self) -> String {
        let local = self.time_seconds + i64::from(self.utc_offset_seconds);
        let (year, month, day) = civil_from_days(local.div_euclid(86_400));
        format!("{year:04}-{month:02}-{day:02}")
    }

    /// Returns the inline annotation shown after the active line, e.g.
    /// `"Jane Doe, 2026-11-26 · a1b2c3d Fix parser"`.
    #[must_use]
    pub fn inline_label(&self) -> String {
        format!(
            "{}, {} · {} {}",
            self.author,
            self.date_label(),
            self.short_id(),
            self.summary
        )
    }
}

/// Converts days since 1970-01-01 to a proleptic Gregorian `(year, month, day)`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "month and day are bounded to 1..=31 by the algorithm"
)]
const fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

/// Blame of a whole file at `HEAD`, as consecutive line ranges sharing a commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileBlame {
    /// `(first_line, line_count, commit)` ranges in 0-based `HEAD` line coordinates.
    hunks: Vec<(usize, usize, Arc<BlameInfo>)>,
}

impl FileBlame {
    /// Returns the commit of 0-based `HEAD` line `line`.
    #[must_use]
    pub fn line(&self, line: usize) -> Option<&BlameInfo> {
        let idx = self
            .hunks
            .partition_point(|(start, len, _)| start + len <= line);
        self.hunks
            .get(idx)
            .filter(|(start, _, _)| *start <= line)
            .map(|(_, _, info)| info.as_ref())
    }

    /// Returns the number of blamed lines.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.hunks.last().map_or(0, |(start, len, _)| start + len)
    }

    /// Returns `true` when nothing is blamed (file not committed).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hunks.is_empty()
    }
}

impl GitRepository {
    /// Blames every line of `path` at `HEAD`. Returns an empty [`FileBlame`] when the
    /// branch is unborn or the file is not committed.
    ///
    /// # Errors
    /// Fails when the path is invalid or the history cannot be traversed.
    pub fn blame(&self, path: impl AsRef<Path>) -> GitResult<FileBlame> {
        let relative = self.relative_path(path)?;
        if self.head_contents(&relative)?.is_none() {
            return Ok(FileBlame::default());
        }
        let repo = self.local();
        let head_id = repo.head_id().map_err(GitError::gix)?;
        let outcome = repo
            .blame_file(
                relative.as_bytes().as_bstr(),
                head_id.detach(),
                gix::repository::blame_file::Options::default(),
            )
            .map_err(GitError::gix)?;

        let mut commits: HashMap<gix::ObjectId, Arc<BlameInfo>> = HashMap::new();
        let mut hunks = Vec::with_capacity(outcome.entries.len());
        for entry in &outcome.entries {
            let info = if let Some(info) = commits.get(&entry.commit_id) {
                Arc::clone(info)
            } else {
                let info = Arc::new(load_commit(&repo, entry.commit_id)?);
                commits.insert(entry.commit_id, Arc::clone(&info));
                info
            };
            let range = entry.range_in_blamed_file();
            hunks.push((range.start, range.len(), info));
        }
        hunks.sort_by_key(|(start, _, _)| *start);
        Ok(FileBlame { hunks })
    }

    /// Blames a single 0-based `HEAD` line of `path`.
    ///
    /// # Errors
    /// Fails when the path is invalid or the history cannot be traversed.
    pub fn blame_line(&self, path: impl AsRef<Path>, line: usize) -> GitResult<Option<BlameInfo>> {
        Ok(self.blame(path)?.line(line).cloned())
    }
}

fn load_commit(repo: &gix::Repository, id: gix::ObjectId) -> GitResult<BlameInfo> {
    let commit = repo.find_commit(id).map_err(GitError::gix)?;
    let author = commit.author().map_err(GitError::gix)?;
    let time = author.time().map_err(GitError::gix)?;
    let message = commit.message().map_err(GitError::gix)?;
    let summary = message.summary().to_str_lossy().into_owned();
    Ok(BlameInfo {
        commit_id: id.to_string(),
        author: author.name.to_str_lossy().into_owned(),
        email: author.email.to_str_lossy().into_owned(),
        time_seconds: time.seconds,
        utc_offset_seconds: time.offset,
        summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        let info = BlameInfo {
            commit_id: "0123456789abcdef".to_owned(),
            author: "Jane".to_owned(),
            email: "jane@example.com".to_owned(),
            time_seconds: 1_764_115_200,
            utc_offset_seconds: 3600,
            summary: "Fix parser".to_owned(),
        };
        assert_eq!(info.short_id(), "0123456");
        assert_eq!(info.date_label(), "2025-11-26");
        assert_eq!(info.inline_label(), "Jane, 2025-11-26 · 0123456 Fix parser");
    }
}
