//! Transparent VFS forwarding. Each SQLite-owned file allocation contains a
//! wrapper followed by the underlying VFS's aligned file storage. All methods
//! receive the original VFS/file pointer; only successful xWrite is observed.
#![expect(
    unsafe_code,
    reason = "SQLite VFS callbacks borrow SQLite-owned file buffers and forward to the original VFS"
)]
use super::{State, WriteKind};
use rusqlite::ffi;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::{Arc, Mutex, OnceLock};
pub(super) const NAME: &str = "lash-write-observer";

#[repr(C)]
struct File {
    base: ffi::sqlite3_file,
    real: *mut ffi::sqlite3_file,
    context: *mut Context,
}
struct Context {
    state: Arc<Mutex<State>>,
    database: String,
    flags: c_int,
    checkpoint: bool,
    methods: Box<ffi::sqlite3_io_methods>,
}
static REGISTERED: OnceLock<Result<(), &'static str>> = OnceLock::new();
pub(super) fn register() -> Result<(), &'static str> {
    *REGISTERED.get_or_init(|| unsafe {
        ffi::sqlite3_initialize();
        let base = ffi::sqlite3_vfs_find(std::ptr::null());
        if base.is_null() {
            return Err("SQLite has no default VFS");
        }
        let mut vfs = Box::new(*base);
        vfs.zName = c"lash-write-observer".as_ptr();
        vfs.pAppData = base.cast();
        vfs.szOsFile += std::mem::size_of::<File>() as c_int;
        vfs.xOpen = Some(open);
        vfs.xDelete = vfs.xDelete.map(|_| delete as _);
        vfs.xAccess = vfs.xAccess.map(|_| access as _);
        vfs.xFullPathname = vfs.xFullPathname.map(|_| full_path as _);
        vfs.xDlOpen = vfs.xDlOpen.map(|_| dl_open as _);
        vfs.xDlError = vfs.xDlError.map(|_| dl_error as _);
        vfs.xDlSym = vfs.xDlSym.map(|_| dl_sym as _);
        vfs.xDlClose = vfs.xDlClose.map(|_| dl_close as _);
        vfs.xRandomness = vfs.xRandomness.map(|_| randomness as _);
        vfs.xSleep = vfs.xSleep.map(|_| sleep as _);
        vfs.xCurrentTime = vfs.xCurrentTime.map(|_| current_time as _);
        vfs.xGetLastError = vfs.xGetLastError.map(|_| last_error as _);
        vfs.xCurrentTimeInt64 = vfs.xCurrentTimeInt64.map(|_| current_time_int64 as _);
        vfs.xSetSystemCall = vfs.xSetSystemCall.map(|_| set_system_call as _);
        vfs.xGetSystemCall = vfs.xGetSystemCall.map(|_| get_system_call as _);
        vfs.xNextSystemCall = vfs.xNextSystemCall.map(|_| next_system_call as _);
        // SQLite retains the registered descriptor for the process lifetime.
        let raw = Box::into_raw(vfs);
        if ffi::sqlite3_vfs_register(raw, 0) != ffi::SQLITE_OK {
            drop(Box::from_raw(raw));
            return Err("could not register SQLite write observer VFS");
        }
        Ok(())
    })
}
unsafe extern "C" fn delete(vfs: *mut ffi::sqlite3_vfs, name: *const c_char, sync: c_int) -> c_int {
    unsafe {
        let base = (*vfs).pAppData.cast::<ffi::sqlite3_vfs>();
        (*base)
            .xDelete
            .map_or(ffi::SQLITE_IOERR, |callback| callback(base, name, sync))
    }
}
unsafe extern "C" fn access(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    unsafe {
        let base = (*vfs).pAppData.cast::<ffi::sqlite3_vfs>();
        (*base).xAccess.map_or(ffi::SQLITE_IOERR, |callback| {
            callback(base, name, flags, out)
        })
    }
}
unsafe extern "C" fn full_path(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    len: c_int,
    out: *mut c_char,
) -> c_int {
    unsafe {
        let base = (*vfs).pAppData.cast::<ffi::sqlite3_vfs>();
        (*base)
            .xFullPathname
            .map_or(ffi::SQLITE_IOERR, |callback| callback(base, name, len, out))
    }
}
unsafe extern "C" fn dl_open(vfs: *mut ffi::sqlite3_vfs, name: *const c_char) -> *mut c_void {
    unsafe {
        let base = (*vfs).pAppData.cast::<ffi::sqlite3_vfs>();
        (*base)
            .xDlOpen
            .map_or(std::ptr::null_mut(), |callback| callback(base, name))
    }
}
unsafe extern "C" fn dl_error(vfs: *mut ffi::sqlite3_vfs, len: c_int, out: *mut c_char) {
    unsafe {
        let base = (*vfs).pAppData.cast::<ffi::sqlite3_vfs>();
        (*base)
            .xDlError
            .map_or((), |callback| callback(base, len, out))
    }
}
unsafe extern "C" fn dl_sym(
    vfs: *mut ffi::sqlite3_vfs,
    handle: *mut c_void,
    name: *const c_char,
) -> Option<unsafe extern "C" fn(*mut ffi::sqlite3_vfs, *mut c_void, *const c_char)> {
    unsafe {
        let base = (*vfs).pAppData.cast::<ffi::sqlite3_vfs>();
        (*base)
            .xDlSym
            .and_then(|callback| callback(base, handle, name))
    }
}
unsafe extern "C" fn dl_close(vfs: *mut ffi::sqlite3_vfs, handle: *mut c_void) {
    unsafe {
        let base = (*vfs).pAppData.cast::<ffi::sqlite3_vfs>();
        (*base)
            .xDlClose
            .map_or((), |callback| callback(base, handle))
    }
}
unsafe extern "C" fn randomness(vfs: *mut ffi::sqlite3_vfs, len: c_int, out: *mut c_char) -> c_int {
    unsafe {
        let base = (*vfs).pAppData.cast::<ffi::sqlite3_vfs>();
        (*base)
            .xRandomness
            .map_or(ffi::SQLITE_IOERR, |callback| callback(base, len, out))
    }
}
unsafe extern "C" fn sleep(vfs: *mut ffi::sqlite3_vfs, micros: c_int) -> c_int {
    unsafe {
        let base = (*vfs).pAppData.cast::<ffi::sqlite3_vfs>();
        (*base)
            .xSleep
            .map_or(ffi::SQLITE_IOERR, |callback| callback(base, micros))
    }
}
unsafe extern "C" fn current_time(vfs: *mut ffi::sqlite3_vfs, out: *mut f64) -> c_int {
    unsafe {
        let base = (*vfs).pAppData.cast::<ffi::sqlite3_vfs>();
        (*base)
            .xCurrentTime
            .map_or(ffi::SQLITE_IOERR, |callback| callback(base, out))
    }
}
unsafe extern "C" fn last_error(vfs: *mut ffi::sqlite3_vfs, len: c_int, out: *mut c_char) -> c_int {
    unsafe {
        let base = (*vfs).pAppData.cast::<ffi::sqlite3_vfs>();
        (*base)
            .xGetLastError
            .map_or(ffi::SQLITE_IOERR, |callback| callback(base, len, out))
    }
}
unsafe extern "C" fn current_time_int64(vfs: *mut ffi::sqlite3_vfs, out: *mut i64) -> c_int {
    unsafe {
        let base = (*vfs).pAppData.cast::<ffi::sqlite3_vfs>();
        (*base)
            .xCurrentTimeInt64
            .map_or(ffi::SQLITE_IOERR, |callback| callback(base, out))
    }
}
unsafe extern "C" fn set_system_call(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    call: ffi::sqlite3_syscall_ptr,
) -> c_int {
    unsafe {
        let base = (*vfs).pAppData.cast::<ffi::sqlite3_vfs>();
        (*base)
            .xSetSystemCall
            .map_or(ffi::SQLITE_IOERR, |callback| callback(base, name, call))
    }
}
unsafe extern "C" fn get_system_call(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
) -> ffi::sqlite3_syscall_ptr {
    unsafe {
        let base = (*vfs).pAppData.cast::<ffi::sqlite3_vfs>();
        (*base)
            .xGetSystemCall
            .and_then(|callback| callback(base, name))
    }
}
unsafe extern "C" fn next_system_call(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
) -> *const c_char {
    unsafe {
        let base = (*vfs).pAppData.cast::<ffi::sqlite3_vfs>();
        (*base)
            .xNextSystemCall
            .map_or(std::ptr::null(), |callback| callback(base, name))
    }
}
unsafe extern "C" fn open(
    vfs: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    file: *mut ffi::sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    unsafe {
        let base = (*vfs).pAppData.cast::<ffi::sqlite3_vfs>();
        let real = file
            .cast::<u8>()
            .add(std::mem::size_of::<File>())
            .cast::<ffi::sqlite3_file>();
        let result = (*base).xOpen.map_or(ffi::SQLITE_CANTOPEN, |callback| {
            callback(base, name, real, flags, out)
        });
        if result != ffi::SQLITE_OK {
            // SQLite may call xClose even when xOpen failed.
            if !(*real).pMethods.is_null() {
                close_real(real);
            }
            (*file).pMethods = std::ptr::null();
            return result;
        }
        let state = super::CURRENT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(state) = state else {
            close_real(real);
            (*file).pMethods = std::ptr::null();
            return ffi::SQLITE_CANTOPEN;
        };
        let mut methods = Box::new(*(*real).pMethods);
        methods.xClose = Some(close);
        methods.xWrite = Some(write);
        methods.xFileControl = Some(file_control);
        methods.xRead = methods.xRead.map(|_| read as _);
        methods.xTruncate = methods.xTruncate.map(|_| truncate as _);
        methods.xSync = methods.xSync.map(|_| sync as _);
        methods.xFileSize = methods.xFileSize.map(|_| file_size as _);
        methods.xLock = methods.xLock.map(|_| lock as _);
        methods.xUnlock = methods.xUnlock.map(|_| unlock as _);
        methods.xCheckReservedLock = methods.xCheckReservedLock.map(|_| check_reserved as _);
        methods.xSectorSize = methods.xSectorSize.map(|_| sector_size as _);
        methods.xDeviceCharacteristics = methods
            .xDeviceCharacteristics
            .map(|_| device_characteristics as _);
        methods.xShmMap = methods.xShmMap.map(|_| shm_map as _);
        methods.xShmLock = methods.xShmLock.map(|_| shm_lock as _);
        methods.xShmBarrier = methods.xShmBarrier.map(|_| shm_barrier as _);
        methods.xShmUnmap = methods.xShmUnmap.map(|_| shm_unmap as _);
        methods.xFetch = methods.xFetch.map(|_| fetch as _);
        methods.xUnfetch = methods.xUnfetch.map(|_| unfetch as _);
        let database = if name.is_null() {
            "<temporary>".into()
        } else {
            CStr::from_ptr(name).to_string_lossy().into_owned()
        };
        let database = database
            .strip_suffix("-wal")
            .unwrap_or(&database)
            .to_owned();
        let context = Box::new(Context {
            state,
            database,
            flags,
            checkpoint: false,
            methods,
        });
        file.cast::<File>().write(File {
            base: ffi::sqlite3_file {
                pMethods: &*context.methods,
            },
            real,
            context: Box::into_raw(context),
        });
        ffi::SQLITE_OK
    }
}
unsafe extern "C" fn close(file: *mut ffi::sqlite3_file) -> c_int {
    unsafe {
        let file = &mut *file.cast::<File>();
        let result = close_real(file.real);
        drop(Box::from_raw(file.context));
        file.base.pMethods = std::ptr::null();
        result
    }
}
unsafe extern "C" fn file_control(
    file: *mut ffi::sqlite3_file,
    op: c_int,
    arg: *mut c_void,
) -> c_int {
    unsafe {
        let file = &mut *file.cast::<File>();
        let context = &mut *file.context;
        if op == ffi::SQLITE_FCNTL_CKPT_START {
            context.checkpoint = true;
        }
        if op == ffi::SQLITE_FCNTL_CKPT_DONE {
            context.checkpoint = false;
        }
        (*(*file.real).pMethods)
            .xFileControl
            .map_or(ffi::SQLITE_NOTFOUND, |callback| {
                callback(file.real, op, arg)
            })
    }
}
unsafe extern "C" fn read(
    file: *mut ffi::sqlite3_file,
    buf: *mut c_void,
    len: c_int,
    offset: i64,
) -> c_int {
    unsafe {
        let real = (*file.cast::<File>()).real;
        (*(*real).pMethods)
            .xRead
            .map_or(ffi::SQLITE_IOERR, |callback| {
                callback(real, buf, len, offset)
            })
    }
}
unsafe extern "C" fn truncate(file: *mut ffi::sqlite3_file, len: i64) -> c_int {
    unsafe {
        let real = (*file.cast::<File>()).real;
        (*(*real).pMethods)
            .xTruncate
            .map_or(ffi::SQLITE_IOERR, |callback| callback(real, len))
    }
}
unsafe extern "C" fn sync(file: *mut ffi::sqlite3_file, flags: c_int) -> c_int {
    unsafe {
        let real = (*file.cast::<File>()).real;
        (*(*real).pMethods)
            .xSync
            .map_or(ffi::SQLITE_IOERR, |callback| callback(real, flags))
    }
}
unsafe extern "C" fn file_size(file: *mut ffi::sqlite3_file, out: *mut i64) -> c_int {
    unsafe {
        let real = (*file.cast::<File>()).real;
        (*(*real).pMethods)
            .xFileSize
            .map_or(ffi::SQLITE_IOERR, |callback| callback(real, out))
    }
}
unsafe extern "C" fn lock(file: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    unsafe {
        let real = (*file.cast::<File>()).real;
        (*(*real).pMethods)
            .xLock
            .map_or(ffi::SQLITE_IOERR, |callback| callback(real, level))
    }
}
unsafe extern "C" fn unlock(file: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    unsafe {
        let real = (*file.cast::<File>()).real;
        (*(*real).pMethods)
            .xUnlock
            .map_or(ffi::SQLITE_IOERR, |callback| callback(real, level))
    }
}
unsafe extern "C" fn check_reserved(file: *mut ffi::sqlite3_file, out: *mut c_int) -> c_int {
    unsafe {
        let real = (*file.cast::<File>()).real;
        (*(*real).pMethods)
            .xCheckReservedLock
            .map_or(ffi::SQLITE_IOERR, |callback| callback(real, out))
    }
}
unsafe extern "C" fn sector_size(file: *mut ffi::sqlite3_file) -> c_int {
    unsafe {
        let real = (*file.cast::<File>()).real;
        (*(*real).pMethods)
            .xSectorSize
            .map_or(ffi::SQLITE_IOERR, |callback| callback(real))
    }
}
unsafe extern "C" fn device_characteristics(file: *mut ffi::sqlite3_file) -> c_int {
    unsafe {
        let real = (*file.cast::<File>()).real;
        (*(*real).pMethods)
            .xDeviceCharacteristics
            .map_or(ffi::SQLITE_IOERR, |callback| callback(real))
    }
}
unsafe extern "C" fn shm_map(
    file: *mut ffi::sqlite3_file,
    page: c_int,
    size: c_int,
    extend: c_int,
    out: *mut *mut c_void,
) -> c_int {
    unsafe {
        let real = (*file.cast::<File>()).real;
        (*(*real).pMethods)
            .xShmMap
            .map_or(ffi::SQLITE_IOERR, |callback| {
                callback(real, page, size, extend, out)
            })
    }
}
unsafe extern "C" fn shm_lock(
    file: *mut ffi::sqlite3_file,
    offset: c_int,
    len: c_int,
    flags: c_int,
) -> c_int {
    unsafe {
        let real = (*file.cast::<File>()).real;
        (*(*real).pMethods)
            .xShmLock
            .map_or(ffi::SQLITE_IOERR, |callback| {
                callback(real, offset, len, flags)
            })
    }
}
unsafe extern "C" fn shm_barrier(file: *mut ffi::sqlite3_file) {
    unsafe {
        let real = (*file.cast::<File>()).real;
        (*(*real).pMethods)
            .xShmBarrier
            .map_or((), |callback| callback(real))
    }
}
unsafe extern "C" fn shm_unmap(file: *mut ffi::sqlite3_file, delete: c_int) -> c_int {
    unsafe {
        let real = (*file.cast::<File>()).real;
        (*(*real).pMethods)
            .xShmUnmap
            .map_or(ffi::SQLITE_IOERR, |callback| callback(real, delete))
    }
}
unsafe extern "C" fn fetch(
    file: *mut ffi::sqlite3_file,
    offset: i64,
    len: c_int,
    out: *mut *mut c_void,
) -> c_int {
    unsafe {
        let real = (*file.cast::<File>()).real;
        (*(*real).pMethods)
            .xFetch
            .map_or(ffi::SQLITE_IOERR, |callback| {
                callback(real, offset, len, out)
            })
    }
}
unsafe extern "C" fn unfetch(file: *mut ffi::sqlite3_file, offset: i64, buf: *mut c_void) -> c_int {
    unsafe {
        let real = (*file.cast::<File>()).real;
        (*(*real).pMethods)
            .xUnfetch
            .map_or(ffi::SQLITE_IOERR, |callback| callback(real, offset, buf))
    }
}
unsafe extern "C" fn write(
    file: *mut ffi::sqlite3_file,
    buf: *const c_void,
    len: c_int,
    offset: i64,
) -> c_int {
    unsafe {
        let file = &mut *file.cast::<File>();
        let result = (*(*file.real).pMethods)
            .xWrite
            .map_or(ffi::SQLITE_IOERR_WRITE, |callback| {
                callback(file.real, buf, len, offset)
            });
        let context = &*file.context;
        // Never unwind into SQLite, and never change the underlying I/O result.
        let recorded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut state = context
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if result != ffi::SQLITE_OK {
                state.failures += 1;
                return;
            }
            state.calls += 1;
            state.bytes += len as u64;
            let start = offset as u64;
            let end = start + len as u64;
            if context.flags & ffi::SQLITE_OPEN_WAL != 0 {
                observe_wal(file.real, context, &mut state, start, end);
            } else if context.flags & ffi::SQLITE_OPEN_MAIN_DB != 0 {
                let mut header = [0_u8; 2];
                let page_size = if read_at(file.real, 16, &mut header) {
                    let size = u16::from_be_bytes(header);
                    if size == 1 { 65536 } else { u64::from(size) }
                } else {
                    0
                };
                if page_size == 0 {
                    state.unknown += len as u64;
                    return;
                }
                let kind = if context.checkpoint {
                    WriteKind::Checkpoint
                } else {
                    WriteKind::Database
                };
                let mut at = start;
                while at < end {
                    let page = at / page_size + 1;
                    let next = end.min(page * page_size);
                    state.record(
                        &context.database,
                        u32::try_from(page).ok(),
                        kind.clone(),
                        next - at,
                    );
                    at = next;
                }
            } else {
                state.record(&context.database, None, WriteKind::Other, len as u64);
            }
        }));
        if recorded.is_err() {
            context
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .failures += 1;
        }
        result
    }
}
unsafe fn close_real(file: *mut ffi::sqlite3_file) -> c_int {
    unsafe {
        (*(*file).pMethods)
            .xClose
            .map_or(ffi::SQLITE_IOERR_CLOSE, |callback| callback(file))
    }
}
unsafe fn read_at(file: *mut ffi::sqlite3_file, offset: u64, buf: &mut [u8]) -> bool {
    unsafe {
        (*(*file).pMethods).xRead.is_some_and(|callback| {
            callback(
                file,
                buf.as_mut_ptr().cast(),
                buf.len() as c_int,
                offset as i64,
            ) == ffi::SQLITE_OK
        })
    }
}
unsafe fn observe_wal(
    file: *mut ffi::sqlite3_file,
    context: &Context,
    state: &mut State,
    mut at: u64,
    end: u64,
) {
    unsafe {
        if at < 32 {
            let next = end.min(32);
            state.record(&context.database, None, WriteKind::WalHeader, next - at);
            at = next;
        }
        if at == end {
            return;
        }
        let mut size = [0_u8; 4];
        if !read_at(file, 8, &mut size) {
            state.unknown += end - at;
            return;
        }
        let page_size = u64::from(u32::from_be_bytes(size));
        if !(512..=65536).contains(&page_size) || !page_size.is_power_of_two() {
            state.unknown += end - at;
            return;
        }
        while at < end {
            let frame_start = 32 + ((at - 32) / (24 + page_size)) * (24 + page_size);
            let mut page = [0_u8; 4];
            if !read_at(file, frame_start, &mut page) {
                state.unknown += end - at;
                return;
            }
            let page = u32::from_be_bytes(page);
            if page == 0 {
                state.unknown += end - at;
                return;
            }
            let header = at < frame_start + 24;
            let next = end.min(frame_start + if header { 24 } else { 24 + page_size });
            state.record(
                &context.database,
                Some(page),
                if header {
                    WriteKind::WalFrameHeader
                } else {
                    WriteKind::WalBody
                },
                next - at,
            );
            at = next;
        }
    }
}
