mod accept;

mod close;
pub(crate) use close::Close;

mod connect;

mod fsync;

mod noop;
pub(crate) use noop::NoOp;

mod op;
pub(crate) use op::Op;

mod open;

mod read;

mod readv;

mod recv_from;

mod rename_at;

mod send_to;

mod send_zc;

mod shared_fd;
pub(crate) use shared_fd::SharedFd;

mod socket;
pub(crate) use socket::Socket;

mod unlink_at;

mod util;

mod write;

mod writev;

use crate::driver::op::Lifecycle;
use io_uring::opcode::AsyncCancel;
use io_uring::IoUring;
use slab::Slab;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};

pub(crate) struct Driver {
    /// In-flight operations
    ops: Ops,

    /// IoUring bindings
    pub(crate) uring: IoUring,
}

struct Ops {
    // When dropping the driver, all in-flight operations must have completed. This
    // type wraps the slab and ensures that, on drop, the slab is empty.
    lifecycle: Slab<op::Lifecycle>,

    /// Received but unserviced Op completions
    completions: Slab<op::Completion>,
}

impl Driver {
    pub(crate) fn new(b: &crate::Builder) -> io::Result<Driver> {
        let uring = b.urb.build(b.entries)?;

        Ok(Driver {
            ops: Ops::new(),
            uring,
        })
    }

    fn wait(&self) -> io::Result<usize> {
        self.uring.submit_and_wait(1)
    }

    // only used in tests rn
    #[allow(unused)]
    fn num_operations(&self) -> usize {
        self.ops.lifecycle.len()
    }

    pub(crate) fn tick(&mut self) {
        let mut cq = self.uring.completion();
        cq.sync();

        for cqe in cq {
            if cqe.user_data() == u64::MAX {
                // Result of the cancellation action. There isn't anything we
                // need to do here. We must wait for the CQE for the operation
                // that was canceled.
                continue;
            }

            let index = cqe.user_data() as _;

            self.ops.complete(index, cqe.into());
        }
    }

    pub(crate) fn submit(&mut self) -> io::Result<()> {
        loop {
            match self.uring.submit() {
                Ok(_) => {
                    self.uring.submission().sync();
                    return Ok(());
                }
                Err(ref e) if e.raw_os_error() == Some(libc::EBUSY) => {
                    self.tick();
                }
                Err(e) if e.raw_os_error() != Some(libc::EINTR) => {
                    return Err(e);
                }
                _ => continue,
            }
        }
    }
}

impl AsRawFd for Driver {
    fn as_raw_fd(&self) -> RawFd {
        self.uring.as_raw_fd()
    }
}

/// Drop the driver, cancelling any in-progress ops and waiting for them to terminate.
///
/// This first cancels all ops and then waits for them to be moved to the completed lifecycle phase.
///
/// It is possible for this to be run without previously dropping the runtime, but this should only
/// be possible in the case of [`std::process::exit`].
///
/// This depends on us knowing when ops are completed and done firing.
/// When multishot ops are added (support exists but none are implemented), a way to know if such
/// an op is finished MUST be added, otherwise our shutdown process is unsound.
impl Drop for Driver {
    fn drop(&mut self) {
        // get all ops in flight for cancellation
        while !self.uring.submission().is_empty() {
            self.submit().expect("Internal error when dropping driver");
        }

        // Pre-determine what to cancel
        // After this pass, all LifeCycles will be marked either as Completed or Ignored, as appropriate
        for (_, cycle) in self.ops.lifecycle.iter_mut() {
            match std::mem::replace(cycle, Lifecycle::Ignored(Box::new(()))) {
                lc @ Lifecycle::Completed(_) => {
                    // don't cancel completed items
                    *cycle = lc;
                }

                Lifecycle::CompletionList(indices) => {
                    let mut list = indices.clone().into_list(&mut self.ops.completions);
                    if !io_uring::cqueue::more(list.peek_end().unwrap().flags) {
                        // This op is complete. Replace with a null Completed entry
                        *cycle = Lifecycle::Completed(op::CqeResult {
                            result: Ok(0),
                            flags: 0,
                        });
                    }
                }

                _ => {
                    // All other states need cancelling.
                    // The mem::replace means these are now marked Ignored.
                }
            }
        }

        // Submit cancellation for all ops marked Ignored
        for (id, cycle) in self.ops.lifecycle.iter_mut() {
            if let Lifecycle::Ignored(..) = cycle {
                unsafe {
                    while self
                        .uring
                        .submission()
                        .push(&AsyncCancel::new(id as u64).build().user_data(u64::MAX))
                        .is_err()
                    {
                        self.uring
                            .submit_and_wait(1)
                            .expect("Internal error when dropping driver");
                    }
                }
            }
        }

        // Wait until all Lifetimes have been removed from the slab.
        //
        // Ignored entries will be removed from the Lifecycle slab
        // by the complete logic called by `tick()`
        //
        // Completed Entries are removed here directly
        let mut id = 0;
        loop {
            if self.ops.lifecycle.is_empty() {
                break;
            }
            // Cycles are either all ignored or complete
            // If there is at least one Ignored still to process, call wait
            match self.ops.lifecycle.get(id) {
                Some(Lifecycle::Ignored(..)) => {
                    // If waiting fails, ignore the error. The wait will be attempted
                    // again on the next loop.
                    let _ = self.wait();
                    self.tick();
                }

                Some(_) => {
                    // Remove Completed entries
                    let _ = self.ops.lifecycle.remove(id);
                    id += 1;
                }

                None => {
                    id += 1;
                }
            }
        }
    }
}

impl Ops {
    fn new() -> Ops {
        Ops {
            lifecycle: Slab::with_capacity(64),
            completions: Slab::with_capacity(64),
        }
    }

    fn get_mut(&mut self, index: usize) -> Option<(&mut op::Lifecycle, &mut Slab<op::Completion>)> {
        let completions = &mut self.completions;
        self.lifecycle
            .get_mut(index)
            .map(|lifecycle| (lifecycle, completions))
    }

    // Insert a new operation
    fn insert(&mut self) -> usize {
        self.lifecycle.insert(op::Lifecycle::Submitted)
    }

    // Remove an operation
    fn remove(&mut self, index: usize) {
        self.lifecycle.remove(index);
    }

    fn complete(&mut self, index: usize, cqe: op::CqeResult) {
        let completions = &mut self.completions;
        if self.lifecycle[index].complete(completions, cqe) {
            self.lifecycle.remove(index);
        }
    }
}

impl Drop for Ops {
    fn drop(&mut self) {
        assert!(self
            .lifecycle
            .iter()
            .all(|(_, cycle)| matches!(cycle, Lifecycle::Completed(_))))
    }
}
