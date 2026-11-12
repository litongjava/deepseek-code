//! Side-effect-free filesystem preflight for a parsed apply-patch plan.
//!
//! This module resolves every patch path against explicitly trusted roots,
//! reads the actual bytes, and delegates byte calculation to
//! [`super::apply_patch::plan_patch`]. It never creates, writes, renames or
//! deletes anything: [`preflight_patch`] only returns the resolved operations
//! and their expected old/new bytes for a later apply stage.
//!
//! # Path policy
//!
//! - [`TrustedRoots::new`] takes the workspace plus additional roots. Every
//!   root must already exist and is canonicalized.
//! - Relative patch paths join the workspace. Absolute paths are accepted only
//!   when the resolved target stays inside a trusted root. Containment is
//!   checked on canonical components, never on raw string prefixes.
//! - Parent directories may be symbolic links only when the canonical parent
//!   stays inside a trusted root. A final-component symbolic link is rejected
//!   rather than followed, so a read, rename or delete always acts on the same
//!   object; dangling links are reported explicitly.
//! - Existing targets must be regular files. Directories, sockets, FIFOs and
//!   devices are rejected.
//! - A missing final path component is resolved through the nearest existing
//!   ancestor directory. That directory must be in scope and must be a real
//!   directory; intermediate directories are not created here.
//! - Two targets that normalize to the same path, or where one target is an
//!   ancestor of the other, are rejected before any plan is built.
//!
//! # Explicit operation semantics
//!
//! Add fails when the destination exists or is a dangling link. Update, Delete
//! and a Move source fail when the source is missing. Move fails when its
//! destination exists and is validated independently from its source. Modes of
//! existing files are recorded so a later apply stage can preserve them.
//!
//! A multi-file filesystem apply is not crash-atomic without a journal; that
//! contract belongs to the apply stage and is not claimed here.
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs, io,
    path::{Component, Path, PathBuf},
};

use super::apply_patch::{
    plan_patch, ParsedPatch, PatchError, PatchInputs, PatchOperation, PlannedChange,
};

/// Trusted roots for one explicit apply. Constructed per call, never global.
#[derive(Debug, Clone)]
pub(crate) struct TrustedRoots {
    workspace: PathBuf,
    roots: Vec<PathBuf>,
}

impl TrustedRoots {
    /// Canonicalize the workspace and additional roots.
    pub(crate) fn new(
        workspace: impl AsRef<Path>,
        additional: &[PathBuf],
    ) -> Result<Self, FsPatchError> {
        let workspace = canonical_root(workspace.as_ref())?;
        let mut roots = vec![workspace.clone()];
        for root in additional {
            let canonical = canonical_root(root)?;
            if !roots.contains(&canonical) {
                roots.push(canonical);
            }
        }
        Ok(Self { workspace, roots })
    }

