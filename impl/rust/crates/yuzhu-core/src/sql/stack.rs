//! PostgreSQL's `check_stack_depth`: a guard against unbounded recursion.
//!
//! A Rust stack overflow aborts the whole process (it cannot be caught), so
//! every recursive walk over user-controlled input (parser, analyzer,
//! executor) should call [`check_stack_depth`] at each recursion step and
//! fail with `54001 stack depth limit exceeded` instead.
//!
//! The check is address based, like PostgreSQL's: the address of a local
//! variable approximates the stack pointer (no `unsafe` needed), and usage is
//! measured from the shallowest point a check ran on this thread. Linux
//! stacks grow downward.

use std::cell::Cell;

use crate::error::{Error, Result, SqlState};

/// Default per-thread budget: half of Rust's default 2 MB thread stack,
/// leaving headroom for the frames above the first check and below the
/// last one. Threads with a bigger stack should raise it with
/// [`set_stack_budget`].
pub const DEFAULT_STACK_BUDGET: usize = 1 << 20;

thread_local! {
    /// Highest (shallowest) stack address a check has seen on this thread.
    static BASE: Cell<usize> = const { Cell::new(0) };
    static BUDGET: Cell<usize> = const { Cell::new(DEFAULT_STACK_BUDGET) };
}

/// Sets how many bytes of stack the current thread may use (measured from
/// the shallowest check) before [`check_stack_depth`] fails. Call it at the
/// start of a thread spawned with a non-default `stack_size`, with a value
/// comfortably below that size.
pub fn set_stack_budget(bytes: usize) {
    BUDGET.with(|b| b.set(bytes));
}

#[inline(never)]
fn stack_pointer() -> usize {
    let marker = 0u8;
    std::hint::black_box(std::ptr::addr_of!(marker)) as usize
}

/// Fails with `54001` when the current thread has used more stack than its
/// budget allows.
pub fn check_stack_depth() -> Result<()> {
    let sp = stack_pointer();
    let base = BASE.with(|b| {
        if sp > b.get() {
            b.set(sp);
        }
        b.get()
    });
    if base - sp > BUDGET.with(Cell::get) {
        return Err(stack_depth_error());
    }
    Ok(())
}

/// `54001 stack depth limit exceeded`, worded as PostgreSQL reports it.
pub fn stack_depth_error() -> Error {
    Error::new(SqlState("54001"), "stack depth limit exceeded").with_hint(
        "Increase the configuration parameter \"max_stack_depth\" (currently 2048kB), \
         after ensuring the platform's stack depth limit is adequate.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recurse() -> Result<usize> {
        check_stack_depth()?;
        let pad = std::hint::black_box([0u8; 512]);
        Ok(recurse()? + usize::from(pad[0]))
    }

    #[test]
    fn unbounded_recursion_fails_with_54001() {
        std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(|| {
                let e = recurse().unwrap_err();
                assert_eq!(e.sqlstate.code(), "54001");
                assert_eq!(e.message, "stack depth limit exceeded");
                // Shallow again: passes.
                assert!(check_stack_depth().is_ok());
                set_stack_budget(64 << 10);
                assert!(recurse().is_err());
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
