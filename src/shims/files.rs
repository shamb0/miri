use std::any::Any;
use std::collections::BTreeMap;
use std::io::{IsTerminal, Read, SeekFrom, Write};
use std::marker::CoercePointee;
use std::ops::Deref;
use std::rc::{Rc, Weak};
use std::{fs, io};

use rustc_abi::Size;

use crate::shims::unix::UnixFileDescription;
use crate::*;

/// A unique id for file descriptions. While we could use the address, considering that
/// is definitely unique, the address would expose interpreter internal state when used
/// for sorting things. So instead we generate a unique id per file description is the name
/// for all `dup`licates and is never reused.
#[derive(Debug, Copy, Clone, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct FdId(usize);

#[derive(Debug, Clone)]
struct FdIdWith<T: ?Sized> {
    id: FdId,
    inner: T,
}

/// A refcounted pointer to a file description, also tracking the
/// globally unique ID of this file description.
#[repr(transparent)]
#[derive(CoercePointee, Debug)]
pub struct FileDescriptionRef<T: ?Sized>(Rc<FdIdWith<T>>);

impl<T: ?Sized> Clone for FileDescriptionRef<T> {
    fn clone(&self) -> Self {
        FileDescriptionRef(self.0.clone())
    }
}

impl<T: ?Sized> Deref for FileDescriptionRef<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0.inner
    }
}

impl<T: ?Sized> FileDescriptionRef<T> {
    pub fn id(&self) -> FdId {
        self.0.id
    }
}

/// Holds a weak reference to the actual file description.
#[derive(Debug)]
pub struct WeakFileDescriptionRef<T: ?Sized>(Weak<FdIdWith<T>>);

impl<T: ?Sized> Clone for WeakFileDescriptionRef<T> {
    fn clone(&self) -> Self {
        WeakFileDescriptionRef(self.0.clone())
    }
}

impl<T: ?Sized> FileDescriptionRef<T> {
    pub fn downgrade(this: &Self) -> WeakFileDescriptionRef<T> {
        WeakFileDescriptionRef(Rc::downgrade(&this.0))
    }
}

impl<T: ?Sized> WeakFileDescriptionRef<T> {
    pub fn upgrade(&self) -> Option<FileDescriptionRef<T>> {
        self.0.upgrade().map(FileDescriptionRef)
    }
}

impl<T> VisitProvenance for WeakFileDescriptionRef<T> {
    fn visit_provenance(&self, _visit: &mut VisitWith<'_>) {
        // A weak reference can never be the only reference to some pointer or place.
        // Since the actual file description is tracked by strong ref somewhere,
        // it is ok to make this a NOP operation.
    }
}

/// A helper trait to indirectly allow downcasting on `Rc<FdIdWith<dyn _>>`.
/// Ideally we'd just add a `FdIdWith<Self>: Any` bound to the `FileDescription` trait,
/// but that does not allow upcasting.
pub trait FileDescriptionExt: 'static {
    fn into_rc_any(self: FileDescriptionRef<Self>) -> Rc<dyn Any>;

    /// We wrap the regular `close` function generically, so both handle `Rc::into_inner`
    /// and epoll interest management.
    fn close_ref<'tcx>(
        self: FileDescriptionRef<Self>,
        communicate_allowed: bool,
        ecx: &mut MiriInterpCx<'tcx>,
    ) -> InterpResult<'tcx, io::Result<()>>;
}

impl<T: FileDescription + 'static> FileDescriptionExt for T {
    fn into_rc_any(self: FileDescriptionRef<Self>) -> Rc<dyn Any> {
        self.0
    }

    fn close_ref<'tcx>(
        self: FileDescriptionRef<Self>,
        communicate_allowed: bool,
        ecx: &mut MiriInterpCx<'tcx>,
    ) -> InterpResult<'tcx, io::Result<()>> {
        match Rc::into_inner(self.0) {
            Some(fd) => {
                // Remove entry from the global epoll_event_interest table.
                ecx.machine.epoll_interests.remove(fd.id);

                fd.inner.close(communicate_allowed, ecx)
            }
            None => {
                // Not the last reference.
                interp_ok(Ok(()))
            }
        }
    }
}

pub type DynFileDescriptionRef = FileDescriptionRef<dyn FileDescription>;

