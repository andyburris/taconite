// taconite::state — the reactive state handle.
//
// `State<S>` is the one currency layers and callbacks pass around: a cheap,
// clonable handle that can read the screen's state, mutate it and repaint, and
// narrow itself to a sub-field (`split_field`). It sits on top of the screen's
// single `Rc<RefCell<State>>`; the `Project` trait is the type-erased seam that
// lets a handle focused on one field hide the parent struct from its type.

use alloc::rc::Rc;
use core::cell::{Ref, RefCell, RefMut};

/// Projected, parent-type-erased access to one slice `S` of a screen's state.
///
/// This is the `dyn`-erased seam between a `State<S>` and the concrete
/// `RefCell<Root>` it ultimately borrows from: an implementor knows the parent
/// type, the trait object hides it. Both methods hand back *projected* guards
/// (built with `Ref::map`), so a handle focused on one field still reads/writes
/// straight through the parent's single borrow — zero-copy, no second cell.
trait Project<S> {
    /// Borrow the root and narrow to `&S` for the life of the returned guard.
    fn read(&self) -> Ref<'_, S>;
    /// Like `read` but mutable; `None` (instead of a panic) when the cell is
    /// already borrowed, so the caller can warn-and-skip during a render.
    fn try_write(&self) -> Option<RefMut<'_, S>>;
}

/// The root projector: the whole state struct, no narrowing. The identity end of
/// every projection chain — `Focus` layers stack on top of this.
struct Root<S> {
    cell: Rc<RefCell<S>>,
}

impl<S> Project<S> for Root<S> {
    fn read(&self) -> Ref<'_, S> {
        self.cell.borrow()
    }
    fn try_write(&self) -> Option<RefMut<'_, S>> {
        // `.ok()`: a conflict is reported as `None`, not a panic — `update`
        // turns that into a logged no-op rather than killing the app.
        self.cell.try_borrow_mut().ok()
    }
}

/// A projector focused from a parent `P` down to one part `F` via a lens (the
/// `get`/`get_mut` accessor pair). `split_field` stacks these: each `Focus`
/// wraps the parent projector and threads the borrow through
/// `Ref::map`/`RefMut::map`, so only `F`'s type surfaces while the real cell
/// stays the root's. Field accessors are non-capturing, hence plain `fn`
/// pointers (no allocation).
struct Focus<P, F> {
    parent:  Rc<dyn Project<P>>,
    get:     fn(&P) -> &F,
    get_mut: fn(&mut P) -> &mut F,
}

impl<P: 'static, F> Project<F> for Focus<P, F> {
    fn read(&self) -> Ref<'_, F> {
        Ref::map(self.parent.read(), self.get)
    }
    fn try_write(&self) -> Option<RefMut<'_, F>> {
        self.parent.try_write().map(|m| RefMut::map(m, self.get_mut))
    }
}

/// A clonable handle to one slice of screen state, paired with the trigger that
/// repaints the screen.
///
/// `inner` is parent-type-erased (`dyn Project`) so a handle focused on a single
/// field doesn't drag the whole state struct into its type — that's what lets
/// `split_field` hand a sub-layer a narrow `State<Field>`. Cloning is two `Rc`
/// refcount bumps; every layer that might read or write state holds one.
pub struct State<S> {
    inner:    Rc<dyn Project<S>>,
    rerender: Rc<dyn Fn()>,
}

impl<S> Clone for State<S> {
    fn clone(&self) -> Self {
        State { inner: self.inner.clone(), rerender: self.rerender.clone() }
    }
}

impl<S: 'static> State<S> {
    /// Build the root handle over a screen's state cell + its repaint trigger.
    /// taconite-internal — app code gets a `State` from `ScreenCtx::state`.
    pub(crate) fn root(cell: Rc<RefCell<S>>, rerender: Rc<dyn Fn()>) -> Self {
        State { inner: Rc::new(Root { cell }), rerender }
    }

    /// Borrow the state for reading. The returned guard holds the borrow until it
    /// drops — read what you need at paint time and let it go.
    pub fn read(&self) -> Ref<'_, S> {
        self.inner.read()
    }

    /// Read via a closure (handy when you don't want to name the guard). Mirrors
    /// stdlib `thread_local!`'s `LocalKey::with`: runs `f` while the borrow is
    /// held and returns `f`'s result.
    pub fn with<R>(&self, f: impl FnOnce(&S) -> R) -> R {
        f(&self.inner.read())
    }

    /// Mutate the state and repaint the screen — the reactive setter every
    /// writable callback uses.
    ///
    /// If the cell is already borrowed (e.g. called from inside a draw/read
    /// callback, which runs *during* a render's read borrow) it logs and
    /// **no-ops** instead of panicking — the Svelte/React "you mutated during
    /// render" guard. The write borrow is dropped before `rerender`, since the
    /// repaint takes its own read borrow.
    pub fn update(&self, f: impl FnOnce(&mut S)) {
        match self.inner.try_write() {
            Some(mut guard) => {
                f(&mut guard);
                drop(guard);
                (self.rerender)();
            }
            None => {
                pbl_err!("taconite: state updated during render - move this out of a draw/read callback");
            }
        }
    }

    /// Mutate without repainting, returning the closure's value. For off-screen
    /// staging (e.g. `update_temp`) where nothing draws from this state and a
    /// return value is needed; a conflict here is a real bug, so it panics.
    pub fn mutate<R>(&self, f: impl FnOnce(&mut S) -> R) -> R {
        let mut guard = self.inner.try_write().expect("taconite: state already borrowed");
        f(&mut guard)
    }

    /// Narrow this handle to one field `F` (a lens = `get`/`get_mut` pair),
    /// sharing the same cell and repaint trigger. Lets a sub-layer take a
    /// `State<Field>` without naming the whole parent struct.
    pub fn split_field<F: 'static>(
        &self,
        get: fn(&S) -> &F,
        get_mut: fn(&mut S) -> &mut F,
    ) -> State<F> {
        State {
            inner:    Rc::new(Focus { parent: self.inner.clone(), get, get_mut }),
            rerender: self.rerender.clone(),
        }
    }
}
