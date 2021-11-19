//! Everything to perform async system call injection to perform system calls
//! within the guest.
//!
//! This feature allows for writing code using Rust's async model in such a manner
//! that allows you to treat guest system calls as I/O to be performed. This enables
//! writing code that feels synchronous while allowing for automatically running the
//! guest concurrently in order to perform any needed tasks such as filesystem access,
//! interacting with processes/signals, mapping memory, etc. all within the guest,
//! while all computation is performed on the host.
//!
//! A system call injector under this API is an async block which can make use of the
//! [`syscall`] function in order to perform system calls. An injector can only be run
//! (or, rather, started) within a syscall enter callback.
//!
//! ## Example
//!
//! ```
//! use panda::prelude::*;
//! use panda::syscall_injection::{run_injector, syscall};
//!
//! async fn getpid() -> target_ulong {
//!     syscall(GET_PID, ()).await
//! }
//!
//! async fn getuid() -> target_ulong {
//!     syscall(GET_UID, ()).await
//! }
//!
//! #[panda::on_all_sys_enter]
//! fn any_syscall(cpu: &mut CPUState, pc: SyscallPc, syscall_num: target_ulong) {
//!     run_injector(pc, async {
//!         println!("PID: {}", getpid().await);
//!         println!("UID: {}", getuid().await);
//!         println!("PID (again): {}", getpid().await);
//!     });
//! }
//!
//! fn main() {
//!     Panda::new()
//!         .generic("x86_64")
//!         .args(&["-loadvm", "root"])
//!         .run();
//! }
//! ```
//!
//! (Full example present in `examples/syscall_injection.rs`)

use std::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

use dashmap::DashMap;
use lazy_static::lazy_static;
use parking_lot::{const_mutex, Mutex};

use crate::prelude::*;
use crate::{
    plugins::{osi::OSI, syscalls2::Syscalls2Callbacks},
    regs, sys, PppCallback,
};

mod conversion;
mod pinned_queue;
mod syscall_future;
mod syscall_regs;
mod syscalls;

pub use {conversion::*, syscall_future::*};
use {
    pinned_queue::PinnedQueue,
    syscall_future::WAITING_FOR_SYSCALL,
    syscall_regs::{SyscallRegs, SYSCALL_RET},
};

#[cfg(feature = "x86_64")]
const FORK: target_ulong = 57;

#[cfg(not(feature = "x86_64"))]
compile_error!("Only x86_64 has fork defined");

type Injector = dyn Future<Output = ()>;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ThreadId {
    pid: target_ulong,
    tid: target_ulong,
}

impl ThreadId {
    fn current() -> Self {
        let cpu = unsafe { &mut *sys::get_cpu() };
        let thread = OSI.get_current_thread(cpu);

        let tid = thread.tid as target_ulong;
        let pid = thread.pid as target_ulong;
        Self { tid, pid }
    }
}

lazy_static! {
    static ref INJECTORS: DashMap<ThreadId, PinnedQueue<Injector>> = DashMap::new();
}

