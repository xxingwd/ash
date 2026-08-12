use std::path::PathBuf;

use ash_core::FileChange;
use similar::TextDiff;

pub(crate) fn added(path: PathBuf, content: String) -> FileChange {
    FileChange::Add { path, content }
}

pub(crate) fn updated(path: PathBuf, before: &str, after: &str) -> Option<FileChange> {
    if before == after {
        return None;
    }
    let unified_diff = TextDiff::from_lines(before, after)
        .unified_diff()
        .context_radius(3)
        .header("before", "after")
        .to_string();
    Some(FileChange::Update { path, unified_diff })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_contextual_unified_diff() {
        let change = updated(
            PathBuf::from("src/main.rs"),
            "one\ntwo\nthree\n",
            "one\nchanged\nthree\n",
        )
        .unwrap();
        let FileChange::Update { unified_diff, .. } = change else {
            panic!("expected update");
        };

        assert!(unified_diff.contains("-two"));
        assert!(unified_diff.contains("+changed"));
        assert!(unified_diff.contains(" three"));
    }

    #[test]
    fn omits_noop_updates() {
        assert!(updated(PathBuf::from("same.txt"), "same", "same").is_none());
    }
}
