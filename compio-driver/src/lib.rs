//! The platform-specified driver.
//! Some types differ by compilation target.

#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(feature = "once_cell_try", feature(once_cell_try))]
#![warn(missing_docs)]

#[cfg(all(
    target_os = "linux",
    not(feature = "io-uring"),
    not(feature = "polling")
))]
compile_error!("You must choose at least one of these features: [\"io-uring\", \"polling\"]");

use std::{io, task::Poll, time::Duration};

use compio_buf::BufResult;
use slab::Slab;

pub mod op;
#[cfg(unix)]
#[cfg_attr(docsrs, doc(cfg(all())))]
mod unix;

mod asyncify;
pub use asyncify::*;

cfg_if::cfg_if! {
    if #[cfg(windows)] {
        #[path = "iocp/mod.rs"]
        mod sys;
    } else if #[cfg(all(target_os = "linux", feature = "polling", feature = "io-uring"))] {
        #[path = "fusion/mod.rs"]
        mod sys;
    } else if #[cfg(all(target_os = "linux", feature = "io-uring"))] {
        #[path = "iour/mod.rs"]
        mod sys;
    } else if #[cfg(unix)] {
        #[path = "poll/mod.rs"]
        mod sys;
    }
}

pub use sys::*;

#[cfg(windows)]
#[macro_export]
#[doc(hidden)]
macro_rules! syscall {
    (BOOL, $e:expr) => {
        $crate::syscall!($e, == 0)
    };
    (SOCKET, $e:expr) => {
        $crate::syscall!($e, != 0)
    };
    (HANDLE, $e:expr) => {
        $crate::syscall!($e, == ::windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE)
    };
    ($e:expr, $op: tt $rhs: expr) => {{
        #[allow(unused_unsafe)]
        let res = unsafe { $e };
        if res $op $rhs {
            Err(::std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}

/// Helper macro to execute a system call
#[cfg(unix)]
#[macro_export]
#[doc(hidden)]
macro_rules! syscall {
    (break $e:expr) => {
        match $crate::syscall!($e) {
            Ok(fd) => ::std::task::Poll::Ready(Ok(fd as usize)),
            Err(e) if e.kind() == ::std::io::ErrorKind::WouldBlock || e.raw_os_error() == Some(::libc::EINPROGRESS)
                   => ::std::task::Poll::Pending,
            Err(e) => ::std::task::Poll::Ready(Err(e)),
        }
    };
    ($e:expr, $f:ident($fd:expr)) => {
        match $crate::syscall!(break $e) {
            ::std::task::Poll::Pending => Ok($crate::sys::Decision::$f($fd)),
            ::std::task::Poll::Ready(Ok(res)) => Ok($crate::sys::Decision::Completed(res)),
            ::std::task::Poll::Ready(Err(e)) => Err(e),
        }
    };
    ($e:expr) => {{
        #[allow(unused_unsafe)]
        let res = unsafe { $e };
        if res == -1 {
            Err(::std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}

#[macro_export]
#[doc(hidden)]
macro_rules! impl_raw_fd {
    ($t:ty, $inner:ident) => {
        impl $crate::AsRawFd for $t {
            fn as_raw_fd(&self) -> $crate::RawFd {
                self.$inner.as_raw_fd()
            }
        }
        impl $crate::FromRawFd for $t {
            unsafe fn from_raw_fd(fd: $crate::RawFd) -> Self {
                Self {
                    $inner: $crate::FromRawFd::from_raw_fd(fd),
                }
            }
        }
        impl $crate::IntoRawFd for $t {
            fn into_raw_fd(self) -> $crate::RawFd {
                self.$inner.into_raw_fd()
            }
        }
    };
}

/// The return type of [`Proactor::push`].
pub enum PushEntry<K, R> {
    /// The operation is pushed to the submission queue.
    Pending(K),
    /// The operation is ready and returns.
    Ready(R),
}

impl<K, R> PushEntry<K, R> {
    /// Map the [`PushEntry::Pending`] branch.
    pub fn map_pending<L>(self, f: impl FnOnce(K) -> L) -> PushEntry<L, R> {
        match self {
            Self::Pending(k) => PushEntry::Pending(f(k)),
            Self::Ready(r) => PushEntry::Ready(r),
        }
    }

    /// Map the [`PushEntry::Ready`] branch.
    pub fn map_ready<S>(self, f: impl FnOnce(R) -> S) -> PushEntry<K, S> {
        match self {
            Self::Pending(k) => PushEntry::Pending(k),
            Self::Ready(r) => PushEntry::Ready(f(r)),
        }
    }
}

/// Low-level actions of completion-based IO.
/// It owns the operations to keep the driver safe.
pub struct Proactor {
    driver: Driver,
    ops: Slab<RawOp>,
}

impl Proactor {
    /// Create [`Proactor`] with 1024 entries.
    pub fn new() -> io::Result<Self> {
        Self::builder().build()
    }

    /// Create [`ProactorBuilder`] to config the proactor.
    pub fn builder() -> ProactorBuilder {
        ProactorBuilder::new()
    }

    fn with_builder(builder: &ProactorBuilder) -> io::Result<Self> {
        Ok(Self {
            driver: Driver::new(builder)?,
            ops: Slab::with_capacity(builder.capacity as _),
        })
    }

    /// Attach an fd to the driver. It will cause unexpected result to attach
    /// the handle with one driver and push an op to another driver.
    ///
    /// ## Platform specific
    /// * IOCP: it will be attached to the completion port. An fd could only be
    ///   attached to one driver, and could only be attached once, even if you
    ///   `try_clone` it.
    /// * io-uring: it will do nothing and return `Ok(())`.
    /// * polling: it will initialize inner queue and register to the driver. On
    ///   Linux and Android, if the fd is a normal file or a directory, this
    ///   method will do nothing. For other fd and systems, you should only call
    ///   this method once for a specific resource. If this method is called
    ///   twice with the same fd, we assume that the old fd has been closed, and
    ///   it's a new fd.
    pub fn attach(&mut self, fd: RawFd) -> io::Result<()> {
        self.driver.attach(fd)
    }

    /// Cancel an operation with the pushed user-defined data.
    ///
    /// The cancellation is not reliable. The underlying operation may continue,
    /// but just don't return from [`Proactor::poll`]. Therefore, although an
    /// operation is cancelled, you should not reuse its `user_data`.
    ///
    /// It is well-defined to cancel before polling. If the submitted operation
    /// contains a cancelled user-defined data, the operation will be ignored.
    pub fn cancel(&mut self, user_data: usize) {
        self.driver.cancel(user_data, &mut self.ops);
    }

    /// Push an operation into the driver, and return the unique key, called
    /// user-defined data, associated with it.
    pub fn push<T: OpCode + 'static>(&mut self, op: T) -> PushEntry<usize, BufResult<usize, T>> {
        let entry = self.ops.vacant_entry();
        let user_data = entry.key();
        let op = RawOp::new(user_data, op);
        let op = entry.insert(op);
        match self.driver.push(user_data, op) {
            Poll::Pending => PushEntry::Pending(user_data),
            Poll::Ready(res) => {
                let op = self.ops.remove(user_data);
                PushEntry::Ready(BufResult(res, unsafe { op.into_inner::<T>() }))
            }
        }
    }

    /// Poll the driver and get completed entries.
    /// You need to call [`Proactor::pop`] to get the pushed operations.
    pub fn poll(
        &mut self,
        timeout: Option<Duration>,
        entries: &mut impl Extend<Entry>,
    ) -> io::Result<()> {
        unsafe {
            self.driver.poll(timeout, entries, &mut self.ops)?;
        }
        Ok(())
    }

    /// Get the pushed operations from the completion entries.
    pub fn pop<'a>(
        &'a mut self,
        entries: &'a mut impl Iterator<Item = Entry>,
    ) -> impl Iterator<Item = BufResult<usize, Operation>> + 'a {
        std::iter::from_fn(|| {
            entries.next().map(|entry| {
                let op = self
                    .ops
                    .try_remove(entry.user_data())
                    .expect("the entry should be valid");
                let op = Operation::new(op, entry.user_data());
                BufResult(entry.into_result(), op)
            })
        })
    }

    /// Create a notify handle to interrupt the inner driver.
    pub fn handle(&self) -> io::Result<NotifyHandle> {
        self.driver.handle()
    }

    /// Create a notify handle for specified user_data.
    ///
    /// # Safety
    ///
    /// The caller should ensure `user_data` being valid.
    #[cfg(windows)]
    pub unsafe fn handle_for(&self, user_data: usize) -> io::Result<NotifyHandle> {
        self.driver.handle_for(user_data)
    }
}

impl AsRawFd for Proactor {
    fn as_raw_fd(&self) -> RawFd {
        self.driver.as_raw_fd()
    }
}

/// Contains the operation and the user_data.
pub struct Operation {
    op: RawOp,
    user_data: usize,
}

impl Operation {
    pub(crate) fn new(op: RawOp, user_data: usize) -> Self {
        Self { op, user_data }
    }

    /// Restore the original operation.
    ///
    /// # Safety
    ///
    /// The caller should guarantee that the type is right.
    pub unsafe fn into_op<T: OpCode>(self) -> T {
        self.op.into_inner()
    }

    /// The same user_data when the operation is pushed into the driver.
    pub fn user_data(&self) -> usize {
        self.user_data
    }
}

/// An completed entry returned from kernel.
#[derive(Debug)]
pub struct Entry {
    user_data: usize,
    result: io::Result<usize>,
}

impl Entry {
    pub(crate) fn new(user_data: usize, result: io::Result<usize>) -> Self {
        Self { user_data, result }
    }

    /// The user-defined data returned by [`Proactor::push`].
    pub fn user_data(&self) -> usize {
        self.user_data
    }

    /// The result of the operation.
    pub fn into_result(self) -> io::Result<usize> {
        self.result
    }
}

#[derive(Debug, Clone)]
enum ThreadPoolBuilder {
    Create { limit: usize, recv_limit: Duration },
    Reuse(AsyncifyPool),
}

impl Default for ThreadPoolBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadPoolBuilder {
    pub fn new() -> Self {
        Self::Create {
            limit: 256,
            recv_limit: Duration::from_secs(60),
        }
    }

    pub fn create_or_reuse(&self) -> AsyncifyPool {
        match self {
            Self::Create { limit, recv_limit } => AsyncifyPool::new(*limit, *recv_limit),
            Self::Reuse(pool) => pool.clone(),
        }
    }
}

/// Builder for [`Proactor`].
#[derive(Debug, Clone)]
pub struct ProactorBuilder {
    capacity: u32,
    pool_builder: ThreadPoolBuilder,
}

impl Default for ProactorBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ProactorBuilder {
    /// Create the builder with default config.
    pub fn new() -> Self {
        Self {
            capacity: 1024,
            pool_builder: ThreadPoolBuilder::new(),
        }
    }

    /// Set the capacity of the inner event queue or submission queue, if
    /// exists. The default value is 1024.
    pub fn capacity(&mut self, capacity: u32) -> &mut Self {
        self.capacity = capacity;
        self
    }

    /// Set the thread number limit of the inner thread pool, if exists. The
    /// default value is 256.
    ///
    /// It will be ignored if `reuse_thread_pool` is set.
    pub fn thread_pool_limit(&mut self, value: usize) -> &mut Self {
        if let ThreadPoolBuilder::Create { limit, .. } = &mut self.pool_builder {
            *limit = value;
        }
        self
    }

    /// Set the waiting timeout of the inner thread, if exists. The default is
    /// 60 seconds.
    ///
    /// It will be ignored if `reuse_thread_pool` is set.
    pub fn thread_pool_recv_timeout(&mut self, timeout: Duration) -> &mut Self {
        if let ThreadPoolBuilder::Create { recv_limit, .. } = &mut self.pool_builder {
            *recv_limit = timeout;
        }
        self
    }

    /// Set to reuse an existing [`AsyncifyPool`] in this proactor.
    pub fn reuse_thread_pool(&mut self, pool: AsyncifyPool) -> &mut Self {
        self.pool_builder = ThreadPoolBuilder::Reuse(pool);
        self
    }

    /// Force reuse the thread pool for each proactor created by this builder,
    /// even `reuse_thread_pool` is not set.
    pub fn force_reuse_thread_pool(&mut self) -> &mut Self {
        self.reuse_thread_pool(self.create_or_get_thread_pool());
        self
    }

    /// Create or reuse the thread pool from the config.
    pub fn create_or_get_thread_pool(&self) -> AsyncifyPool {
        self.pool_builder.create_or_reuse()
    }

    /// Build the [`Proactor`].
    pub fn build(&self) -> io::Result<Proactor> {
        Proactor::with_builder(self)
    }
}
