use std::collections::BTreeMap;
use std::io;

/// Linux process resources for a capacity law in its own test binary.
/// Capture before creating a runtime and after dropping it; concurrent laws
/// in the same process would make the comparison meaningless.
#[derive(Debug, PartialEq, Eq)]
pub struct ThreadCensus {
    /// Live native threads grouped by their kernel thread name.
    pub threads_by_name: BTreeMap<String, usize>,
    /// Open file descriptors, including the census directory descriptor.
    pub file_descriptors: usize,
}

impl ThreadCensus {
    /// Capture once, without waiting or polling for resources to disappear.
    ///
    /// Linux excludes `PF_EXITING` from live threads. A joined task may
    /// retain its proc directory during kernel exit cleanup, so read its
    /// name and flags together from `stat` and exclude only exiting tasks.
    ///
    /// # Errors
    /// Returns an error when proc resources cannot be read or a thread's
    /// status is malformed. A task removed during capture is skipped.
    #[allow(
        clippy::disallowed_methods,
        reason = "Linux process resource census requires proc filesystem reads"
    )]
    pub fn capture() -> io::Result<Self> {
        let mut stats = Vec::new();
        for entry in std::fs::read_dir("/proc/self/task")? {
            stats.push(std::fs::read_to_string(entry?.path().join("stat")));
        }
        let file_descriptors = std::fs::read_dir("/proc/self/fd")?
            .collect::<io::Result<Vec<_>>>()?
            .len();
        Ok(Self {
            threads_by_name: threads_by_name(stats)?,
            file_descriptors,
        })
    }
}

/// Count the live threads behind each `stat` read, grouped by kernel thread
/// name. A thread that exits between the task listing and its own read is
/// absent from the count: `ENOENT` when its directory vanished, `ESRCH` when
/// the read lost the race with exit cleanup. Any other error fails.
fn threads_by_name(
    stats: impl IntoIterator<Item = io::Result<String>>,
) -> io::Result<BTreeMap<String, usize>> {
    const PF_EXITING: u64 = 0x0000_0004;
    let invalid_stat = || io::Error::new(io::ErrorKind::InvalidData, "invalid thread stat");
    let mut threads_by_name = BTreeMap::new();
    for stat in stats {
        let stat = match stat {
            Ok(stat) => stat,
            Err(error) if exited(&error) => continue,
            Err(error) => return Err(error),
        };
        let (_, stat) = stat.split_once('(').ok_or_else(invalid_stat)?;
        let (name, fields) = stat.rsplit_once(") ").ok_or_else(invalid_stat)?;
        let flags = fields
            .split_whitespace()
            .nth(6)
            .ok_or_else(invalid_stat)?
            .parse::<u64>()
            .map_err(|_| invalid_stat())?;
        if flags & PF_EXITING == 0 {
            *threads_by_name.entry(name.to_owned()).or_insert(0) += 1;
        }
    }
    Ok(threads_by_name)
}

/// Whether a per-thread `stat` read failed because the thread exited
/// underneath the census. `ESRCH` has no dedicated [`io::ErrorKind`].
fn exited(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(name: &str, flags: u64) -> io::Result<String> {
        Ok(format!("1 ({name}) S 0 0 0 0 0 {flags}"))
    }

    #[test]
    fn a_thread_that_exits_mid_read_is_not_counted() {
        let counted = threads_by_name(vec![
            stat("main", 0),
            Err(io::Error::from_raw_os_error(libc::ESRCH)),
            Err(io::Error::from_raw_os_error(libc::ENOENT)),
            stat("worker", 0),
            stat("worker", 0),
        ])
        .expect("threads that exited mid-read are skipped");
        assert_eq!(
            counted,
            BTreeMap::from([("main".to_owned(), 1), ("worker".to_owned(), 2)])
        );
    }

    #[test]
    fn any_other_read_error_fails_the_census() {
        let error = threads_by_name(vec![
            stat("main", 0),
            Err(io::Error::from_raw_os_error(libc::EIO)),
        ])
        .expect_err("an unrelated read error fails the census");
        assert_eq!(error.raw_os_error(), Some(libc::EIO));
    }
}