/// Represents a dynamic callback for file I/O operations that is invoked upon completion.
/// The callback receives either the number of bytes successfully read (u64) or an IoError.
pub type DynFileDescriptionCallback<'tcx> = DynMachineCallback<'tcx, Result<u64, IoError>>;

impl FileDescriptionRef<dyn FileDescription> {
    pub fn downcast<T: FileDescription + 'static>(self) -> Option<FileDescriptionRef<T>> {
        let inner = self.into_rc_any().downcast::<FdIdWith<T>>().ok()?;
        Some(FileDescriptionRef(inner))
    }
}

/// Represents an open file description.
pub trait FileDescription: std::fmt::Debug + FileDescriptionExt {
    fn name(&self) -> &'static str;

    /// Reads as much as possible into the given buffer `ptr`.
    /// `len` indicates how many bytes we should try to read.
    /// `finish` Callback to be invoked on operation completion with bytes read or error
    #[allow(dead_code)]
    fn read<'tcx>(
        self: FileDescriptionRef<Self>,
        _communicate_allowed: bool,
        _ptr: Pointer,
        _len: usize,
        _finish: DynFileDescriptionCallback<'tcx>,
        _ecx: &mut MiriInterpCx<'tcx>,
    ) -> InterpResult<'tcx> {
        throw_unsup_format!("cannot read from {}", self.name());
    }

    /// Writes as much as possible from the given buffer `ptr`.
    /// `len` indicates how many bytes we should try to write.
    /// `dest` is where the return value should be stored: number of bytes written, or `-1` in case of error.
    fn write<'tcx>(
        self: FileDescriptionRef<Self>,
        _communicate_allowed: bool,
        _ptr: Pointer,
        _len: usize,
        _dest: &MPlaceTy<'tcx>,
        _ecx: &mut MiriInterpCx<'tcx>,
    ) -> InterpResult<'tcx> {
        throw_unsup_format!("cannot write to {}", self.name());
    }

    /// Seeks to the given offset (which can be relative to the beginning, end, or current position).
    /// Returns the new position from the start of the stream.
    fn seek<'tcx>(
        &self,
        _communicate_allowed: bool,
        _offset: SeekFrom,
    ) -> InterpResult<'tcx, io::Result<u64>> {
        throw_unsup_format!("cannot seek on {}", self.name());
    }

    /// Close the file descriptor.
    fn close<'tcx>(
        self,
        _communicate_allowed: bool,
        _ecx: &mut MiriInterpCx<'tcx>,
    ) -> InterpResult<'tcx, io::Result<()>>
    where
        Self: Sized,
    {
        throw_unsup_format!("cannot close {}", self.name());
    }

    fn metadata<'tcx>(&self) -> InterpResult<'tcx, io::Result<fs::Metadata>> {
        throw_unsup_format!("obtaining metadata is only supported on file-backed file descriptors");
    }

    fn is_tty(&self, _communicate_allowed: bool) -> bool {
        // Most FDs are not tty's and the consequence of a wrong `false` are minor,
        // so we use a default impl here.
        false
    }

    fn as_unix(&self) -> &dyn UnixFileDescription {
        panic!("Not a unix file descriptor: {}", self.name());
    }
}

impl FileDescription for io::Stdin {
    fn name(&self) -> &'static str {
        "stdin"
    }

    fn read<'tcx>(
        self: FileDescriptionRef<Self>,
        communicate_allowed: bool,
        ptr: Pointer,
        len: usize,
        finish: DynFileDescriptionCallback<'tcx>,
        ecx: &mut MiriInterpCx<'tcx>,
    ) -> InterpResult<'tcx> {
        // First handle isolation mode check
        if !communicate_allowed {
            // We want isolation mode to be deterministic, so we have to disallow all reads, even stdin.
            helpers::isolation_abort_error("`read` from stdin")?;
        }

        let mut bytes = vec![0; len];

        match Read::read(&mut &*self, &mut bytes) {
            Ok(actual_read_size) => {
                // Write the successfully read bytes to the destination pointer
                ecx.write_bytes_ptr(ptr, bytes[..actual_read_size].iter().copied())?;

                let Ok(read_size) = u64::try_from(actual_read_size) else {
                    throw_unsup_format!(
                        "Read operation returned size {} which exceeds maximum allowed value",
                        actual_read_size
                    )
                };

                finish.call(ecx, Ok(read_size))
            }
            Err(e) => finish.call(ecx, Err(e.into())),
        }
    }

    fn is_tty(&self, communicate_allowed: bool) -> bool {
        communicate_allowed && self.is_terminal()
    }
}

