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
        const PF_EXITING: u64 = 0x0000_0004;
        let invalid_stat = || io::Error::new(io::ErrorKind::InvalidData, "invalid thread stat");
        let mut threads_by_name = BTreeMap::new();
        for entry in std::fs::read_dir("/proc/self/task")? {
            let stat = match std::fs::read_to_string(entry?.path().join("stat")) {
                Ok(stat) => stat,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
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
        let file_descriptors = std::fs::read_dir("/proc/self/fd")?
            .collect::<io::Result<Vec<_>>>()?
            .len();
        Ok(Self {
            threads_by_name,
            file_descriptors,
        })
    }
}
