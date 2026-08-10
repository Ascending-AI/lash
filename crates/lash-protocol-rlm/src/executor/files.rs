use std::collections::BTreeMap;
use std::path::{Component, Path};

use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ScratchFileStamp {
    len: u64,
    modified_nanos: u128,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanos: i64,
}

impl ScratchFileStamp {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        let modified_nanos = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_nanos());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            Self {
                len: metadata.len(),
                modified_nanos,
                device: metadata.dev(),
                inode: metadata.ino(),
                changed_seconds: metadata.ctime(),
                changed_nanos: metadata.ctime_nsec(),
            }
        }
        #[cfg(not(unix))]
        {
            Self {
                len: metadata.len(),
                modified_nanos,
            }
        }
    }
}

pub(super) struct CollectedScratchFile {
    pub stamp: ScratchFileStamp,
    pub changed_body: Option<Vec<u8>>,
}

#[derive(Debug, Error)]
pub(crate) enum ScratchFileError {
    #[error("failed to create replacement scratch directory: {0}")]
    CreateScratch(#[source] std::io::Error),
    #[error("failed to enumerate scratch directory `{path}`: {source}")]
    Enumerate {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to inspect scratch path `{path}`: {source}")]
    Inspect {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("scratch path `{path}` is not valid UTF-8")]
    NonUtf8Path { path: String },
    #[error("failed to read scratch file `{path}`: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("scratch snapshot path `{path}` is not a safe relative path")]
    UnsafeRestorePath { path: String },
    #[error("failed to create scratch directory `{path}`: {source}")]
    CreateDirectory {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to restore scratch file `{path}`: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

pub(super) fn collect_files(
    root: &Path,
    previous: &BTreeMap<String, ScratchFileStamp>,
) -> Result<BTreeMap<String, CollectedScratchFile>, ScratchFileError> {
    let mut files = BTreeMap::new();
    walk_dir(root, root, previous, &mut files)?;
    Ok(files)
}

fn walk_dir(
    root: &Path,
    dir: &Path,
    previous: &BTreeMap<String, ScratchFileStamp>,
    files: &mut BTreeMap<String, CollectedScratchFile>,
) -> Result<(), ScratchFileError> {
    let entries = std::fs::read_dir(dir).map_err(|source| ScratchFileError::Enumerate {
        path: dir.display().to_string(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| ScratchFileError::Inspect {
            path: dir.display().to_string(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|source| ScratchFileError::Inspect {
                path: path.display().to_string(),
                source,
            })?;
        if file_type.is_dir() {
            walk_dir(root, &path, previous, files)?;
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let rel = rel
            .to_str()
            .ok_or_else(|| ScratchFileError::NonUtf8Path {
                path: rel.display().to_string(),
            })?
            .replace(std::path::MAIN_SEPARATOR, "/");
        let metadata = std::fs::metadata(&path).map_err(|source| ScratchFileError::Inspect {
            path: path.display().to_string(),
            source,
        })?;
        let stamp = ScratchFileStamp::from_metadata(&metadata);
        let changed_body = if previous.get(&rel) == Some(&stamp) {
            None
        } else {
            Some(
                std::fs::read(&path).map_err(|source| ScratchFileError::Read {
                    path: path.display().to_string(),
                    source,
                })?,
            )
        };
        files.insert(
            rel,
            CollectedScratchFile {
                stamp,
                changed_body,
            },
        );
    }
    Ok(())
}

pub(super) fn restore_files(
    root: &Path,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<(), ScratchFileError> {
    for (rel, contents) in files {
        let relative = Path::new(rel);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(ScratchFileError::UnsafeRestorePath { path: rel.clone() });
        }
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| {
                ScratchFileError::CreateDirectory {
                    path: parent.display().to_string(),
                    source,
                }
            })?;
        }
        std::fs::write(&path, contents).map_err(|source| ScratchFileError::Write {
            path: path.display().to_string(),
            source,
        })?;
    }
    Ok(())
}
