//! Descriptor-relative file operations reject symlinks in every component.
use bund_core::{Error, Result, ToolContext};
use nix::fcntl::{open, openat, renameat, AtFlags, OFlag};
use nix::sys::stat::{fstatat, Mode, SFlag};
use nix::unistd::{unlinkat, UnlinkatFlags};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

pub(crate) const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;

fn io(error: nix::errno::Errno) -> Error {
    std::io::Error::from_raw_os_error(error as i32).into()
}

pub(crate) fn relative(path: &str) -> Result<PathBuf> {
    let mut clean = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            _ => {
                return Err(Error::Denied(
                    "paths must be workspace-relative without '..'".into(),
                ))
            }
        }
    }
    Ok(clean)
}

pub(crate) fn root(context: &ToolContext) -> Result<PathBuf> {
    Ok(context.workspace.canonicalize()?)
}

fn open_dir(path: &Path) -> Result<OwnedFd> {
    open(
        path,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(io)
}

pub(crate) fn directory(context: &ToolContext, path: &Path) -> Result<OwnedFd> {
    let mut fd = open_dir(&root(context)?)?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(Error::Denied("invalid normalized directory".into()));
        };
        fd = openat(
            &fd,
            Path::new(name),
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(io)?;
    }
    Ok(fd)
}

pub(crate) fn checked_directory(context: &ToolContext, path: &Path) -> Result<PathBuf> {
    let _directory = directory(context, path)?;
    Ok(root(context)?.join(path))
}

pub(crate) struct Entry {
    parent: OwnedFd,
    name: PathBuf,
}

impl Entry {
    pub(crate) fn new(context: &ToolContext, path: &Path, writing: bool) -> Result<Self> {
        if writing && path.components().any(|part| part.as_os_str() == ".git") {
            return Err(Error::Denied(
                "editing Git metadata is not permitted".into(),
            ));
        }
        let name = path
            .file_name()
            .ok_or_else(|| Error::Invalid("expected a file path".into()))?;
        let parent = directory(context, path.parent().unwrap_or(Path::new("")))?;
        Ok(Self {
            parent,
            name: PathBuf::from(name),
        })
    }

    pub(crate) fn read(&self) -> Result<Option<Vec<u8>>> {
        let fd = match openat(
            &self.parent,
            &self.name,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(nix::errno::Errno::ENOENT) => return Ok(None),
            Err(e) => return Err(io(e)),
        };
        let mut file = File::from(fd);
        if !file.metadata()?.is_file() {
            return Err(Error::Denied("expected a regular file".into()));
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take((MAX_FILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(Error::Invalid("file exceeds 2 MiB limit".into()));
        }
        Ok(Some(bytes))
    }

    pub(crate) fn replace(&self, expected: Option<&[u8]>, content: Option<&[u8]>) -> Result<()> {
        if self.read()?.as_deref() != expected {
            return Err(Error::Invalid(
                "file changed since approval; refusing to overwrite".into(),
            ));
        }
        if let Some(content) = content {
            let mode = match openat(
                &self.parent,
                &self.name,
                OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
                Mode::empty(),
            ) {
                Ok(fd) => File::from(fd).metadata()?.permissions().mode() & 0o777,
                Err(nix::errno::Errno::ENOENT) => 0o644,
                Err(e) => return Err(io(e)),
            };
            let temporary = PathBuf::from(format!(".bund-edit-{}", uuid::Uuid::new_v4()));
            let fd = openat(
                &self.parent,
                &temporary,
                OFlag::O_WRONLY
                    | OFlag::O_CREAT
                    | OFlag::O_EXCL
                    | OFlag::O_NOFOLLOW
                    | OFlag::O_CLOEXEC,
                Mode::from_bits_truncate(mode as nix::libc::mode_t),
            )
            .map_err(io)?;
            let result = (|| -> Result<()> {
                let mut file = File::from(fd);
                file.write_all(content)?;
                file.set_permissions(std::fs::Permissions::from_mode(mode))?;
                file.sync_all()?;
                if self.read()?.as_deref() != expected {
                    return Err(Error::Invalid(
                        "file changed during write preparation".into(),
                    ));
                }
                renameat(&self.parent, &temporary, &self.parent, &self.name).map_err(io)?;
                Ok(())
            })();
            if result.is_err() {
                let _ = unlinkat(&self.parent, &temporary, UnlinkatFlags::NoRemoveDir);
            }
            result?;
        } else if expected.is_some() {
            unlinkat(&self.parent, &self.name, UnlinkatFlags::NoRemoveDir).map_err(io)?;
        }
        // Persist the directory entry as well as the file bytes.
        if unsafe { nix::libc::fsync(self.parent.as_raw_fd()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}

pub(crate) fn read(context: &ToolContext, path: &Path) -> Result<Option<Vec<u8>>> {
    Entry::new(context, path, false)?.read()
}

/// Enumeration never follows symlinks and excludes Git/build internals.
pub(crate) fn list(context: &ToolContext, base: &Path, limit: usize) -> Result<Vec<PathBuf>> {
    let mut pending = vec![base.to_path_buf()];
    let mut files = Vec::new();
    while let Some(relative_dir) = pending.pop() {
        let mut directory =
            nix::dir::Dir::from_fd(directory(context, &relative_dir)?).map_err(io)?;
        let mut entries = directory
            .iter()
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(io)?;
        entries.sort_by(|a, b| a.file_name().cmp(b.file_name()));
        for entry in entries {
            let name = std::ffi::OsStr::from_bytes(entry.file_name().to_bytes());
            if matches!(
                name.to_str(),
                Some("." | ".." | ".git" | "target" | "node_modules")
            ) {
                continue;
            }
            let stat =
                fstatat(&directory, entry.file_name(), AtFlags::AT_SYMLINK_NOFOLLOW).map_err(io)?;
            let kind = SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT;
            let path = relative_dir.join(name);
            if kind == SFlag::S_IFDIR {
                pending.push(path);
            } else if kind == SFlag::S_IFREG {
                files.push(path);
                if files.len() >= limit {
                    files.sort();
                    return Ok(files);
                }
            }
        }
    }
    files.sort();
    Ok(files)
}
