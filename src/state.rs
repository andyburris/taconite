// taconite::state — the reactive value primitives.
//
// One currency flows through every layer: a `State<T>` — a cheap, clonable handle to
// a *readable* reactive value (a constant, a projection, a computed value, or a
// read-view of the writable root). The one writable source of truth is
// `MutableState<T>`; it converts into a `State<T>` for free, so anything that only
// reads accepts either via `impl Into<State<T>>`.
//
// The writable cell holds its value as an immutable **snapshot** `Rc<T>` (Compose's
// trick). `update` mutates it in place via `Rc::get_mut` — free while nobody else
// holds the snapshot (the norm: snapshots are built during paint and dropped). A
// `Snap<T>` is a `'static`, clonable, zero-copy pointer *into* a snapshot, so a bundle
// struct can *own* pointers to state fields (`HeaderData { title: Snap<CString>, … }`)
// and thus be a real `State<HeaderData>` — no lifetime, no per-field `Rc`, no clone.
//
// Two transforms, uniform across `State` and `MutableState`:
//   • `focus(|&T| -> &U)`  — zero-copy lens onto existing data (reads are alloc-free).
//   • `map(|&Snap<T>| -> U)` — build a new owned value; its fields can keep `Snap`
//     pointers via `snap.focus(..)`. `Snap<T>: Deref<Target = T>`, so plain reads
//     inside the builder look identical to a `&T`.

use alloc::rc::Rc;
use core::cell::{Ref, RefCell};
use core::ops::Deref;

use pebble::layer::{AsLayer, Layer};
use pebble::RawLayer;

// ── Snap<T> — a zero-copy pointer into an immutable snapshot ──────────────────────

/// One projection step: hand back a `&T` derived from an owner the impl keeps alive.
/// The owner (an `Rc`) makes the data immobile, so `get(&self) -> &T` — re-deriving
/// per access rather than storing a self-reference — is sound in fully safe Rust.
trait ProjectTo<T: ?Sized> {
    fn get(&self) -> &T;
}

enum SnapRepr<T: ?Sized> {
    Whole(Rc<T>),                  // the snapshot itself — no extra allocation
    Part(Rc<dyn ProjectTo<T>>),    // a `focus` chain rooted in some snapshot
}

/// A `'static`, clonable, zero-copy pointer into a state snapshot. Derefs to `&T` and
/// narrows with `focus`, so it reads exactly like a borrow — but it *owns* its slice
/// of the snapshot (via a refcounted root), which is what lets a bundle struct hold
/// `Snap` fields and still be a plain `'static` value.
///
/// Rule of thumb: snaps are for reads and for bundle fields. Holding one across an
/// `update` keeps the old snapshot alive, so that update can't reuse the cell and
/// fails loudly (see `MutableState::update`) — don't stash snaps long-term.
pub struct Snap<T: ?Sized>(SnapRepr<T>);

impl<T: ?Sized> Snap<T> {
    /// The whole snapshot as a `Snap` (allocation-free — reuses the snapshot `Rc`).
    pub(crate) fn whole(rc: Rc<T>) -> Snap<T> {
        Snap(SnapRepr::Whole(rc))
    }

    fn get(&self) -> &T {
        match &self.0 {
            SnapRepr::Whole(rc) => &**rc,
            SnapRepr::Part(p) => p.get(),
        }
    }
}

impl<T: ?Sized + 'static> Snap<T> {
    /// Narrow to a piece *inside* the snapshot — zero-copy, still `'static`. The whole
    /// snapshot stays alive behind the new `Snap`, so the `&U` is always valid.
    pub fn focus<U: ?Sized + 'static>(&self, f: impl Fn(&T) -> &U + 'static) -> Snap<U> {
        self.focus_rc(Rc::new(f))
    }

    // Shared spelling: takes the projection already boxed, so `State`'s stored lens
    // (an `Rc<dyn Fn>`) can compose into a `Snap` without re-wrapping.
    fn focus_rc<U: ?Sized + 'static>(&self, f: Rc<dyn Fn(&T) -> &U>) -> Snap<U> {
        Snap(SnapRepr::Part(Rc::new(FocusedSnap { parent: self.clone(), f })))
    }
}

impl<T: ?Sized> Clone for Snap<T> {
    fn clone(&self) -> Self {
        Snap(match &self.0 {
            SnapRepr::Whole(rc) => SnapRepr::Whole(rc.clone()),
            SnapRepr::Part(p) => SnapRepr::Part(p.clone()),
        })
    }
}

impl<T: ?Sized> Deref for Snap<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.get()
    }
}

struct FocusedSnap<T: ?Sized, U: ?Sized> {
    parent: Snap<T>,
    f: Rc<dyn Fn(&T) -> &U>,
}
impl<T: ?Sized, U: ?Sized> ProjectTo<U> for FocusedSnap<T, U> {
    fn get(&self) -> &U {
        // `&*self.f` is `&dyn Fn` (which *does* impl `Fn`); the returned `&U` borrows
        // `self` through the parent chain, all rooted in the live snapshot `Rc`.
        (&*self.f)(self.parent.get())
    }
}

