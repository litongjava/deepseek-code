//! Presentation only: never use these labels to open a file or persist a tool call.
use std::path::{Component, Path};

pub(super) fn label(path: &str, workspace: Option<&Path>) -> String {
    let path = readable(path);
    let path = path.as_str();
    let workspace = workspace.map(|value| readable(&value.to_string_lossy()));
    let workspace = workspace.as_deref().map(Path::new);
    let Some(workspace) = workspace else {
        return path.into();
    };
    let candidate = Path::new(path);
    if !workspace.is_absolute() || !candidate.is_absolute() {
        return path.into();
    }
    let parts = candidate.components().collect::<Vec<_>>();
    let root = workspace.components().collect::<Vec<_>>();
    // Keep ambiguous traversal visible rather than implying workspace membership.
    if parts
        .iter()
        .chain(root.iter())
        .any(|part| matches!(part, Component::ParentDir))
        || parts.len() < root.len()
        || !parts.iter().zip(&root).all(|(a, b)| {
            if cfg!(windows) {
                a.as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
            } else {
                a == b
            }
        })
    {
        return path.into();
    }
    let relative = parts[root.len()..]
        .iter()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if relative.is_empty() {
        ".".into()
    } else {
        relative
    }
}

/// Windows extended paths remain unchanged for I/O; hide only their display prefix.
fn readable(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        if rest.as_bytes().get(1) == Some(&b':') {
            rest.into()
        } else {
            path.into()
        }
    } else {
        path.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_paths_are_display_only_and_unc_is_preserved() {
        assert_eq!(
            label(r"\\?\C:\Users\name\plans\plan.md", None),
            r"C:\Users\name\plans\plan.md"
        );
        assert_eq!(
            label(r"\\?\UNC\server\share\a.md", None),
            r"\\server\share\a.md"
        );
        assert_eq!(label(r"\\?\Volume{abc}\a", None), r"\\?\Volume{abc}\a");
    }

    #[test]
    fn workspace_labels_preserve_external_and_ambiguous_paths() {
        let workspace = std::env::temp_dir().join("display path workspace");
        let internal = workspace.join("中文 folder").join("hello.rs");
        assert_eq!(
            label(&internal.to_string_lossy(), Some(&workspace)),
            "中文 folder/hello.rs"
        );
        assert_eq!(label(&workspace.to_string_lossy(), Some(&workspace)), ".");
        for path in [
            workspace
                .with_file_name("display path workspace-other")
                .join("x.rs"),
            workspace.join("..").join("outside.rs"),
        ] {
            assert_eq!(
                label(&path.to_string_lossy(), Some(&workspace)),
                path.to_string_lossy()
            );
        }
        assert_eq!(
            label("relative/file.rs", Some(&workspace)),
            "relative/file.rs"
        );
        assert_eq!(
            label(&internal.to_string_lossy(), None),
            internal.to_string_lossy()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_case_slashes_and_unc_paths() {
        let root = Path::new(r"D:\Code\Project");
        assert_eq!(label("d:/code/project/src/a.rs", Some(root)), "src/a.rs");
        assert_eq!(
            label("E:/Code/Project/a.rs", Some(root)),
            "E:/Code/Project/a.rs"
        );
        assert_eq!(
            label(
                r"\\server\share\repo\文 件.rs",
                Some(Path::new(r"\\server\share\repo"))
            ),
            "文 件.rs"
        );
    }
}
