// A minimal `RefCell` replacement that does not pull in `core::fmt`.
//
// std's `RefCell` formats a `BorrowError`/`BorrowMutError` via `Display` on its
// borrow-conflict panic path (`panic!("{}", err)`). That `{}` links ~1.8 KB of
// formatting machinery (`core::fmt::Formatter::pad` + `core::str::count::
// do_count_chars`) — significant on the watch's tiny code budget. `ReCell` keeps
// the same runtime borrow-checking (a reentrant misuse panics instead of being
// UB) but panics with a `&'static str` literal, which the pebble panic handler
// ignores, so no formatting code is referenced.

use core::cell::{Cell, UnsafeCell};
use core::ops::{Deref, DerefMut};

/// Borrow-checked interior mutability without the `core::fmt` cost.
///
/// `borrow` counter: `0` = free, `>0` = that many shared borrows, `-1` = mutably
/// borrowed (the same scheme `RefCell` uses).
pub struct ReCell<T> {
    borrow: Cell<isize>,
    value:  UnsafeCell<T>,
}

impl<T> ReCell<T> {
    pub const fn new(value: T) -> Self {
        ReCell { borrow: Cell::new(0), value: UnsafeCell::new(value) }
    }

    pub fn borrow(&self) -> Ref<'_, T> {
        let b = self.borrow.get();
        if b < 0 {
            borrow_fail();
        }
        self.borrow.set(b + 1);
        Ref { cell: self }
    }

    pub fn borrow_mut(&self) -> RefMut<'_, T> {
        if self.borrow.get() != 0 {
            borrow_fail();
        }
        self.borrow.set(-1);
        RefMut { cell: self }
    }
}

#[cold]
#[inline(never)]
fn borrow_fail() -> ! {
    // String literal — no `{}`/Display, so `Formatter::pad`/`do_count_chars`
    // are never linked. The pebble panic handler ignores the message anyway.
    panic!("taconite: state already borrowed")
}

pub struct Ref<'a, T> {
    cell: &'a ReCell<T>,
}

impl<T> Deref for Ref<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.cell.value.get() }
    }
}

impl<T> Drop for Ref<'_, T> {
    fn drop(&mut self) {
        self.cell.borrow.set(self.cell.borrow.get() - 1);
    }
}

pub struct RefMut<'a, T> {
    cell: &'a ReCell<T>,
}

impl<T> Deref for RefMut<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.cell.value.get() }
    }
}

impl<T> DerefMut for RefMut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.cell.value.get() }
    }
}

impl<T> Drop for RefMut<'_, T> {
    fn drop(&mut self) {
        self.cell.borrow.set(0);
    }
}