// ── State<T> — read-only reactive value ──────────────────────────────────────────

/// The erased backing of a `State<T>`. `with` is the scoped, usually alloc-free read;
/// `snapshot` materializes a stable `Snap<T>` (what `map` hands its builder).
trait ReadSource<T> {
    fn with(&self, f: &mut dyn FnMut(&T));
    fn snapshot(&self) -> Snap<T>;
}

/// A clonable, read-only handle to a reactive value. Cloning is one `Rc` bump (the
/// value is never copied). Build one with `fixed`, `focus`, `map`, or from a
/// `MutableState` via `.into()`.
pub struct State<T> {
    inner: Rc<dyn ReadSource<T>>,
}

impl<T> Clone for State<T> {
    fn clone(&self) -> Self {
        State { inner: self.inner.clone() }
    }
}

impl<T: 'static> State<T> {
    /// A constant as a `State` — the degenerate (never-changes) case that lets a layer
    /// take `impl Into<State<T>>` and still accept a plain literal.
    pub fn fixed(value: T) -> State<T> {
        State { inner: Rc::new(FixedSrc(Rc::new(value))) }
    }

    /// Zero-copy lens: point at a piece of the existing value. Reads project in place
    /// (no allocation). Capturing is allowed, so index projection works:
    /// `state.focus(move |s| &s.games[i])`.
    pub fn focus<U: 'static>(&self, f: impl Fn(&T) -> &U + 'static) -> State<U> {
        let proj: Rc<dyn Fn(&T) -> &U> = Rc::new(f);
        State { inner: Rc::new(LensSrc { src: self.clone(), proj }) }
    }

    /// Build a new owned value from a snapshot. The builder gets `&Snap<T>` (Derefs to
    /// `&T`, so plain field reads look normal) and returns an owned `U`; `U`'s fields
    /// can keep zero-copy pointers into the snapshot via `s.focus(..)`:
    /// `state.map(|s| HeaderData { title: s.focus(|s| &s.name), on: s.flag })`.
    pub fn map<U: 'static>(&self, f: impl Fn(&Snap<T>) -> U + 'static) -> State<U> {
        let f: Rc<dyn Fn(&Snap<T>) -> U> = Rc::new(f);
        State { inner: Rc::new(MapSrc { src: self.clone(), f }) }
    }

    /// Read the value for the duration of `f` and return `f`'s result. The universal
    /// accessor — works for every variant, including computed ones built on the stack.
    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        // Adapt `FnOnce(&T) -> R` to the `&mut dyn FnMut(&T)` continuation; the source
        // calls it exactly once, so the take/Option dance is sound.
        let mut f = Some(f);
        let mut out = None;
        self.inner.with(&mut |t| out = Some((f.take().unwrap())(t)));
        out.unwrap()
    }
}

struct FixedSrc<T>(Rc<T>);
impl<T: 'static> ReadSource<T> for FixedSrc<T> {
    fn with(&self, f: &mut dyn FnMut(&T)) {
        f(&self.0)
    }
    fn snapshot(&self) -> Snap<T> {
        Snap::whole(self.0.clone())
    }
}

// A `MutableState`'s cell, read-only — the root every derived `State`/`Snap` grows
// from. The value is stored as a snapshot `Rc<T>` so it can be handed out directly.
struct CellSrc<T>(Rc<RefCell<Rc<T>>>);
impl<T: 'static> ReadSource<T> for CellSrc<T> {
    fn with(&self, f: &mut dyn FnMut(&T)) {
        let cell = self.0.borrow();
        f(&cell); // &Ref<Rc<T>> → &T
    }
    fn snapshot(&self) -> Snap<T> {
        Snap::whole(self.0.borrow().clone())
    }
}

struct LensSrc<A, T> {
    src: State<A>,
    proj: Rc<dyn Fn(&A) -> &T>,
}
impl<A: 'static, T: 'static> ReadSource<T> for LensSrc<A, T> {
    fn with(&self, out: &mut dyn FnMut(&T)) {
        let proj = &*self.proj;
        self.src.inner.with(&mut |a| out(proj(a))); // alloc-free projection
    }
    fn snapshot(&self) -> Snap<T> {
        // Compose this lens onto the source's snapshot — all data still lives in the
        // root `Rc`, only the projection chain grows.
        self.src.inner.snapshot().focus_rc(self.proj.clone())
    }
}

struct MapSrc<A, T> {
    src: State<A>,
    f: Rc<dyn Fn(&Snap<A>) -> T>,
}
impl<A: 'static, T: 'static> ReadSource<T> for MapSrc<A, T> {
    fn with(&self, out: &mut dyn FnMut(&T)) {
        let snap = self.src.inner.snapshot();
        let val = (&*self.f)(&snap);
        out(&val);
    }
    fn snapshot(&self) -> Snap<T> {
        // Rare (mapping on top of a computed value): materialize the built value into
        // its own `Rc` so it can back a `Snap`.
        let snap = self.src.inner.snapshot();
        Snap::whole(Rc::new((&*self.f)(&snap)))
    }
}

