//! Portable interface to epoll, kqueue, and wepoll.
//!
//! Supported platforms:
//! - [epoll](https://en.wikipedia.org/wiki/Epoll): Linux, Android, illumos
//! - [kqueue](https://en.wikipedia.org/wiki/Kqueue): macOS, iOS, FreeBSD, NetBSD, OpenBSD,
//!   DragonFly BSD
//! - [wepoll](https://github.com/piscisaureus/wepoll): Windows
//!
//! Polling is done in oneshot mode, which means interest in I/O events needs to be reset after
//! an event is delivered if we're interested in the next event of the same kind.
//!
//! Only one thread can be waiting for I/O events at a time.
//!
//! # Examples
//!
//! ```no_run
//! use polling::{Event, Poller};
//! use std::net::TcpListener;
//!
//! // Create a TCP listener and put the socket in non-blocking mode.
//! let socket = TcpListener::bind("127.0.0.1:8000")?;
//! socket.set_nonblocking(true)?;
//! let key = 7; // arbitrary key identifying the socket
//!
//! // Create a poller and register interest in readability on the socket.
//! let poller = Poller::new()?;
//! poller.insert(&socket)?;
//! poller.interest(&socket, Event::readable(key))?;
//!
//! // The event loop.
//! let mut events = Vec::new();
//! loop {
//!     // Wait for at least one I/O event.
//!     events.clear();
//!     poller.wait(&mut events, None)?;
//!
//!     for ev in &events {
//!         if ev.key == key {
//!             // Perform a non-blocking accept operation.
//!             socket.accept()?;
//!             // Set interest in the next readability event.
//!             poller.interest(&socket, Event::readable(key))?;
//!         }
//!     }
//! }
//! # std::io::Result::Ok(())
//! ```

#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]

use std::fmt;
use std::io;
use std::sync::Mutex;
use std::time::Duration;
use std::usize;

use cfg_if::cfg_if;

/// Calls a libc function and results in `io::Result`.
#[cfg(unix)]
macro_rules! syscall {
    ($fn:ident $args:tt) => {{
        let res = unsafe { libc::$fn $args };
        if res == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}

cfg_if! {
    if #[cfg(any(target_os = "linux", target_os = "android", target_os = "illumos"))] {
        mod epoll;
        use epoll as sys;
    } else if #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
    ))] {
        mod kqueue;
        use kqueue as sys;
    } else if #[cfg(target_os = "windows")] {
        mod wepoll;
        use wepoll as sys;
    } else {
        compile_error!("polling does not support this target OS");
    }
}

/// Indicates that a file descriptor or socket can read or write without blocking.
#[derive(Debug)]
pub struct Event {
    /// Key identifying the file descriptor or socket.
    pub key: usize,
    /// Can it do a read operation without blocking?
    pub readable: bool,
    /// Can it do a write operation without blocking?
    pub writable: bool,
}

impl Event {
    /// All kinds of events (readable and writable).
    ///
    /// Equivalent to: `Event { key, readable: true, writable: true }`
    pub fn all(key: usize) -> Event {
        Event {
            key,
            readable: true,
            writable: true,
        }
    }

    /// Only the readable event.
    ///
    /// Equivalent to: `Event { key, readable: true, writable: false }`
    pub fn readable(key: usize) -> Event {
        Event {
            key,
            readable: true,
            writable: false,
        }
    }

    /// Only the writable event.
    ///
    /// Equivalent to: `Event { key, readable: false, writable: true }`
    pub fn writable(key: usize) -> Event {
        Event {
            key,
            readable: false,
            writable: true,
        }
    }

    /// No events.
    ///
    /// Equivalent to: `Event { key, readable: false, writable: false }`
    pub fn none(key: usize) -> Event {
        Event {
            key,
            readable: true,
            writable: true,
        }
    }
}

/// Waits for I/O events.
pub struct Poller {
    poller: sys::Poller,
    events: Mutex<sys::Events>,
}

