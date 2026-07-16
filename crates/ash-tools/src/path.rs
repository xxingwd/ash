use std::path::{Component, Path, PathBuf};

use ash_core::ToolError;

pub(crate) fn existing(root: &Path, requested: &str) -> Result<PathBuf, ToolError> {
    let root = canonical_root(root)?;
    let candidate = lexical_path(&root, requested)?;
    let resolved = std::fs::canonicalize(&candidate).map_err(|error| {
        ToolError::Execution(format!("cannot access {}: {error}", candidate.display()))
    })?;
    ensure_inside(&root, resolved)
}

pub(crate) fn for_write(root: &Path, requested: &str) -> Result<PathBuf, ToolError> {
    let root = canonical_root(root)?;
    let candidate = lexical_path(&root, requested)?;
    match std::fs::symlink_metadata(&candidate) {
        Ok(_) => {
            let resolved = std::fs::canonicalize(&candidate).map_err(|error| {
                ToolError::Execution(format!("cannot access {}: {error}", candidate.display()))
            })?;
            ensure_inside(&root, resolved)?;
            return Ok(candidate);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(ToolError::Execution(format!(
                "cannot access {}: {error}",
                candidate.display()
            )));
        }
    }
    let mut ancestor = candidate.parent().unwrap_or(&root);
    while !ancestor.exists() {
        ancestor = ancestor.parent().ok_or_else(|| {
            ToolError::Execution(format!("path escapes working directory: {requested}"))
        })?;
    }
    let resolved_ancestor = std::fs::canonicalize(ancestor).map_err(|error| {
        ToolError::Execution(format!("cannot access {}: {error}", ancestor.display()))
    })?;
    ensure_inside(&root, resolved_ancestor)?;
    Ok(candidate)
}

fn canonical_root(root: &Path) -> Result<PathBuf, ToolError> {
    std::fs::canonicalize(root).map_err(|error| {
        ToolError::Execution(format!(
            "cannot access working directory {}: {error}",
            root.display()
        ))
    })
}

fn lexical_path(root: &Path, requested: &str) -> Result<PathBuf, ToolError> {
    let requested = Path::new(requested);
    let relative = if requested.is_absolute() {
        requested.strip_prefix(root).map_err(|_| {
            ToolError::Execution(format!(
                "path is outside working directory: {}",
                requested.display()
            ))
        })?
    } else {
        requested
    };

    let mut clean = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !clean.pop() {
                    return Err(ToolError::Execution(format!(
                        "path escapes working directory: {}",
                        requested.display()
                    )));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ToolError::Execution(format!(
                    "invalid path: {}",
                    requested.display()
                )));
            }
        }
    }
    Ok(root.join(clean))
}

fn ensure_inside(root: &Path, resolved: PathBuf) -> Result<PathBuf, ToolError> {
    if resolved.starts_with(root) {
        Ok(resolved)
    } else {
        Err(ToolError::Execution(format!(
            "path resolves outside working directory: {}",
            resolved.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_escape() {
        let root = tempfile::tempdir().unwrap();
        assert!(for_write(root.path(), "../secret").is_err());
    }

    #[test]
    fn accepts_nested_new_file() {
        let root = tempfile::tempdir().unwrap();
        let path = for_write(root.path(), "src/new/file.rs").unwrap();
        assert!(path.starts_with(std::fs::canonicalize(root.path()).unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_existing_symlink_to_outside_the_workdir() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = root.path().join("outside.txt");
        let target = outside.path().join("secret.txt");
        std::fs::write(&target, "secret").unwrap();
        std::os::unix::fs::symlink(target, &link).unwrap();

        assert!(for_write(root.path(), "outside.txt").is_err());
    }
}