// ── MutableState<T> — the writable source of truth ───────────────────────────────

/// The writable reactive cell every read-only `State` ultimately derives from. Holds
/// the value as an immutable snapshot `Rc<T>`; `update` mutates it in place (via
/// `Rc::get_mut`) and repaints. It's a superset of `State<T>` — `.into()` gives a
/// read-view — and exposes the same `focus`/`map` combinators.
pub struct MutableState<T> {
    cell: Rc<RefCell<Rc<T>>>,
    rerender: Rc<dyn Fn()>,
}

impl<T> Clone for MutableState<T> {
    fn clone(&self) -> Self {
        MutableState { cell: self.cell.clone(), rerender: self.rerender.clone() }
    }
}

impl<T: 'static> MutableState<T> {
    /// Build a cell with an explicit repaint trigger (e.g. the screen's `view`).
    pub fn new(initial: T, rerender: impl Fn() + 'static) -> Self {
        MutableState { cell: Rc::new(RefCell::new(Rc::new(initial))), rerender: Rc::new(rerender) }
    }

    /// Wrap an *existing* shared cell with a repaint trigger. Used by `ScreenCtx` so
    /// the screen's `MutableState` and the layers' reads share one cell.
    pub fn from_shared(cell: Rc<RefCell<Rc<T>>>, rerender: impl Fn() + 'static) -> Self {
        MutableState { cell, rerender: Rc::new(rerender) }
    }

    /// Layer-local state: `update`s repaint `layer`. The raw-pointer/`mark_dirty`
    /// plumbing lives here, not in caller code — pass any layer you own (its root).
    pub fn for_layer(layer: &impl AsLayer, initial: T) -> Self {
        let raw: *mut RawLayer = layer.as_raw();
        // The SDK layer pointer is stable for the layer's life; `Layer::from_raw` is a
        // non-owning wrapper, so marking dirty through it is allocation-free.
        MutableState::new(initial, move || Layer::from_raw(raw).mark_dirty())
    }

    /// Read the value for the duration of `f`.
    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        f(&self.cell.borrow())
    }

    /// Borrow the value (a real guard, projected past the snapshot `Rc`).
    pub fn read(&self) -> Ref<'_, T> {
        Ref::map(self.cell.borrow(), |rc| &**rc)
    }

    /// The current immutable snapshot (one refcount bump). Held snapshots block the
    /// next `update` from mutating in place — see below.
    pub fn snapshot(&self) -> Rc<T> {
        self.cell.borrow().clone()
    }

    /// Mutate and repaint. Two guards, both `pbl_err!` + no-op instead of panicking:
    /// - the cell is already borrowed → you called this during a draw/read (a render);
    /// - a `Snap` of the current value is still alive → `Rc::get_mut` can't get unique
    ///   access. That only happens if you stored a snap across the update (don't).
    /// In normal flow snapshots are built during paint and dropped, so the refcount is
    /// 1 at update time and the mutation is in place, zero-copy.
    pub fn update(&self, f: impl FnOnce(&mut T)) {
        let mut cell = match self.cell.try_borrow_mut() {
            Ok(c) => c,
            Err(_) => {
                pbl_err!("taconite: state updated during render - move this out of a draw/read callback");
                return;
            }
        };
        match Rc::get_mut(&mut cell) {
            Some(t) => {
                f(t);
                drop(cell); // release before the repaint takes its own read borrow
                (self.rerender)();
            }
            None => {
                pbl_err!("taconite: state updated while a Snap of it is held - don't store Snaps across updates");
            }
        }
    }

    /// A read-only `State<T>` view of this cell (cheap `Rc` clone). Same as `.into()`.
    pub fn as_state(&self) -> State<T> {
        State { inner: Rc::new(CellSrc(self.cell.clone())) }
    }

    /// Zero-copy lens (see `State::focus`).
    pub fn focus<U: 'static>(&self, f: impl Fn(&T) -> &U + 'static) -> State<U> {
        self.as_state().focus(f)
    }

    /// Build an owned value from a snapshot (see `State::map`).
    pub fn map<U: 'static>(&self, f: impl Fn(&Snap<T>) -> U + 'static) -> State<U> {
        self.as_state().map(f)
    }
}

impl<T: 'static> From<MutableState<T>> for State<T> {
    fn from(m: MutableState<T>) -> State<T> {
        m.as_state()
    }
}

impl<T: 'static> From<&MutableState<T>> for State<T> {
    fn from(m: &MutableState<T>) -> State<T> {
        m.as_state()
    }
}

// `State<T>: Into<State<T>>` is automatic (identity), so layer params can be
// `impl Into<State<T>>` and accept a State, a &MutableState, or an owned MutableState.

// Follow-ups (not yet needed by the current screens):
//   - `MutableState::focus_mut` (writable lens into a field of a parent MutableState).
//   - non-capturing `focus`/`map` fast paths (fn-pointer projections, no `Rc` alloc).