impl Poller {
    /// Creates a new poller.
    ///
    /// # Examples
    ///
    /// ```
    /// use polling::Poller;
    ///
    /// let poller = Poller::new()?;
    /// # std::io::Result::Ok(())
    /// ```
    pub fn new() -> io::Result<Poller> {
        let poller = sys::Poller::new()?;
        let events = Mutex::new(sys::Events::new());
        Ok(Poller { poller, events })
    }

    /// Inserts a file descriptor or socket into the poller.
    ///
    /// Before setting interest in readability or writability, the file descriptor or socket must
    /// be inserted into the poller.
    ///
    /// Don't forget to [remove][`Poller::remove()`] it when it is no longer used!
    ///
    /// # Examples
    ///
    /// ```
    /// use polling::Poller;
    /// use std::net::TcpListener;
    ///
    /// let poller = Poller::new()?;
    /// let socket = TcpListener::bind("127.0.0.1:0")?;
    ///
    /// poller.insert(&socket)?;
    /// # std::io::Result::Ok(())
    /// ```
    pub fn insert(&self, source: impl Source) -> io::Result<()> {
        self.poller.insert(source.raw())
    }

    /// Enables or disables interest in a file descriptor or socket.
    ///
    /// A file descriptor or socket is considered readable or writable when a read or write
    /// operation on it would not block. This doesn't mean the read or write operation will
    /// succeed, it only means the operation will return immediately.
    ///
    /// If interest is set in both readability and writability, the two kinds of events might be
    /// delivered either separately or together.
    ///
    /// For example, interest in `Event { key: 7, readable: true, writable: true }` might result in
    /// a single [`Event`] of the same form, or in two separate [`Event`]s:
    /// - `Event { key: 7, readable: true, writable: false }`
    /// - `Event { key: 7, readable: false, writable: true }`
    ///
    /// # Errors
    ///
    /// This method returns an error in the following situations:
    ///
    /// * If `source` was not [inserted][`Poller::interest()`] into the poller.
    /// * If `key` equals `usize::MAX` because that key is reserved for internal use.
    /// * If an error is returned by the syscall.
    ///
    /// # Examples
    ///
    /// To enable interest in all events:
    ///
    /// ```no_run
    /// # use polling::{Event, Poller};
    /// # let poller = Poller::new()?;
    /// # let key = 7;
    /// # let source = std::net::TcpListener::bind("127.0.0.1:0")?;
    /// poller.interest(&source, Event::all(key))?;
    /// # std::io::Result::Ok(())
    /// ```
    ///
    /// To enable interest in readable events and disable interest in writable events:
    ///
    /// ```no_run
    /// # use polling::{Event, Poller};
    /// # let poller = Poller::new()?;
    /// # let key = 7;
    /// # let source = std::net::TcpListener::bind("127.0.0.1:0")?;
    /// poller.interest(&source, Event::readable(key))?;
    /// # std::io::Result::Ok(())
    /// ```
    ///
    /// To disable interest in readable events and enable interest in writable events:
    ///
    /// ```no_run
    /// # use polling::{Event, Poller};
    /// # let poller = Poller::new()?;
    /// # let key = 7;
    /// # let source = std::net::TcpListener::bind("127.0.0.1:0")?;
    /// poller.interest(&source, Event::writable(key))?;
    /// # std::io::Result::Ok(())
    /// ```
    ///
    /// To disable interest in all events:
    ///
    /// ```no_run
    /// # use polling::{Event, Poller};
    /// # let poller = Poller::new()?;
    /// # let key = 7;
    /// # let source = std::net::TcpListener::bind("127.0.0.1:0")?;
    /// poller.interest(&source, Event::none(key))?;
    /// # std::io::Result::Ok(())
    /// ```
    pub fn interest(&self, source: impl Source, event: Event) -> io::Result<()> {
        if event.key == usize::MAX {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the key cannot be `usize::MAX`",
            ))
        } else {
            self.poller.interest(source.raw(), event)
        }
    }

    /// Removes a file descriptor or socket from the poller.
    ///
    /// # Examples
    ///
    /// ```
    /// use polling::Poller;
    /// use std::net::TcpListener;
    ///
    /// let poller = Poller::new()?;
    /// let socket = TcpListener::bind("127.0.0.1:0")?;
    ///
    /// poller.insert(&socket)?;
    /// poller.remove(&socket)?;
    /// # std::io::Result::Ok(())
    /// ```
    pub fn remove(&self, source: impl Source) -> io::Result<()> {
        self.poller.remove(source.raw())
    }

    /// Waits for at least one I/O event and returns the number of new events.
    ///
    /// New events will be appended to `events`.
    ///
    /// This call will return with no new events if a notification is delivered by the [`notify()`]
    /// method, or the timeout is reached.
    ///
    /// Only one thread can wait on I/O. If another thread is already in [`wait()`], concurrent
    /// calls to this method will return immediately with no new events.
    ///
    /// If the operating system is ready to deliver a large number of events at once, this method
    /// may decide to deliver them in smaller batches.
    ///
    /// [`notify()`]: `Poller::notify()`
    /// [`wait()`]: `Poller::wait()`
    ///
    /// # Examples
    ///
    /// ```
    /// use polling::Poller;
    /// use std::net::TcpListener;
    /// use std::time::Duration;
    ///
    /// let poller = Poller::new()?;
    /// let socket = TcpListener::bind("127.0.0.1:0")?;
    /// poller.insert(&socket)?;
    ///
    /// let mut events = Vec::new();
    /// let n = poller.wait(&mut events, Some(Duration::from_secs(1)))?;
    /// # std::io::Result::Ok(())
    /// ```
    pub fn wait(&self, events: &mut Vec<Event>, timeout: Option<Duration>) -> io::Result<usize> {
        if let Ok(mut lock) = self.events.try_lock() {
            let n = self.poller.wait(&mut lock, timeout)?;
            events.extend(lock.iter().filter(|ev| ev.key != usize::MAX));
            Ok(n)
        } else {
            Ok(0)
        }
    }

    /// Wakes up the current or the following invocation of [`wait()`].
    ///
    /// If no thread is calling [`wait()`] right now, this method will cause the following call
    /// to wake up immediately.
    ///
    /// [`wait()`]: `Poller::wait()`
    ///
    /// # Examples
    ///
    /// ```
    /// use polling::Poller;
    ///
    /// let poller = Poller::new()?;
    ///
    /// // Notify the poller.
    /// poller.notify()?;
    ///
    /// let mut events = Vec::new();
    /// poller.wait(&mut events, None)?; // wakes up immediately
    /// assert!(events.is_empty());
    /// # std::io::Result::Ok(())
    /// ```
    pub fn notify(&self) -> io::Result<()> {
        self.poller.notify()
    }
}

impl fmt::Debug for Poller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.poller.fmt(f)
    }
}

cfg_if! {
    if #[cfg(unix)] {
        use std::os::unix::io::{AsRawFd, RawFd};

        /// A [`RawFd`] or a reference to a type implementing [`AsRawFd`].
        pub trait Source {
            /// Returns the [`RawFd`] for this I/O object.
            fn raw(&self) -> RawFd;
        }

        impl Source for RawFd {
            fn raw(&self) -> RawFd {
                *self
            }
        }

        impl<T: AsRawFd> Source for &T {
            fn raw(&self) -> RawFd {
                self.as_raw_fd()
            }
        }
    } else if #[cfg(windows)] {
        use std::os::windows::io::{AsRawSocket, RawSocket};

        /// A [`RawSocket`] or a reference to a type implementing [`AsRawSocket`].
        pub trait Source {
            /// Returns the [`RawSocket`] for this I/O object.
            fn raw(&self) -> RawSocket;
        }

        impl Source for RawSocket {
            fn raw(&self) -> RawSocket {
                *self
            }
        }

        impl<T: AsRawSocket> Source for &T {
            fn raw(&self) -> RawSocket {
                self.as_raw_socket()
            }
        }
    }
}
