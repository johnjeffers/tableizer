//! Folder completion for the start screen's jump-to field: as the user types a local path or a remote
//! URL, suggest the subfolders of the folder typed so far (the text up to its last separator) whose
//! names start with the partial name after it.
//!
//! These are the pure pieces (splitting, matching, accepting, highlight movement, and the local
//! listing); the app lists folders off the UI thread and renders the suggestion popup.

use std::path::Path;

/// Split typed text at its last separator into the folder to list (keeping its trailing separator)
/// and the partial name being typed: `/Users/jo` → (`/Users/`, `jo`). `None` until a separator is
/// typed — there is no folder to list yet.
pub(crate) fn split(text: &str, is_separator: fn(char) -> bool) -> Option<(&str, &str)> {
    let at = text.rfind(is_separator)? + 1;
    Some(text.split_at(at))
}

/// The folder names in `dirs` that complete `partial`, in their listed order: a case-insensitive
/// prefix match. Hidden (dot) folders are offered only once the partial itself starts with a dot.
pub(crate) fn matches<'a>(dirs: &'a [String], partial: &str) -> Vec<&'a str> {
    let partial = partial.to_lowercase();
    dirs.iter()
        .map(String::as_str)
        .filter(|name| partial.starts_with('.') || !name.starts_with('.'))
        .filter(|name| name.to_lowercase().starts_with(&partial))
        .collect()
}

/// The field's text after accepting folder `name` in `parent`: the folder's path with a trailing
/// separator (the one `parent` ends in), so the next level's suggestions follow straight away.
pub(crate) fn accept(parent: &str, name: &str) -> String {
    let separator = parent.chars().last().unwrap_or('/');
    format!("{parent}{name}{separator}")
}

/// Move the highlighted suggestion one step (`down`, or up) in a list of `len`. Down from nothing
/// highlights the first; up from the first returns to nothing highlighted (the typed text). Stops at
/// the last entry rather than wrapping.
pub(crate) fn move_highlight(current: Option<usize>, len: usize, down: bool) -> Option<usize> {
    match (current, down) {
        (_, true) if len == 0 => None,
        (None, true) => Some(0),
        (Some(i), true) => Some((i + 1).min(len - 1)),
        (Some(0) | None, false) => None,
        (Some(i), false) => Some(i - 1),
    }
}

/// Where a remote folder's subfolder names come from, if it can be listed at all.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RemoteSource {
    /// `s3://` itself: the buckets reachable with the configured credentials.
    Buckets,
    /// A bucket or a prefix within one: list it.
    Prefix,
}

/// Classify a remote folder (`parent` from [`split`]) for listing. A bare scheme other than S3 can't
/// enumerate its buckets, and a half-typed scheme (`s3:/`) isn't a folder, so neither completes.
pub(crate) fn remote_source(parent: &str) -> Option<RemoteSource> {
    if matches!(parent, "s3://" | "s3a://") {
        return Some(RemoteSource::Buckets);
    }
    let has_bucket = parent
        .split_once("://")
        .is_some_and(|(_, rest)| !rest.is_empty() && !rest.starts_with('/'));
    (tableizer_core::remote::is_remote(parent) && has_bucket).then_some(RemoteSource::Prefix)
}

