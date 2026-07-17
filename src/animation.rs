// taconite::animation — reactive animated values.
//
// `AnimatedState<T>` wraps a source `State<T>` and produces a `State<T>` whose reads
// smoothly interpolate toward the source's value. It is *pull-based* like everything
// else: retargeting happens lazily at read (= paint) time — no change-notification
// system. When the source's value differs from the current target, the read starts (or
// replaces) a pebble `Animation`; each animation frame stores the new interpolated
// value and marks the layer dirty, so the next paint reads the next step. When source
// and target agree, no animation runs.

use alloc::rc::Rc;
use core::cell::{Cell, RefCell};

use pebble::animation::{Animation, AnimationCurve, AnimationProgress, ANIMATION_NORMALIZED_MAX};
use pebble::layer::{AsLayer, LayerRef};

use crate::state::{ReadSource, Snap, State};

/// A value that can be smoothly interpolated by an [`AnimatedState`].
pub trait Interpolatable: Copy + PartialEq {
    fn interpolate(from: Self, to: Self, progress: AnimationProgress) -> Self;
}

impl Interpolatable for i32 {
    fn interpolate(from: i32, to: i32, progress: AnimationProgress) -> i32 {
        // Shift progress right by 1 (0..32767) so delta*p fits in i32 when
        // |delta| <= TRIG_MAX_ANGLE.
        let delta = to - from;
        let p = (progress >> 1) as i32;
        from + delta * p / (ANIMATION_NORMALIZED_MAX as i32 >> 1)
    }
}

const DEFAULT_DURATION_MS: u32 = 300;

struct Book<T> {
    from:    Option<T>,
    current: Option<T>,
    target:  Option<T>,
    anim:    Option<Animation>,
}

struct Shared<T: Interpolatable> {
    source:   State<T>,
    book:     RefCell<Book<T>>,
    rerender: Rc<dyn Fn()>,
    duration: Cell<u32>,
    curve:    Cell<AnimationCurve>,
}

impl<T: Interpolatable + 'static> Shared<T> {
    // Read the source's target and reconcile the animation, returning the value to
    // show right now. Called at paint time through the `ReadSource`. All bookkeeping
    // lives in `book` — the source state cell is only *read*, never written, so this
    // never trips the "state updated during render" guard.
    fn sync(self: &Rc<Self>) -> T {
        let new_target = *self.source.snapshot();
        let mut book = self.book.borrow_mut();
        match book.target {
            None => {
                // First read: snap straight to the value (no animation from nothing).
                book.from = Some(new_target);
                book.current = Some(new_target);
                book.target = Some(new_target);
                new_target
            }
            Some(t) if t == new_target => book.current.unwrap_or(new_target),
            Some(_) => {
                // Target moved: animate from where we are now toward it.
                let from = book.current.unwrap_or(new_target);
                book.from = Some(from);
                book.current = Some(from);
                book.target = Some(new_target);

                // A fresh `Animation` per retarget: Pebble animations are immutable
                // once scheduled and are auto-destroyed by the firmware when they
                // stop, so a scheduled animation can't be reconfigured or reused.
                // Assigning replaces (and drops) any in-flight one.
                //
                // The frame closure holds a `Weak` so `Shared` → `book` → `anim` →
                // closure isn't a strong cycle (which would leak the animation).
                let weak = Rc::downgrade(self);
                let anim = Animation::new(move |progress| {
                    if let Some(sh) = weak.upgrade() {
                        {
                            let mut b = sh.book.borrow_mut();
                            if let (Some(f), Some(tg)) = (b.from, b.target) {
                                b.current = Some(T::interpolate(f, tg, progress));
                            }
                        }
                        (sh.rerender)();
                    }
                });
                anim.set_duration(self.duration.get());
                anim.set_curve(self.curve.get());
                anim.schedule();
                book.anim = Some(anim);
                from
            }
        }
    }
}

struct AnimatedSrc<T: Interpolatable> {
    shared: Rc<Shared<T>>,
}
impl<T: Interpolatable + 'static> ReadSource<T> for AnimatedSrc<T> {
    fn with(&self, out: &mut dyn FnMut(&T)) {
        let current = self.shared.sync();
        out(&current);
    }
    fn snapshot(&self) -> Snap<T> {
        let current = self.shared.sync();
        Snap::whole(Rc::new(current))
    }
}

/// A reactive value that smoothly animates toward its source. Reads (and thus every
/// layer built from `as_state()`/`focus`/`map`) yield the current interpolated value.
pub struct AnimatedState<T: Interpolatable> {
    shared: Rc<Shared<T>>,
}

impl<T: Interpolatable + 'static> AnimatedState<T> {
    /// Animate toward `source`, calling `rerender` each frame (typically to mark a
    /// layer dirty). Prefer [`AnimatedState::for_layer`] for the common case.
    pub fn new(source: impl Into<State<T>>, rerender: impl Fn() + 'static) -> Self {
        AnimatedState {
            shared: Rc::new(Shared {
                source:   source.into(),
                book:     RefCell::new(Book { from: None, current: None, target: None, anim: None }),
                rerender: Rc::new(rerender),
                duration: Cell::new(DEFAULT_DURATION_MS),
                curve:    Cell::new(AnimationCurve::EaseInOut),
            }),
        }
    }

    /// Animate toward `source`, repainting `layer` each frame.
    pub fn for_layer(source: impl Into<State<T>>, layer: &impl AsLayer) -> Self {
        let raw = layer.as_raw();
        AnimatedState::new(source, move || LayerRef::from_raw(raw).mark_dirty())
    }

    pub fn with_duration(self, ms: u32) -> Self {
        self.shared.duration.set(ms);
        self
    }

    pub fn with_curve(self, curve: AnimationCurve) -> Self {
        self.shared.curve.set(curve);
        self
    }

    /// A read-only `State<T>` yielding the current interpolated value.
    pub fn as_state(&self) -> State<T> {
        State::from_source(Rc::new(AnimatedSrc { shared: self.shared.clone() }))
    }

    /// Zero-copy lens over the animated value (see `State::focus`).
    pub fn focus<U: 'static>(&self, f: impl Fn(&T) -> &U + 'static) -> State<U> {
        self.as_state().focus(f)
    }

    /// Build an owned value from the animated value (see `State::map`).
    pub fn map<U: 'static>(&self, f: impl Fn(&Snap<T>) -> U + 'static) -> State<U> {
        self.as_state().map(f)
    }
}

impl<T: Interpolatable + 'static> From<&AnimatedState<T>> for State<T> {
    fn from(a: &AnimatedState<T>) -> State<T> { a.as_state() }
}
impl<T: Interpolatable + 'static> From<AnimatedState<T>> for State<T> {
    fn from(a: AnimatedState<T>) -> State<T> { a.as_state() }
}