    /// Join a patch path to the workspace and normalize `.` components.
    ///
    /// `..` is rejected outright: lexical normalization of `..` after a
    /// symbolic-link component would not denote the same object the kernel
    /// resolves, so accepting it could silently edit a different file than the
    /// patch text denotes. Callers use paths relative to a trusted root.
    fn resolve_patch_path(&self, raw: &str) -> Result<PathBuf, FsPatchError> {
        let requested = Path::new(raw);
        if requested
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(FsPatchError::Path {
                path: raw.to_string(),
                message: "'..' components are not supported; after a symbolic link they would not denote the same object as lexical normalization. Use a path relative to a trusted root".into(),
            });
        }
        let joined = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.workspace.join(requested)
        };
        let mut normalized = PathBuf::new();
        for component in joined.components() {
            match component {
                Component::CurDir => {}
                other => normalized.push(other.as_os_str()),
            }
        }
        Ok(normalized)
    }

    fn require_contained(&self, raw: &str, canonical: &Path) -> Result<(), FsPatchError> {
        if self.roots.iter().any(|root| canonical.starts_with(root)) {
            return Ok(());
        }
        Err(FsPatchError::Path {
            path: raw.to_string(),
            message: format!(
                "resolves to '{}', outside the trusted workspace and additional roots",
                canonical.display()
            ),
        })
    }

    /// Resolve an existing regular file without reading its bytes. The apply
    /// stage reruns this to detect a path, symlink or parent change since
    /// preflight.
    fn resolve_existing(&self, raw: &str) -> Result<(PathBuf, Option<u32>), FsPatchError> {
        let lexical = self.resolve_patch_path(raw)?;
        let metadata = match fs::symlink_metadata(&lexical) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(FsPatchError::Path {
                    path: raw.to_string(),
                    message: format!("'{}' does not exist", lexical.display()),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotADirectory => {
                return Err(FsPatchError::Path {
                    path: raw.to_string(),
                    message: format!("'{}' is not a directory", lexical.display()),
                });
            }
            Err(error) => return Err(io_error(&lexical, error)),
        };
        if metadata.file_type().is_symlink() {
            let detail = match fs::canonicalize(&lexical) {
                Ok(target) => format!(
                    "'{}' is a symbolic link to '{}'; final symbolic links are not followed",
                    lexical.display(),
                    target.display()
                ),
                Err(_) => format!("'{}' is a dangling symbolic link", lexical.display()),
            };
            return Err(FsPatchError::Path {
                path: raw.to_string(),
                message: detail,
            });
        }
        if metadata.is_dir() {
            return Err(FsPatchError::Path {
                path: raw.to_string(),
                message: format!(
                    "'{}' is a directory; patch operations target regular files",
                    lexical.display()
                ),
            });
        }
        if !metadata.is_file() {
            return Err(FsPatchError::Path {
                path: raw.to_string(),
                message: format!(
                    "'{}' is not a regular file; special files are unsupported",
                    lexical.display()
                ),
            });
        }
        let canonical = fs::canonicalize(&lexical).map_err(|error| io_error(&lexical, error))?;
        self.require_contained(raw, &canonical)?;
        Ok((canonical, permission_bits(&metadata)))
    }

    fn read_existing(&self, raw: &str) -> Result<ExistingFile, FsPatchError> {
        let (path, mode) = self.resolve_existing(raw)?;
        let bytes = fs::read(&path).map_err(|error| io_error(&path, error))?;
        Ok(ExistingFile { path, bytes, mode })
    }

    pub(crate) fn revalidate_existing(
        &self,
        raw: &str,
    ) -> Result<(PathBuf, Option<u32>), FsPatchError> {
        self.resolve_existing(raw)
    }

    pub(crate) fn revalidate_new(&self, raw: &str) -> Result<PathBuf, FsPatchError> {
        self.resolve_new(raw)
    }

    pub(crate) fn contains(&self, canonical: &Path) -> bool {
        self.roots.iter().any(|root| canonical.starts_with(root))
    }

    /// Resolve a path that must not exist yet. The nearest existing parent must
    /// be an in-scope real directory; missing intermediate directories are
    /// carried lexically, not created.
    fn resolve_new(&self, raw: &str) -> Result<PathBuf, FsPatchError> {
        let lexical = self.resolve_patch_path(raw)?;
        match fs::symlink_metadata(&lexical) {
            Ok(metadata) => {
                let detail = if metadata.file_type().is_symlink() {
                    if fs::canonicalize(&lexical).is_ok() {
                        format!("'{}' already exists as a symbolic link", lexical.display())
                    } else {
                        format!("'{}' is a dangling symbolic link", lexical.display())
                    }
                } else if metadata.is_dir() {
                    format!("'{}' already exists as a directory", lexical.display())
                } else if metadata.is_file() {
                    format!("'{}' already exists", lexical.display())
                } else {
                    format!("'{}' already exists as a special file", lexical.display())
                };
                Err(FsPatchError::Path {
                    path: raw.to_string(),
                    message: detail,
                })
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                self.missing_destination(raw, &lexical)
            }
            Err(error) => Err(io_error(&lexical, error)),
        }
    }

    fn missing_destination(&self, raw: &str, lexical: &Path) -> Result<PathBuf, FsPatchError> {
        let mut missing: Vec<OsString> = Vec::new();
        let mut current = lexical.to_path_buf();
        loop {
            let Some(name) = current.file_name().map(|name| name.to_os_string()) else {
                return Err(FsPatchError::Path {
                    path: raw.to_string(),
                    message: format!(
                        "'{}' has no existing parent directory inside a trusted root",
                        lexical.display()
                    ),
                });
            };
            missing.push(name);
            let Some(parent) = current.parent() else {
                return Err(FsPatchError::Path {
                    path: raw.to_string(),
                    message: format!(
                        "'{}' has no existing parent directory inside a trusted root",
                        lexical.display()
                    ),
                });
            };
            match fs::symlink_metadata(parent) {
                Ok(metadata) => {
                    let canonical_parent = match fs::canonicalize(parent) {
                        Ok(path) => path,
                        Err(_) if metadata.file_type().is_symlink() => {
                            return Err(FsPatchError::Path {
                                path: raw.to_string(),
                                message: format!(
                                    "nearest existing parent '{}' is a dangling symbolic link",
                                    parent.display()
                                ),
                            });
                        }
                        Err(error) => return Err(io_error(parent, error)),
                    };
                    let parent_metadata =
                        fs::metadata(&canonical_parent).map_err(|error| io_error(parent, error))?;
                    if !parent_metadata.is_dir() {
                        return Err(FsPatchError::Path {
                            path: raw.to_string(),
                            message: format!(
                                "nearest existing parent '{}' is not a directory",
                                canonical_parent.display()
                            ),
                        });
                    }
                    self.require_contained(raw, &canonical_parent)?;
                    let mut resolved = canonical_parent;
                    for name in missing.iter().rev() {
                        resolved.push(name);
                    }
                    return Ok(resolved);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    current = parent.to_path_buf();
                }
                Err(error) if error.kind() == io::ErrorKind::NotADirectory => {
                    return Err(FsPatchError::Path {
                        path: raw.to_string(),
                        message: format!("'{}' is not a directory", parent.display()),
                    });
                }
                Err(error) => return Err(io_error(parent, error)),
            }
        }
    }
}