/// The names of the subfolders of local directory `path`, sorted. Symlinks to folders count as
/// folders, and hidden ones are included (the jump-to field is how the user reaches folders the tree
/// hides); [`matches`] decides whether to offer them.
pub(crate) fn list_local_subdirs(path: &Path) -> Result<Vec<String>, String> {
    let mut dirs: Vec<String> = std::fs::read_dir(path)
        .map_err(|e| e.to_string())?
        .flatten()
        .filter(|entry| {
            // `file_type` doesn't follow symlinks; only a symlink pays for a second `stat`.
            entry
                .file_type()
                .is_ok_and(|t| t.is_dir() || (t.is_symlink() && entry.path().is_dir()))
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    dirs.sort();
    Ok(dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unix_sep(c: char) -> bool {
        c == '/'
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn split_separates_the_folder_from_the_partial_name() {
        assert_eq!(split("/Users/jo", unix_sep), Some(("/Users/", "jo")));
        assert_eq!(split("/Users/", unix_sep), Some(("/Users/", "")));
        assert_eq!(split("/", unix_sep), Some(("/", "")));
        assert_eq!(
            split("s3://bucket/da", unix_sep),
            Some(("s3://bucket/", "da"))
        );
        assert_eq!(split("s3://bu", unix_sep), Some(("s3://", "bu")));
    }

    #[test]
    fn split_needs_a_separator() {
        assert_eq!(split("", unix_sep), None);
        assert_eq!(split("Users", unix_sep), None);
    }

    #[test]
    fn split_honours_the_given_separators() {
        let windows_sep = |c: char| c == '/' || c == '\\';
        assert_eq!(
            split(r"C:\Users\jo", windows_sep),
            Some((r"C:\Users\", "jo"))
        );
    }

    #[test]
    fn matches_is_a_case_insensitive_prefix_match_in_listed_order() {
        let dirs = names(&["Desktop", "Documents", "Downloads", "Music", "docs"]);
        assert_eq!(matches(&dirs, "do"), ["Documents", "Downloads", "docs"]);
        assert_eq!(matches(&dirs, "Mu"), ["Music"]);
        assert_eq!(matches(&dirs, "x"), Vec::<&str>::new());
    }

    #[test]
    fn matches_offers_everything_for_an_empty_partial_except_hidden_folders() {
        let dirs = names(&[".config", "Desktop", "Music"]);
        assert_eq!(matches(&dirs, ""), ["Desktop", "Music"]);
    }

    #[test]
    fn matches_offers_hidden_folders_once_a_dot_is_typed() {
        let dirs = names(&[".cache", ".config", "Desktop"]);
        assert_eq!(matches(&dirs, "."), [".cache", ".config"]);
        assert_eq!(matches(&dirs, ".co"), [".config"]);
    }

    #[test]
    fn accept_appends_the_folder_and_the_parents_separator() {
        assert_eq!(accept("/Users/", "john"), "/Users/john/");
        assert_eq!(accept("/", "Users"), "/Users/");
        assert_eq!(accept("s3://", "bucket"), "s3://bucket/");
        assert_eq!(accept(r"C:\Users\", "john"), r"C:\Users\john\");
    }

    #[test]
    fn move_highlight_steps_through_the_list_without_wrapping() {
        assert_eq!(move_highlight(None, 3, true), Some(0));
        assert_eq!(move_highlight(Some(0), 3, true), Some(1));
        assert_eq!(move_highlight(Some(2), 3, true), Some(2));
        assert_eq!(move_highlight(Some(1), 3, false), Some(0));
        assert_eq!(move_highlight(Some(0), 3, false), None);
        assert_eq!(move_highlight(None, 3, false), None);
    }

    #[test]
    fn move_highlight_does_nothing_in_an_empty_list() {
        assert_eq!(move_highlight(None, 0, true), None);
    }

    #[test]
    fn remote_source_lists_s3_buckets_at_the_bare_scheme() {
        assert_eq!(remote_source("s3://"), Some(RemoteSource::Buckets));
        assert_eq!(remote_source("s3a://"), Some(RemoteSource::Buckets));
    }

    #[test]
    fn remote_source_lists_a_bucket_or_prefix() {
        assert_eq!(remote_source("s3://bucket/"), Some(RemoteSource::Prefix));
        assert_eq!(
            remote_source("s3://bucket/a/b/"),
            Some(RemoteSource::Prefix)
        );
        assert_eq!(remote_source("gs://bucket/a/"), Some(RemoteSource::Prefix));
    }

    #[test]
    fn remote_source_skips_what_cannot_be_listed() {
        assert_eq!(remote_source("gs://"), None); // no bucket discovery outside S3
        assert_eq!(remote_source("s3:/"), None); // scheme still being typed
        assert_eq!(remote_source("/Users/"), None);
    }

    #[test]
    fn list_local_subdirs_lists_folders_only_sorted_and_including_hidden() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["beta", "alpha", ".hidden"] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        std::fs::write(dir.path().join("file.csv"), "a,b\n").unwrap();
        assert_eq!(
            list_local_subdirs(dir.path()).unwrap(),
            [".hidden", "alpha", "beta"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn list_local_subdirs_follows_symlinks_to_folders() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
        assert_eq!(list_local_subdirs(dir.path()).unwrap(), ["link", "real"]);
    }

    #[test]
    fn list_local_subdirs_reports_an_unreadable_folder() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_local_subdirs(&dir.path().join("missing")).is_err());
    }
}