impl FileDescription for io::Stdout {
    fn name(&self) -> &'static str {
        "stdout"
    }

    fn write<'tcx>(
        self: FileDescriptionRef<Self>,
        _communicate_allowed: bool,
        ptr: Pointer,
        len: usize,
        dest: &MPlaceTy<'tcx>,
        ecx: &mut MiriInterpCx<'tcx>,
    ) -> InterpResult<'tcx> {
        let bytes = ecx.read_bytes_ptr_strip_provenance(ptr, Size::from_bytes(len))?;
        // We allow writing to stderr even with isolation enabled.
        let result = Write::write(&mut &*self, bytes);
        // Stdout is buffered, flush to make sure it appears on the
        // screen.  This is the write() syscall of the interpreted
        // program, we want it to correspond to a write() syscall on
        // the host -- there is no good in adding extra buffering
        // here.
        io::stdout().flush().unwrap();
        match result {
            Ok(write_size) => ecx.return_write_success(write_size, dest),
            Err(e) => ecx.set_last_error_and_return(e, dest),
        }
    }

    fn is_tty(&self, communicate_allowed: bool) -> bool {
        communicate_allowed && self.is_terminal()
    }
}

impl FileDescription for io::Stderr {
    fn name(&self) -> &'static str {
        "stderr"
    }

    fn write<'tcx>(
        self: FileDescriptionRef<Self>,
        _communicate_allowed: bool,
        ptr: Pointer,
        len: usize,
        dest: &MPlaceTy<'tcx>,
        ecx: &mut MiriInterpCx<'tcx>,
    ) -> InterpResult<'tcx> {
        let bytes = ecx.read_bytes_ptr_strip_provenance(ptr, Size::from_bytes(len))?;
        // We allow writing to stderr even with isolation enabled.
        // No need to flush, stderr is not buffered.
        let result = Write::write(&mut &*self, bytes);
        match result {
            Ok(write_size) => ecx.return_write_success(write_size, dest),
            Err(e) => ecx.set_last_error_and_return(e, dest),
        }
    }

    fn is_tty(&self, communicate_allowed: bool) -> bool {
        communicate_allowed && self.is_terminal()
    }
}

/// Like /dev/null
#[derive(Debug)]
pub struct NullOutput;

impl FileDescription for NullOutput {
    fn name(&self) -> &'static str {
        "stderr and stdout"
    }

    fn write<'tcx>(
        self: FileDescriptionRef<Self>,
        _communicate_allowed: bool,
        _ptr: Pointer,
        len: usize,
        dest: &MPlaceTy<'tcx>,
        ecx: &mut MiriInterpCx<'tcx>,
    ) -> InterpResult<'tcx> {
        // We just don't write anything, but report to the user that we did.
        ecx.return_write_success(len, dest)
    }
}

/// The file descriptor table
#[derive(Debug)]
pub struct FdTable {
    pub fds: BTreeMap<i32, DynFileDescriptionRef>,
    /// Unique identifier for file description, used to differentiate between various file description.
    next_file_description_id: FdId,
}

impl VisitProvenance for FdTable {
    fn visit_provenance(&self, _visit: &mut VisitWith<'_>) {
        // All our FileDescription instances do not have any tags.
    }
}

impl FdTable {
    fn new() -> Self {
        FdTable { fds: BTreeMap::new(), next_file_description_id: FdId(0) }
    }
    pub(crate) fn init(mute_stdout_stderr: bool) -> FdTable {
        let mut fds = FdTable::new();
        fds.insert_new(io::stdin());
        if mute_stdout_stderr {
            assert_eq!(fds.insert_new(NullOutput), 1);
            assert_eq!(fds.insert_new(NullOutput), 2);
        } else {
            assert_eq!(fds.insert_new(io::stdout()), 1);
            assert_eq!(fds.insert_new(io::stderr()), 2);
        }
        fds
    }