struct ChildInjector((SyscallRegs, Pin<Box<dyn Future<Output = ()> + 'static>>));

unsafe impl Send for ChildInjector {}
unsafe impl Sync for ChildInjector {}

static CHILD_INJECTOR: Mutex<Option<ChildInjector>> = const_mutex(None);

pub async fn fork(child_injector: impl Future<Output = ()> + 'static) -> target_ulong {
    let backed_up_regs = get_backed_up_regs().expect("Fork was run outside of an injector");

    println!("set child injector");
    CHILD_INJECTOR
        .lock()
        .replace(ChildInjector((backed_up_regs, Box::pin(child_injector))));
    println!("child injector set");

    syscall(FORK, ()).await
}

fn get_child_injector() -> (SyscallRegs, Pin<Box<dyn Future<Output = ()> + 'static>>) {
    println!("get child injector");
    CHILD_INJECTOR.lock().take().unwrap().0
}

/// Run a syscall injector in the form as an async block/value to be evaluated. If
/// another injector is already running, it will be queued to start after all previous
/// injectors have finished running.
///
/// This operates by running each system call before resuming the original system call,
/// allowing the guest to run until all injected system calls have finished.
///
/// ### Context Requirements
///
/// `run_injector` must be run within a syscall enter callback. This is enforced by
/// means of only accepting [`SyscallPc`] to prevent misuse.
///
/// If you'd like to setup an injector to run during the next system call to avoid this
/// requirement, see [`run_injector_next_syscall`].
///
/// ### Async Execution
///
/// The async runtime included allows for non-system call futures to be awaited, however
/// the async executor used does not provide any support for any level of parallelism
/// outside of Host/Guest parallelism. This means any async I/O performed will be
/// busily polled, wakers are no-ops, and executor-dependent futures will not function.
///
/// There are currently no plans for injectors to be a true-async context, so
/// outside of simple Futures it is recommended to only use the provided [`syscall`]
/// function and Futures built on top of it.
///
/// ### Behavior
///
/// The behavior of injecting into system calls which don't return, fork, or otherwise
/// effect control flow, are currently not defined.
pub fn run_injector(pc: SyscallPc, injector: impl Future<Output = ()> + 'static) {
    let pc = pc.pc();

    let is_first = INJECTORS.is_empty();
    let thread_id = ThreadId::current();
    INJECTORS
        .entry(thread_id)
        .or_default()
        .push_future(current_asid(), async {
            let backed_up_regs = SyscallRegs::backup();
            set_backed_up_regs(backed_up_regs.clone());

            injector.await;

            backed_up_regs.restore();
            unset_backed_up_regs();
            println!("Registers restored");
        });

    // Only install each callback once
    if is_first {
        let sys_enter = PppCallback::new();
        let sys_return = PppCallback::new();

        // after the syscall set the return value for the future then jump back to
        // the syscall instruction
        sys_return.on_all_sys_return(move |cpu: &mut CPUState, _, sys_num_bad| {
            dbg!(sys_num_bad);
            let sys_num = last_injected_syscall();
            let is_fork_child = if dbg!(sys_num) == FORK {
                regs::get_reg(cpu, SYSCALL_RET) == 0
            } else {
                false
            };

            if is_fork_child {
                println!("in fork child");
                let (backed_up_regs, child_injector) = get_child_injector();

                // set up a child-injector, which doesn't back up its registers, only
                // sets up to restore the registers of its parent
                INJECTORS
                    .entry(ThreadId::current())
                    .or_default()
                    .push_future(current_asid(), async move {
                        println!("Start of child injector");
                        child_injector.await;

                        backed_up_regs.restore();
                        println!("Child registers restored");
                    });
            }

            println!("Should loop back?");
            // only run for the asid we're currently injecting into, unless we just forked
            if is_fork_child
                || (CURRENT_INJECTOR_ASID.load(Ordering::SeqCst) == current_asid() as u64)
            {
                SHOULD_LOOP_AGAIN.store(true, Ordering::SeqCst);
                set_ret_value(cpu);
                regs::set_pc(cpu, pc);
                unsafe {
                    panda::sys::cpu_loop_exit_noexc(cpu);
                }
            }
        });

        // poll the injectors and if they've all finished running, disable these
        // callbacks
        sys_enter.on_all_sys_enter(move |cpu, _, sys_num| {
            if poll_injectors() {
                sys_enter.disable();
                sys_return.disable();
            }

            if SHOULD_LOOP_AGAIN.swap(false, Ordering::SeqCst) {
                println!("Looping again...");
                regs::set_pc(cpu, pc);
                unsafe {
                    panda::sys::cpu_loop_exit_noexc(cpu);
                }
                return;
            } else {
                println!("Not looping again, sys_num: {}", sys_num);
            }
        });

        // If this is the first syscall it needs to be polled too,
        // disabling if it's already finished running
        if poll_injectors() {
            println!("WARN: Injector seemed to not call any system calls?");
            sys_enter.disable();
            sys_return.disable();
        }
    }
}

static SHOULD_LOOP_AGAIN: AtomicBool = AtomicBool::new(false);

lazy_static! {
    static ref CURRENT_REGS_BACKUP: DashMap<ThreadId, SyscallRegs> = DashMap::new();
}

pub fn get_backed_up_regs() -> Option<SyscallRegs> {
    CURRENT_REGS_BACKUP
        .get(&ThreadId::current())
        .map(|x| x.clone())
}

fn set_backed_up_regs(regs: SyscallRegs) {
    CURRENT_REGS_BACKUP.insert(ThreadId::current(), regs);
}

fn unset_backed_up_regs() {
    CURRENT_REGS_BACKUP.remove(&ThreadId::current());
}

fn current_asid() -> target_ulong {
    unsafe { sys::panda_current_asid(sys::get_cpu()) }
}

/// Queue an injector to be run during the next system call.
///
/// For more information or for usage during a system call callback, see [`run_injector`].
pub fn run_injector_next_syscall(injector: impl Future<Output = ()> + 'static) {
    let next_syscall = PppCallback::new();
    let mut injector = Some(injector);

    next_syscall.on_all_sys_enter(move |_, pc, _| {
        let injector = injector.take().unwrap();
        run_injector(pc, injector);
        next_syscall.disable();
    });
}

fn do_nothing(_ptr: *const ()) {}

fn clone(ptr: *const ()) -> RawWaker {
    RawWaker::new(ptr, &VTABLE)
}

static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, do_nothing, do_nothing, do_nothing);

fn waiting_for_syscall() -> bool {
    WAITING_FOR_SYSCALL.load(Ordering::SeqCst)
}

static CURRENT_INJECTOR_ASID: AtomicU64 = AtomicU64::new(0);

/// Returns true if all injectors have been processed
fn poll_injectors() -> bool {
    let raw = RawWaker::new(std::ptr::null(), &VTABLE);
    let waker = unsafe { Waker::from_raw(raw) };
    let mut ctxt = Context::from_waker(&waker);

    // reset the 'waiting for system call' flag
    WAITING_FOR_SYSCALL.store(false, Ordering::SeqCst);

    if let Some(mut injectors) = INJECTORS.get_mut(&ThreadId::current()) {
        while let Some(ref mut current_injector) = injectors.current_mut() {
            let (asid, ref mut current_injector) = &mut *current_injector;
            CURRENT_INJECTOR_ASID.store(*asid as u64, Ordering::SeqCst);
            // only poll from correct asid
            if *asid != current_asid() {
                return false;
            }
            match current_injector.as_mut().poll(&mut ctxt) {
                // If the current injector has finished running start polling the next
                // injector.
                Poll::Ready(_) => {
                    injectors.pop();
                    continue;
                }

                // If the future is now waiting on a syscall to be evaluated, return
                // so a system call can be run
                Poll::Pending if waiting_for_syscall() => return false,

                // If the future is not waiting on a system call we should keep polling
                Poll::Pending => continue,
            }
        }
    } else {
        return false;
    }

    true
}