fn canonical_root(path: &Path) -> Result<PathBuf, FsPatchError> {
    let canonical = fs::canonicalize(path).map_err(|error| FsPatchError::Root {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let metadata = fs::metadata(&canonical).map_err(|error| FsPatchError::Root {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    if !metadata.is_dir() {
        return Err(FsPatchError::Root {
            path: path.to_path_buf(),
            message: "trusted root is not a directory".into(),
        });
    }
    Ok(canonical)
}

#[cfg(unix)]
fn permission_bits(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn permission_bits(_metadata: &fs::Metadata) -> Option<u32> {
    None
}

fn io_error(path: &Path, error: io::Error) -> FsPatchError {
    FsPatchError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    }
}

#[derive(Debug)]
struct ExistingFile {
    path: PathBuf,
    bytes: Vec<u8>,
    mode: Option<u32>,
}

#[derive(Debug)]
struct ResolvedTarget {
    path: PathBuf,
    mode: Option<u32>,
}

#[derive(Debug)]
struct Endpoint {
    raw: String,
    path: PathBuf,
}

fn check_conflicts(endpoints: &[Endpoint]) -> Result<(), FsPatchError> {
    for (index, first) in endpoints.iter().enumerate() {
        for second in &endpoints[index + 1..] {
            if first.path == second.path {
                return Err(FsPatchError::Conflict {
                    first: first.raw.clone(),
                    second: second.raw.clone(),
                    message: format!("both resolve to '{}'", first.path.display()),
                });
            }
            if first.path.starts_with(&second.path) || second.path.starts_with(&first.path) {
                return Err(FsPatchError::Conflict {
                    first: first.raw.clone(),
                    second: second.raw.clone(),
                    message: "one target is an ancestor of the other".into(),
                });
            }
        }
    }
    Ok(())
}

/// The resolved changes a later apply stage may install. Nothing is written by
/// [`preflight_patch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiskPlan {
    pub changes: Vec<DiskChange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiskChange {
    Add {
        path: PathBuf,
        /// Original patch path, kept for diagnostics and records.
        raw_path: String,
        contents: Vec<u8>,
    },
    Update {
        path: PathBuf,
        raw_path: String,
        old_contents: Vec<u8>,
        new_contents: Vec<u8>,
        /// Unix permission bits of the existing file, preserved on apply.
        mode: Option<u32>,
    },
    Delete {
        path: PathBuf,
        raw_path: String,
        old_contents: Vec<u8>,
        mode: Option<u32>,
    },
    Move {
        from: PathBuf,
        from_raw: String,
        to: PathBuf,
        to_raw: String,
        old_contents: Vec<u8>,
        new_contents: Vec<u8>,
        /// Mode of the move source, preserved on the destination.
        mode: Option<u32>,
    },
}

/// Resolve paths, read all bytes and compute every change without mutating the
/// filesystem. Any error leaves the tree untouched because nothing is staged.
pub(crate) fn preflight_patch(
    roots: &TrustedRoots,
    patch: &ParsedPatch,
) -> Result<DiskPlan, FsPatchError> {
    let mut inputs = PatchInputs::new();
    let mut resolved: BTreeMap<String, ResolvedTarget> = BTreeMap::new();
    let mut endpoints = Vec::new();

    for operation in &patch.operations {
        match operation {
            PatchOperation::Add { path, .. } => {
                let target = roots.resolve_new(path)?;
                endpoints.push(Endpoint {
                    raw: path.clone(),
                    path: target.clone(),
                });
                resolved.insert(
                    path.clone(),
                    ResolvedTarget {
                        path: target,
                        mode: None,
                    },
                );
            }
            PatchOperation::Update { path, .. } => {
                let file = roots.read_existing(path)?;
                endpoints.push(Endpoint {
                    raw: path.clone(),
                    path: file.path.clone(),
                });
                inputs.insert(path.clone(), file.bytes);
                resolved.insert(
                    path.clone(),
                    ResolvedTarget {
                        path: file.path,
                        mode: file.mode,
                    },
                );
            }
            PatchOperation::Delete { path } => {
                let file = roots.read_existing(path)?;
                endpoints.push(Endpoint {
                    raw: path.clone(),
                    path: file.path.clone(),
                });
                inputs.insert(path.clone(), file.bytes);
                resolved.insert(
                    path.clone(),
                    ResolvedTarget {
                        path: file.path,
                        mode: file.mode,
                    },
                );
            }
            PatchOperation::Move { from, to, .. } => {
                let file = roots.read_existing(from)?;
                let destination = roots.resolve_new(to)?;
                endpoints.push(Endpoint {
                    raw: from.clone(),
                    path: file.path.clone(),
                });
                endpoints.push(Endpoint {
                    raw: to.clone(),
                    path: destination.clone(),
                });
                inputs.insert(from.clone(), file.bytes);
                resolved.insert(
                    from.clone(),
                    ResolvedTarget {
                        path: file.path,
                        mode: file.mode,
                    },
                );
                resolved.insert(
                    to.clone(),
                    ResolvedTarget {
                        path: destination,
                        mode: file.mode,
                    },
                );
            }
        }
    }

    check_conflicts(&endpoints)?;
    let plan = plan_patch(patch, &inputs)?;

    let mut changes = Vec::with_capacity(plan.changes.len());
    for change in plan.changes {
        changes.push(match change {
            PlannedChange::Add { path, contents } => {
                let target = take_target(&mut resolved, &path)?;
                DiskChange::Add {
                    path: target.path,
                    raw_path: path,
                    contents,
                }
            }
            PlannedChange::Update {
                path,
                old_contents,
                new_contents,
            } => {
                let target = take_target(&mut resolved, &path)?;
                DiskChange::Update {
                    path: target.path,
                    raw_path: path,
                    old_contents,
                    new_contents,
                    mode: target.mode,
                }
            }
            PlannedChange::Delete { path, old_contents } => {
                let target = take_target(&mut resolved, &path)?;
                DiskChange::Delete {
                    path: target.path,
                    raw_path: path,
                    old_contents,
                    mode: target.mode,
                }
            }
            PlannedChange::Move {
                from,
                to,
                old_contents,
                new_contents,
            } => {
                let source = take_target(&mut resolved, &from)?;
                let destination = take_target(&mut resolved, &to)?;
                DiskChange::Move {
                    from: source.path,
                    from_raw: from,
                    to: destination.path,
                    to_raw: to,
                    old_contents,
                    new_contents,
                    mode: source.mode,
                }
            }
        });
    }

    Ok(DiskPlan { changes })
}

fn take_target(
    resolved: &mut BTreeMap<String, ResolvedTarget>,
    path: &str,
) -> Result<ResolvedTarget, FsPatchError> {
    resolved.remove(path).ok_or_else(|| FsPatchError::Path {
        path: path.to_string(),
        message: "internal: preflight did not resolve this target".into(),
    })
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum FsPatchError {
    #[error("invalid trusted root '{path}': {message}")]
    Root { path: PathBuf, message: String },
    #[error("cannot use patch path '{path}': {message}")]
    Path { path: String, message: String },
    #[error("conflicting patch targets '{first}' and '{second}': {message}")]
    Conflict {
        first: String,
        second: String,
        message: String,
    },
    #[error("cannot read '{path}': {message}")]
    Io { path: String, message: String },
    #[error(transparent)]
    Plan(#[from] PatchError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::apply_patch::parse_patch;

    #[derive(Debug, PartialEq, Eq)]
    enum Node {
        Dir,
        File(Vec<u8>),
        Link(PathBuf),
        Other,
    }

    fn tree(root: &Path) -> BTreeMap<PathBuf, Node> {
        let mut nodes = BTreeMap::new();
        for entry in walkdir::WalkDir::new(root).follow_links(false) {
            let entry = entry.unwrap();
            let path = entry.path().to_path_buf();
            let kind = entry.file_type();
            let node = if kind.is_symlink() {
                Node::Link(fs::read_link(&path).unwrap())
            } else if kind.is_dir() {
                Node::Dir
            } else if kind.is_file() {
                Node::File(fs::read(&path).unwrap())
            } else {
                Node::Other
            };
            nodes.insert(path, node);
        }
        nodes
    }

    fn patch(text: &str) -> ParsedPatch {
        parse_patch(text).unwrap()
    }

    fn path_error(result: Result<DiskPlan, FsPatchError>) -> (String, String) {
        match result {
            Err(FsPatchError::Path { path, message }) => (path, message),
            other => panic!("expected a path error, got {other:?}"),
        }
    }

    fn conflict_error(result: Result<DiskPlan, FsPatchError>) -> (String, String, String) {
        match result {
            Err(FsPatchError::Conflict {
                first,
                second,
                message,
            }) => (first, second, message),
            other => panic!("expected a conflict, got {other:?}"),
        }
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, bits: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(bits)).unwrap();
    }

    #[test]
    fn mixed_plan_resolves_paths_and_preserves_the_tree() {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        fs::create_dir_all(work.join("new/sub")).unwrap();
        fs::write(work.join("edit.txt"), "one\ntwo\n").unwrap();
        fs::write(work.join("del.txt"), "delete me\n").unwrap();
        fs::write(work.join("move.txt"), "old\n").unwrap();
        #[cfg(unix)]
        {
            set_mode(&work.join("edit.txt"), 0o755);
            set_mode(&work.join("move.txt"), 0o640);
        }
        let canonical_work = fs::canonicalize(&work).unwrap();
        let roots = TrustedRoots::new(&work, &[]).unwrap();
        let parsed = patch(
            "\
*** Begin Patch
*** Add File: new/sub/added.txt
+added line
*** Update File: edit.txt
@@
 one
-two
+TWO
*** Delete File: del.txt
*** Update File: move.txt
*** Move to: moved.txt
@@
-old
+NEW
*** Add File: root.txt
+root
*** End Patch",
        );

        let before = tree(temp.path());
        let plan = preflight_patch(&roots, &parsed).unwrap();
        assert_eq!(plan.changes.len(), 5);
        assert_eq!(
            plan.changes[0],
            DiskChange::Add {
                path: canonical_work.join("new/sub/added.txt"),
                raw_path: "new/sub/added.txt".into(),
                contents: b"added line\n".to_vec(),
            }
        );
        match &plan.changes[1] {
            DiskChange::Update {
                path,
                old_contents,
                new_contents,
                mode,
                ..
            } => {
                assert_eq!(path, &canonical_work.join("edit.txt"));
                assert_eq!(old_contents.as_slice(), b"one\ntwo\n");
                assert_eq!(new_contents.as_slice(), b"one\nTWO\n");
                #[cfg(unix)]
                assert_eq!(*mode, Some(0o755));
                let _ = mode;
            }
            other => panic!("expected an update, got {other:?}"),
        }
        match &plan.changes[2] {
            DiskChange::Delete {
                path, old_contents, ..
            } => {
                assert_eq!(path, &canonical_work.join("del.txt"));
                assert_eq!(old_contents.as_slice(), b"delete me\n");
            }
            other => panic!("expected a delete, got {other:?}"),
        }
        match &plan.changes[3] {
            DiskChange::Move {
                from,
                to,
                old_contents,
                new_contents,
                mode,
                ..
            } => {
                assert_eq!(from, &canonical_work.join("move.txt"));
                assert_eq!(to, &canonical_work.join("moved.txt"));
                assert_eq!(old_contents.as_slice(), b"old\n");
                assert_eq!(new_contents.as_slice(), b"NEW\n");
                #[cfg(unix)]
                assert_eq!(*mode, Some(0o640));
                let _ = mode;
            }
            other => panic!("expected a move, got {other:?}"),
        }
        assert_eq!(
            plan.changes[4],
            DiskChange::Add {
                path: canonical_work.join("root.txt"),
                raw_path: "root.txt".into(),
                contents: b"root\n".to_vec(),
            }
        );
        assert_eq!(tree(temp.path()), before);
    }

    #[test]
    fn second_file_mismatch_leaves_whole_tree_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("a.txt"), "old\n").unwrap();
        fs::write(work.join("b.txt"), "unrelated\n").unwrap();
        let roots = TrustedRoots::new(&work, &[]).unwrap();
        let parsed = patch(
            "\
*** Begin Patch
*** Update File: a.txt
@@
-old
+new
*** Update File: b.txt
@@
-absent
+new
*** End Patch",
        );

        let before = tree(temp.path());
        match preflight_patch(&roots, &parsed) {
            Err(FsPatchError::Plan(PatchError::Update {
                path,
                hunk,
                message,
            })) => {
                assert_eq!(path, "b.txt");
                assert_eq!(hunk, 1);
                assert!(message.contains("were not found"), "{message}");
            }
            other => panic!("expected an update error, got {other:?}"),
        }
        assert_eq!(tree(temp.path()), before);
    }

    #[test]
    fn normalized_aliases_and_ancestor_conflicts_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("a.txt"), "old\n").unwrap();
        fs::write(work.join("b.txt"), "old\n").unwrap();
        let roots = TrustedRoots::new(&work, &[]).unwrap();
        let before = tree(temp.path());

        let alias = patch(
            "\
*** Begin Patch
*** Update File: a.txt
@@
-old
+new
*** Update File: ./a.txt
@@
-old
+new
*** End Patch",
        );
        let (first, second, message) = conflict_error(preflight_patch(&roots, &alias));
        assert_eq!((first.as_str(), second.as_str()), ("a.txt", "./a.txt"));
        assert!(message.contains("both resolve"), "{message}");

        let ancestor = patch(
            "\
*** Begin Patch
*** Add File: d
+x
*** Add File: d/x
+y
*** End Patch",
        );
        let (_, _, message) = conflict_error(preflight_patch(&roots, &ancestor));
        assert!(message.contains("ancestor"), "{message}");

        let destinations = patch(
            "\
*** Begin Patch
*** Update File: a.txt
*** Move to: dest
*** Update File: b.txt
*** Move to: ./dest
*** End Patch",
        );
        let (_, _, message) = conflict_error(preflight_patch(&roots, &destinations));
        assert!(message.contains("both resolve"), "{message}");

        assert_eq!(tree(temp.path()), before);
    }

    #[test]
    fn traversal_and_absolute_escape_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "secret\n").unwrap();
        let roots = TrustedRoots::new(&work, &[]).unwrap();
        let before = tree(temp.path());

        let (path, message) = path_error(preflight_patch(
            &roots,
            &patch("*** Begin Patch\n*** Delete File: ../outside/secret.txt\n*** End Patch"),
        ));
        assert_eq!(path, "../outside/secret.txt");
        assert!(
            message.contains("'..' components are not supported"),
            "{message}"
        );

        let absolute = outside.join("secret.txt").display().to_string();
        let text = format!("*** Begin Patch\n*** Delete File: {absolute}\n*** End Patch");
        let (path, message) = path_error(preflight_patch(&roots, &patch(&text)));
        assert_eq!(path, absolute);
        assert!(message.contains("outside the trusted"), "{message}");

        assert_eq!(tree(temp.path()), before);
    }

    #[cfg(unix)]
    #[test]
    fn dotdot_after_a_symlinked_parent_is_rejected_instead_of_normalized() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        // `work/link` points at `work/real/nested`; the kernel resolves
        // `link/../decoy.txt` to `work/real/decoy.txt`, while lexical
        // normalization would produce `work/decoy.txt`.
        fs::create_dir_all(work.join("real/nested")).unwrap();
        fs::write(work.join("real/decoy.txt"), "kernel target\n").unwrap();
        fs::write(work.join("decoy.txt"), "lexical target\n").unwrap();
        symlink(work.join("real/nested"), work.join("link")).unwrap();
        let roots = TrustedRoots::new(&work, &[]).unwrap();
        let before = tree(temp.path());

        let (path, message) = path_error(preflight_patch(
            &roots,
            &patch("*** Begin Patch\n*** Delete File: link/../decoy.txt\n*** End Patch"),
        ));
        assert_eq!(path, "link/../decoy.txt");
        assert!(
            message.contains("'..' components are not supported"),
            "{message}"
        );
        assert_eq!(tree(temp.path()), before);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_and_dangling_destinations_are_rejected() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "secret\n").unwrap();
        symlink(outside.join("secret.txt"), work.join("link.txt")).unwrap();
        symlink(&outside, work.join("linked")).unwrap();
        symlink(work.join("missing"), work.join("dangling")).unwrap();
        let roots = TrustedRoots::new(&work, &[]).unwrap();
        let before = tree(temp.path());

        let (_, message) = path_error(preflight_patch(
            &roots,
            &patch("*** Begin Patch\n*** Update File: link.txt\n@@\n-secret\n+leak\n*** End Patch"),
        ));
        assert!(message.contains("symbolic link"), "{message}");

        let (_, message) = path_error(preflight_patch(
            &roots,
            &patch(
                "*** Begin Patch\n*** Update File: linked/secret.txt\n@@\n-secret\n+leak\n*** End Patch",
            ),
        ));
        assert!(message.contains("outside the trusted"), "{message}");

        let (_, message) = path_error(preflight_patch(
            &roots,
            &patch("*** Begin Patch\n*** Add File: dangling\n+x\n*** End Patch"),
        ));
        assert!(message.contains("dangling"), "{message}");

        let (_, message) = path_error(preflight_patch(
            &roots,
            &patch("*** Begin Patch\n*** Update File: dangling\n@@\n-x\n+y\n*** End Patch"),
        ));
        assert!(message.contains("dangling"), "{message}");

        assert_eq!(tree(temp.path()), before);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_parent_inside_scope_is_resolved_and_final_links_are_rejected() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        fs::create_dir_all(work.join("pkg")).unwrap();
        fs::write(work.join("pkg/file.txt"), "old\n").unwrap();
        symlink(work.join("pkg"), work.join("alias")).unwrap();
        symlink(work.join("pkg/file.txt"), work.join("filelink")).unwrap();
        let canonical_work = fs::canonicalize(&work).unwrap();
        let roots = TrustedRoots::new(&work, &[]).unwrap();
        let before = tree(temp.path());

        let plan = preflight_patch(
            &roots,
            &patch(
                "*** Begin Patch\n*** Update File: alias/file.txt\n@@\n-old\n+new\n*** End Patch",
            ),
        )
        .unwrap();
        match &plan.changes[0] {
            DiskChange::Update { path, .. } => {
                assert_eq!(path, &canonical_work.join("pkg/file.txt"));
            }
            other => panic!("expected an update, got {other:?}"),
        }

        let (_, message) = path_error(preflight_patch(
            &roots,
            &patch("*** Begin Patch\n*** Update File: filelink\n@@\n-old\n+new\n*** End Patch"),
        ));
        assert!(
            message.contains("final symbolic links are not followed"),
            "{message}"
        );

        assert_eq!(tree(temp.path()), before);
    }

    #[test]
    fn missing_destinations_resolve_through_existing_parents() {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("plainfile"), "x\n").unwrap();
        fs::write(work.join("source.txt"), "old\n").unwrap();
        let canonical_work = fs::canonicalize(&work).unwrap();
        let roots = TrustedRoots::new(&work, &[]).unwrap();
        let before = tree(temp.path());

        let plan = preflight_patch(
            &roots,
            &patch("*** Begin Patch\n*** Add File: newdir/deep/file.txt\n+x\n*** End Patch"),
        )
        .unwrap();
        assert_eq!(
            plan.changes[0],
            DiskChange::Add {
                path: canonical_work.join("newdir/deep/file.txt"),
                raw_path: "newdir/deep/file.txt".into(),
                contents: b"x\n".to_vec(),
            }
        );

        let (_, message) = path_error(preflight_patch(
            &roots,
            &patch("*** Begin Patch\n*** Add File: plainfile/child.txt\n+x\n*** End Patch"),
        ));
        assert!(message.contains("not a directory"), "{message}");

        let (_, message) = path_error(preflight_patch(
            &roots,
            &patch(
                "*** Begin Patch\n*** Update File: source.txt\n*** Move to: plainfile/child.txt\n*** End Patch",
            ),
        ));
        assert!(message.contains("not a directory"), "{message}");

        assert_eq!(tree(temp.path()), before);
    }

    #[test]
    fn add_existing_delete_missing_and_directory_or_special_targets_are_explicit() {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        fs::create_dir_all(work.join("dir")).unwrap();
        fs::write(work.join("existing.txt"), "old\n").unwrap();
        let roots = TrustedRoots::new(&work, &[]).unwrap();
        let before = tree(temp.path());

        let (_, message) = path_error(preflight_patch(
            &roots,
            &patch("*** Begin Patch\n*** Add File: existing.txt\n+x\n*** End Patch"),
        ));
        assert!(message.contains("already exists"), "{message}");

        let (_, message) = path_error(preflight_patch(
            &roots,
            &patch("*** Begin Patch\n*** Add File: dir\n+x\n*** End Patch"),
        ));
        assert!(
            message.contains("already exists as a directory"),
            "{message}"
        );

        let (_, message) = path_error(preflight_patch(
            &roots,
            &patch("*** Begin Patch\n*** Delete File: missing.txt\n*** End Patch"),
        ));
        assert!(message.contains("does not exist"), "{message}");

        for text in [
            "*** Begin Patch\n*** Delete File: dir\n*** End Patch",
            "*** Begin Patch\n*** Update File: dir\n@@\n-x\n+y\n*** End Patch",
        ] {
            let (_, message) = path_error(preflight_patch(&roots, &patch(text)));
            assert!(message.contains("is a directory"), "{message}");
        }

        #[cfg(unix)]
        {
            let listener = std::os::unix::net::UnixListener::bind(work.join("sock")).unwrap();
            let before_socket = tree(temp.path());
            let (_, message) = path_error(preflight_patch(
                &roots,
                &patch("*** Begin Patch\n*** Delete File: sock\n*** End Patch"),
            ));
            assert!(message.contains("not a regular file"), "{message}");
            assert_eq!(tree(temp.path()), before_socket);
            drop(listener);
            fs::remove_file(work.join("sock")).unwrap();
        }

        assert_eq!(tree(temp.path()), before);
    }

    #[test]
    fn absolute_paths_inside_trusted_roots_and_additional_roots_are_allowed() {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        let extra = temp.path().join("extra");
        let outside = temp.path().join("outside");
        for directory in [&work, &extra, &outside] {
            fs::create_dir_all(directory).unwrap();
        }
        fs::create_dir_all(work.join("sub")).unwrap();
        fs::write(work.join("sub/inner.txt"), "old\n").unwrap();
        fs::write(extra.join("extra.txt"), "old\n").unwrap();
        fs::write(outside.join("outside.txt"), "old\n").unwrap();
        let extra = fs::canonicalize(&extra).unwrap();
        let roots = TrustedRoots::new(&work, std::slice::from_ref(&extra)).unwrap();
        let before = tree(temp.path());

        let text = format!(
            "*** Begin Patch\n*** Update File: {}\n@@\n-old\n+new\n*** Update File: {}\n@@\n-old\n+new\n*** Add File: {}\n+x\n*** End Patch",
            extra.join("extra.txt").display(),
            fs::canonicalize(&work).unwrap().join("sub/inner.txt").display(),
            fs::canonicalize(&work).unwrap().join("sub/added.txt").display(),
        );
        let plan = preflight_patch(&roots, &patch(&text)).unwrap();
        assert_eq!(plan.changes.len(), 3);
        match &plan.changes[0] {
            DiskChange::Update { path, .. } => assert_eq!(path, &extra.join("extra.txt")),
            other => panic!("expected an update, got {other:?}"),
        }

        let text = format!(
            "*** Begin Patch\n*** Update File: {}\n@@\n-old\n+new\n*** End Patch",
            outside.join("outside.txt").display()
        );
        let (_, message) = path_error(preflight_patch(&roots, &patch(&text)));
        assert!(message.contains("outside the trusted"), "{message}");

        assert_eq!(tree(temp.path()), before);
    }

    #[test]
    fn pure_binary_move_preserves_bytes_and_binary_text_updates_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let binary = vec![0u8, 0xff, 0x80, 0x00];
        fs::write(work.join("bin.dat"), &binary).unwrap();
        fs::write(work.join("nul.dat"), b"\0binary\n").unwrap();
        let roots = TrustedRoots::new(&work, &[]).unwrap();
        let before = tree(temp.path());

        let plan = preflight_patch(
            &roots,
            &patch("*** Begin Patch\n*** Update File: bin.dat\n*** Move to: renamed.dat\n*** End Patch"),
        )
        .unwrap();
        match &plan.changes[0] {
            DiskChange::Move {
                old_contents,
                new_contents,
                ..
            } => {
                assert_eq!(old_contents, &binary);
                assert_eq!(new_contents, &binary);
            }
            other => panic!("expected a move, got {other:?}"),
        }

        for text in [
            "*** Begin Patch\n*** Update File: bin.dat\n@@\n-a\n+b\n*** End Patch",
            "*** Begin Patch\n*** Update File: nul.dat\n@@\n-a\n+b\n*** End Patch",
            "*** Begin Patch\n*** Update File: bin.dat\n*** Move to: edited.dat\n@@\n-a\n+b\n*** End Patch",
        ] {
            match preflight_patch(&roots, &patch(text)) {
                Err(FsPatchError::Plan(PatchError::File { .. })) => {}
                other => panic!("expected a binary content error for {text:?}, got {other:?}"),
            }
        }

        assert_eq!(tree(temp.path()), before);
    }

    #[test]
    fn move_destination_is_validated_independently() {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(work.join("move.txt"), "old\n").unwrap();
        fs::write(work.join("taken.txt"), "occupied\n").unwrap();
        let roots = TrustedRoots::new(&work, &[]).unwrap();
        let before = tree(temp.path());

        let (path, message) = path_error(preflight_patch(
            &roots,
            &patch(
                "*** Begin Patch\n*** Update File: move.txt\n*** Move to: taken.txt\n*** End Patch",
            ),
        ));
        assert_eq!(path, "taken.txt");
        assert!(message.contains("already exists"), "{message}");

        let (path, message) = path_error(preflight_patch(
            &roots,
            &patch(
                "*** Begin Patch\n*** Update File: move.txt\n*** Move to: ../outside/moved.txt\n*** End Patch",
            ),
        ));
        assert_eq!(path, "../outside/moved.txt");
        assert!(
            message.contains("'..' components are not supported"),
            "{message}"
        );

        let (path, message) = path_error(preflight_patch(
            &roots,
            &patch("*** Begin Patch\n*** Update File: missing.txt\n*** Move to: new.txt\n*** End Patch"),
        ));
        assert_eq!(path, "missing.txt");
        assert!(message.contains("does not exist"), "{message}");

        let plan = preflight_patch(
            &roots,
            &patch("*** Begin Patch\n*** Update File: move.txt\n*** Move to: newdir/moved.txt\n*** End Patch"),
        )
        .unwrap();
        match &plan.changes[0] {
            DiskChange::Move { to, .. } => {
                assert_eq!(
                    to,
                    &fs::canonicalize(&work).unwrap().join("newdir/moved.txt")
                );
            }
            other => panic!("expected a move, got {other:?}"),
        }

        assert_eq!(tree(temp.path()), before);
    }
}
