//! Opt-in host diagnostics; never installed by the library or worker.
use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[expect(
    unsafe_code,
    reason = "glibc and SQLite expose allocator diagnostics through C FFI only"
)]
pub(super) fn snapshot(dir: &Path, phase: &str, done: usize) -> Result<Value> {
    use anyhow::ensure;
    use rusqlite::ffi;
    use std::os::fd::IntoRawFd as _;

    let stem = dir.join(format!("{}-{phase}-{done}", std::process::id()));
    // Take counters before allocating the /proc text or opening the scanner's
    // SQLite connection. These are adjacent observations, not an atomic census.
    // SAFETY: mallinfo2 has no arguments or ownership requirements.
    let info = unsafe { libc::mallinfo2() };
    let sqlite_status = |code| -> Result<Value> {
        let (mut current, mut highwater) = (0, 0);
        // SAFETY: outputs point to live i64s; resetFlag=0 preserves highwater.
        let rc = unsafe { ffi::sqlite3_status64(code, &mut current, &mut highwater, 0) };
        ensure!(rc == ffi::SQLITE_OK, "sqlite3_status64 failed: {rc}");
        Ok(json!({"current":current,"highwater":highwater}))
    };
    let sqlite = json!({
        "memory_bytes":sqlite_status(ffi::SQLITE_STATUS_MEMORY_USED)?,
        "pagecache_overflow_bytes":sqlite_status(ffi::SQLITE_STATUS_PAGECACHE_OVERFLOW)?,
        "pagecache_pool_slots":sqlite_status(ffi::SQLITE_STATUS_PAGECACHE_USED)?,
    });
    let file = std::fs::File::create(stem.with_extension("malloc.xml"))?;
    let fd = file.into_raw_fd();
    // SAFETY: fd is owned, open for writing, and mode is a terminated C string.
    let stream = unsafe { libc::fdopen(fd, c"w".as_ptr()) };
    if stream.is_null() {
        // SAFETY: fdopen failed, so ownership of fd did not transfer.
        unsafe { libc::close(fd) };
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: stream remains a valid owned FILE until fclose, on every path.
    let rc = unsafe { libc::malloc_info(0, stream) };
    let close_rc = unsafe { libc::fclose(stream) };
    ensure!(
        rc == 0 && close_rc == 0,
        "malloc_info/fclose failed: {rc}/{close_rc}"
    );
    let xml = std::fs::read_to_string(stem.with_extension("malloc.xml"))?;
    let arenas = xml.matches("<heap nr=").count();
    let rollup = std::fs::read_to_string("/proc/self/smaps_rollup")?;
    std::fs::write(stem.with_extension("rollup"), &rollup)?;
    let smaps = std::fs::read_to_string("/proc/self/smaps")?;
    std::fs::write(stem.with_extension("smaps"), &smaps)?;
    let status = std::fs::read_to_string("/proc/self/status")?;
    let threads = status
        .lines()
        .find_map(|line| line.strip_prefix("Threads:"))
        .and_then(|s| s.trim().parse::<usize>().ok());
    Ok(json!({
        "snapshot_stem":stem,
        "glibc":{"arena_bytes":info.arena,"free_bytes":info.fordblks,
            "in_use_bytes":info.uordblks,"mmap_bytes":info.hblkhd,
            "fastbin_bytes":info.fsmblks,"fastbin_blocks":info.smblks,
            "free_chunks":info.ordblks,"main_arena_top_bytes":info.keepcost,
            "arenas":arenas,"threads_times_arenas":threads.map(|n|n*arenas)},
        "sqlite":sqlite,
        "jemalloc":jemalloc()?,
        "scope":"host only; glibc stats describe glibc only under allocator interposition; free_bytes is virtual, not resident; raw smaps supplies mappings"
    }))
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[expect(
    unsafe_code,
    reason = "optional jemalloc diagnostics resolve the documented mallctl ABI"
)]
fn jemalloc() -> Result<Option<Value>> {
    use anyhow::ensure;
    type Mallctl = unsafe extern "C" fn(
        *const libc::c_char,
        *mut libc::c_void,
        *mut usize,
        *mut libc::c_void,
        usize,
    ) -> libc::c_int;
    // SAFETY: RTLD_DEFAULT searches already loaded objects; the name is terminated.
    let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"mallctl".as_ptr()) };
    if symbol.is_null() {
        return Ok(None);
    }
    // SAFETY: a host choosing jemalloc provides mallctl with this documented ABI.
    let mallctl: Mallctl = unsafe { std::mem::transmute(symbol) };
    let mut epoch = 1u64;
    // SAFETY: epoch is an initialized u64, and no old-value output is requested.
    let rc = unsafe {
        mallctl(
            c"epoch".as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            (&mut epoch as *mut u64).cast(),
            std::mem::size_of::<u64>(),
        )
    };
    ensure!(rc == 0, "jemalloc epoch failed: {rc}");
    let mut stats = serde_json::Map::new();
    for name in [
        c"stats.allocated",
        c"stats.active",
        c"stats.resident",
        c"stats.metadata",
        c"stats.mapped",
        c"stats.retained",
    ] {
        let mut value = 0usize;
        let mut len = std::mem::size_of::<usize>();
        // SAFETY: all these mallctl read-only values have the size_t ABI.
        let rc = unsafe {
            mallctl(
                name.as_ptr(),
                (&mut value as *mut usize).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        ensure!(rc == 0, "jemalloc {} failed: {rc}", name.to_string_lossy());
        stats.insert(name.to_string_lossy().into_owned(), json!(value));
    }
    Ok(Some(Value::Object(stats)))
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[expect(
    unsafe_code,
    reason = "the host explicitly selects the glibc idle-trim experiment"
)]
pub(super) fn trim() -> Result<()> {
    // SAFETY: malloc_trim accepts zero padding and frees only unused pages.
    // Its zero return means no pages released, not an error. It is process-wide.
    unsafe { libc::malloc_trim(0) };
    Ok(())
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub(super) fn snapshot(_: &Path, _: &str, _: usize) -> Result<Value> {
    anyhow::bail!("native-memory diagnostics require Linux/glibc")
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub(super) fn trim() -> Result<()> {
    anyhow::bail!("idle trimming requires Linux/glibc")
}