    pub fn new_ref<T: FileDescription>(&mut self, fd: T) -> FileDescriptionRef<T> {
        let file_handle =
            FileDescriptionRef(Rc::new(FdIdWith { id: self.next_file_description_id, inner: fd }));
        self.next_file_description_id = FdId(self.next_file_description_id.0.strict_add(1));
        file_handle
    }

    /// Insert a new file description to the FdTable.
    pub fn insert_new(&mut self, fd: impl FileDescription) -> i32 {
        let fd_ref = self.new_ref(fd);
        self.insert(fd_ref)
    }

    pub fn insert(&mut self, fd_ref: DynFileDescriptionRef) -> i32 {
        self.insert_with_min_num(fd_ref, 0)
    }

    /// Insert a file description, giving it a file descriptor that is at least `min_fd_num`.
    pub fn insert_with_min_num(
        &mut self,
        file_handle: DynFileDescriptionRef,
        min_fd_num: i32,
    ) -> i32 {
        // Find the lowest unused FD, starting from min_fd. If the first such unused FD is in
        // between used FDs, the find_map combinator will return it. If the first such unused FD
        // is after all other used FDs, the find_map combinator will return None, and we will use
        // the FD following the greatest FD thus far.
        let candidate_new_fd =
            self.fds.range(min_fd_num..).zip(min_fd_num..).find_map(|((fd_num, _fd), counter)| {
                if *fd_num != counter {
                    // There was a gap in the fds stored, return the first unused one
                    // (note that this relies on BTreeMap iterating in key order)
                    Some(counter)
                } else {
                    // This fd is used, keep going
                    None
                }
            });
        let new_fd_num = candidate_new_fd.unwrap_or_else(|| {
            // find_map ran out of BTreeMap entries before finding a free fd, use one plus the
            // maximum fd in the map
            self.fds.last_key_value().map(|(fd_num, _)| fd_num.strict_add(1)).unwrap_or(min_fd_num)
        });

        self.fds.try_insert(new_fd_num, file_handle).unwrap();
        new_fd_num
    }

    pub fn get(&self, fd_num: i32) -> Option<DynFileDescriptionRef> {
        let fd = self.fds.get(&fd_num)?;
        Some(fd.clone())
    }

    pub fn remove(&mut self, fd_num: i32) -> Option<DynFileDescriptionRef> {
        self.fds.remove(&fd_num)
    }

    pub fn is_fd_num(&self, fd_num: i32) -> bool {
        self.fds.contains_key(&fd_num)
    }
}

impl<'tcx> EvalContextExt<'tcx> for crate::MiriInterpCx<'tcx> {}
pub trait EvalContextExt<'tcx>: crate::MiriInterpCxExt<'tcx> {
    /// Helper to implement `FileDescription::read`:
    /// This is only used when `read` is successful.
    /// `actual_read_size` should be the return value of some underlying `read` call that used
    /// `bytes` as its output buffer.
    /// The length of `bytes` must not exceed either the host's or the target's `isize`.
    /// `bytes` is written to `buf` and the size is written to `dest`.
    fn return_read_success(
        &mut self,
        buf: Pointer,
        bytes: &[u8],
        actual_read_size: usize,
        dest: &MPlaceTy<'tcx>,
    ) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        // If reading to `bytes` did not fail, we write those bytes to the buffer.
        // Crucially, if fewer than `bytes.len()` bytes were read, only write
        // that much into the output buffer!
        this.write_bytes_ptr(buf, bytes[..actual_read_size].iter().copied())?;

        // The actual read size is always less than what got originally requested so this cannot fail.
        this.write_int(u64::try_from(actual_read_size).unwrap(), dest)?;
        interp_ok(())
    }

    /// Helper to implement `FileDescription::write`:
    /// This function is only used when `write` is successful, and writes `actual_write_size` to `dest`
    fn return_write_success(
        &mut self,
        actual_write_size: usize,
        dest: &MPlaceTy<'tcx>,
    ) -> InterpResult<'tcx> {
        let this = self.eval_context_mut();
        // The actual write size is always less than what got originally requested so this cannot fail.
        this.write_int(u64::try_from(actual_write_size).unwrap(), dest)?;
        interp_ok(())
    }
}
