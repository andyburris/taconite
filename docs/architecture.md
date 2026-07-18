# Architecture & intent

pebble-rust is a faithful translation of the Pebble C API into Rust patterns.
**Taconite is the idiomatic experience built on top of it**: reactive state, layers
that read that state, and a screen framework that wires them to the Pebble window
lifecycle. All the unsafe/Box/`RefCell`/raw-pointer plumbing stays down in
pebble-rust's wrappers; a taconite user — including the author of a custom layer —
writes only safe Rust. If something in taconite needs `unsafe`, that's a sign the
missing piece belongs in pebble-rust.

## The core model: pull, don't push

Pebble redraws **every** layer whenever any layer is marked dirty, so there is
nothing to gain from per-layer invalidation or change notifications. Taconite
leans into that with a strictly pull-based design:

- Data flows one way: a writable cell → derived read-only views → layers.
- Layers **read what they need at paint time and never own a copy** (so a layer
  is as cheap for a big struct as a small one — no clones, no buffers).
- A state `update` = mutate the cell + mark something dirty. The next paint pulls
  everything fresh through the read chain. There are no subscriptions anywhere.

Everything else in taconite — `State`, `Snap`, `AnimatedState`, the layer
constructors — is this one idea applied consistently.

## The state primitives (`state.rs`)

One currency flows through every API: **`State<T>`**, a cheap clonable handle to a
readable reactive value. Layer parameters take `impl Into<State<T>>`, so a
constant (`State::fixed`), a derived view, or a writable cell all plug into the
same slot.

| type | role |
|---|---|
| `MutableState<T>` | the one writable source of truth; a superset of `State<T>` (`.into()` / `as_state()` gives the read view) |
| `State<T>` | read-only handle; built with `fixed`, `focus`, `map`, `combine` |
| `Snap<T>` | a `'static`, clonable, zero-copy pointer *into* a snapshot |
| `AnimatedState<T>` | a `State<T>` whose reads interpolate toward a source `State<T>` (see below) |

### The snapshot model (why `Snap` exists)

The founding problem: how does a layer get a *whole struct's reference* out of
state — including structs **built on the fly** from several fields — without any
of the rejected approaches?

- **Dangling pointers** (the old C-style `set_text(ptr)`) — a footgun; a
  stack-built value dies at the end of the block.
- **Lifetimes on layers** — layers live inside the same struct as the state
  they'd borrow; Rust can't borrow-check self-referential structs, and lifetime
  parameters creep into every signature.
- **Layers owning copies** — forces `Clone` on every data struct and duplicates
  arbitrarily large data.
- **CPS/`get_data` callbacks** (`|s, read| read(&built)`) — worked standalone but
  can't be stored behind a `dyn` trait object without HRTB-variance errors, and
  can't be re-mapped by nested layers.

The answer (borrowed from Compose): `MutableState` stores its value as an
**immutable snapshot `Rc<T>`**. `update` mutates it *in place* via `Rc::get_mut`,
which is free whenever nobody else holds the snapshot — and in the normal flow
nobody does, because snapshots are only materialized during paint and dropped
right after. A `Snap<T>` is a refcounted pointer into that snapshot: it `Deref`s
to `&T` (reads look exactly like borrows), narrows with `focus`, and is `'static`
— so a bundle struct can *own* pointers to state fields
(`HeaderData { title: Snap<CString>, … }`) and still be a plain value with no
lifetime, no per-field `Rc` in the app's data model, and no clone.

Rule of thumb: snaps are for reads and for bundle fields. Holding one across an
`update` pins the old snapshot, and the update fails loudly instead of forking
the data.

### The transforms

- **`focus(|&T| -> &U)`** — a zero-copy lens onto existing data. Reads project in
  place, allocation-free. Prefer it whenever the target already exists inside `T`.
- **`map(|&Snap<T>| -> U)`** — build a new owned value per read. The builder gets
  a `&Snap<T>` (Derefs to `&T`, so field access reads normally) and its result's
  fields may keep zero-copy `Snap` pointers via `s.focus(..)`. This is how
  on-the-fly bundle structs work.
- **`State::combine(&a, &b, |sa, sb| ..)`** — one value from two cells. This is
  the intended way for a layer to read screen data *and* something else (e.g. a
  layer-local selection) at once.

### Failure behavior: warn loudly, never panic

`MutableState::update` guards two misuses — updating during a render (the cell is
already borrowed by a paint-time read) and updating while a `Snap` of the current
value is still alive. Both `pbl_err!` an explanatory message and no-op, in the
spirit of React/Svelte dev warnings: on a watch, a diagnosable wrong frame beats
a crash.

## AnimatedState (`animation.rs`)

Animation is **declarative, not imperative**: you never "start" an animation.
`AnimatedState::new(source, rerender)` (or `for_layer`) wraps a source `State<T>`
and yields, via `as_state()`, a `State<T>` whose reads smoothly chase the
source's value. Because it *is* a `State`, everything composes: feed it to any
layer, `focus`/`map` it, combine it.

Like everything else it is pull-based — there is no change notification:

- **Retargeting happens lazily at read (= paint) time.** When a read finds the
  source differs from the current target, it starts an animation; each frame
  stores the interpolated value and calls `rerender` (typically marking a layer
  dirty), so the next paint pulls the next step.
- **The first read snaps** — animating "from nothing" is meaningless. Corollary
  (learned the hard way): *don't read the animated value while the source is
  still a placeholder*, or the placeholder becomes the animation's baseline and
  the first real value visibly animates out of it. Gate the read on the data
  being real (see the custom-layer example below); values must start at their
  initial position, not animate to it on launch.
- **A fresh pebble `Animation` per retarget.** SDK animations are immutable once
  scheduled and the firmware auto-destroys them when they stop, so a stopped
  handle can never be reconfigured or reused. Assigning the new one drops any
  in-flight predecessor. (The firmware's "Animation N does not exist" log line
  comes from destroying an already-auto-destroyed handle — harmless noise.)
- The frame closure holds a `Weak` back to the shared bookkeeping so the
  animation doesn't keep its own state alive in a cycle.

## Layers (`layer.rs`)

### The layer contract

1. A layer reads its data through a `State` **at paint time** and owns no copy.
2. A layer is **not tied to any screen**. It takes `impl Into<State<T>>` — that's
   the whole point of the `State` currency. Never reach into a specific screen's
   `Layers` struct or state type from inside a reusable layer.
3. Mirroring React/Compose, each layer chooses which state it **controls** vs.
   **leaves controllable**: take state the caller should own as a parameter;
   create layer-local state internally with `MutableState::for_layer(&root, ..)`
   (updates repaint just that layer's root — remembering that Pebble repaints
   everything anyway, this is about wiring, not efficiency).
4. Constructors do the composition; `render()` just marks dirty.

### The built-ins

- **`Draw`** — the reference primitive: bounds + `impl Into<State<T>>` + a painter
  `Fn(&mut GContext, &T, GRect)`. The painter runs against a scoped read of the
  state; pebble-rust's `DrawLayer` owns the boxed painter and the update-proc
  plumbing.
- **`Text`** — deliberately *not* a wrapper over the C `TextLayer`. The C layer
  stores a `char*`, which forces an owned buffer and re-copying on every change.
  Since the pointer is only ever used during redraw, taconite's `Text` instead
  draws with `graphics_draw_text` inside its own update proc, pulling the string
  through the `State` chain at paint time — no buffer, no copy, no stored
  pointer. The `TextContent` trait lets one `Text::new` accept a
  `State<CString>`, a `State<Snap<CString>>` (what `map` produces), a bare
  `Snap<CString>`, or a `&'static CStr` literal.
- **`Menu<R>`** — one constructor over one `State<R>`. Read/layout/paint
  callbacks receive `&R`, read scoped at call time. The **input** callbacks
  (`select_click`, `selection_changed`, …) take no state parameter by design:
  they *capture* whatever handles they need
  (`let s = ctx.state(); move |_, idx| s.update(..)`) — a captured handle holds
  no read borrow, so updating the same cell from inside the callback is safe.
  A menu that needs two cells (screen data + selection) takes a
  `State::combine` of them: combining at the state layer replaced the old
  zoo of `Menu::stateful`/`Menu::split` constructors.
- **`ContentIndicatorLayer`** — adapts pebble-rust's non-layer
  `ContentIndicator` into the layer model: a root layer hosting two strips, one
  indicator, driven by a `State<bool>` per direction.

`StatusBarLayer` has no data binding, so it's re-exported from pebble-rust as-is
— taconite wraps only where the reactive model adds something.

### Writing a custom layer

The canonical shape (this is pebble-transit's arrival wheel, the layer the whole
model was designed around):

```rust
struct ArrivalWheelLayer {
    // Drop order: `_angle` before `inner`/`root`, cancelling the animation first.
    _angle: AnimatedState<i32>,
    inner:  Draw,
    root:   Layer,
}

impl ArrivalWheelLayer {
    pub fn new(bounds: GRect, state: impl Into<State<ScreenState>>) -> Self {
        let state = state.into();
        let root = Layer::new(bounds);

        // Layer-internal animated value, repainting this layer's own root.
        let angle = AnimatedState::for_layer(state.map(|s| target_angle_for(s).unwrap_or(0)), &root);

        // The painter reads its data param and nests a scoped read of the angle —
        // both pulled fresh at paint. The angle read is gated so a loading
        // placeholder can never become the animation baseline.
        let angle_state = angle.as_state();
        let inner = Draw::new(bounds, &state, move |gctx, s, frame| {
            let a = match target_angle_for(s) {
                Some(_) => angle_state.with(|a| *a),
                None => 0,
            };
            wheel_draw(gctx, s, a, frame);
        });
        root.add_child(&inner);

        ArrivalWheelLayer { _angle: angle, inner, root }
    }
}

impl AsLayer for ArrivalWheelLayer {
    fn as_raw(&self) -> *mut RawLayer { self.root.as_raw() }
}
```

The pattern in words: a plain `Layer` root; children added to it; internal state
(`MutableState::for_layer` / `AnimatedState::for_layer`) targets the root;
painter closures capture cloned `State` handles; the struct holds everything for
ownership and drop order; `AsLayer` makes it composable like any other layer.

## Screens & the window lifecycle (`lib.rs`)

A screen is a zero-size type implementing `ScreenFns` (`State` / `TempState` /
`Layers` associated types plus lifecycle methods). Per pushed screen, taconite
boxes a typed `ScreenCtx<S>` and a table of monomorphized trampolines
(`ScreenBundle`), stores the bundle in the window's user data, and registers it
in a router keyed by a per-window ID. App code only ever sees `&S`/`&mut S`
through the ctx — `Rc`, `RefCell`, and raw pointers never surface.

- `ctx.state()` returns the screen's root `MutableState<S>`; its repaint trigger
  re-runs `Sc::view` against the current state. `view` pushes state into layers
  that aren't purely pull-based (e.g. calling `menu.render()` after data
  changes); pull-based layers need nothing there.
- `create_window` builds the `Layers` struct from taconite layer constructors,
  passing `ctx.state()`-derived states down.
- The `ScreenCtx` owns its `Window`; unload frees layers, ctx, and temp in order.

### Messaging: staged loads, atomic commits

Data arriving over multiple AppMessages must never render half-built — a paint
can happen between any two messages (an animation frame, a tick). So each screen
gets an off-screen **`TempState`** (default `()`) that *nothing draws from*:

- `ctx.update_temp(|t| ..)` accumulates chunks; never re-renders.
- `ctx.commit(|s, t| ..)` atomically folds the staging buffer into the real
  state (take/`mem::take` — no clones), resets the buffer to `Default`, and
  re-renders **once**.
- `handle_list_message` fills a `Vec<Option<T>>` from indexed chunks
  (`ItemIndex`/`ItemTotal` keys) and returns completeness — the usual `commit`
  trigger.

Routing uses reserved `TaconiteMessageKey`s ("TACO" ASCII namespace). A message
with `WindowId = 0` is the phone-ready sentinel: PebbleKit JS starts *after* the
watch app, so screens must not send until it fires. `on_messaging_initialized`
runs then (or immediately on load if the phone was already ready) — put
subscribe/init sends there, never in `on_create`.

## Size discipline

Taconite targets aplite, where the whole app (`.text` included) fits in a 24 KB
RAM budget, so abstraction costs are measured, not assumed:

- **`dyn` = vtable = every method forever.** Each distinct closure or source
  type behind `Rc<dyn ..>` costs a RAM vtable and codegens *both* `with` and
  `snapshot` paths. Prefer `focus` (alloc-free, no builder) over `map`; keep
  `map`/`combine` builders small; don't create derived states you don't use.
- Diagnostic strings (`pbl_err!`) are rodata on the watch — keep them terse.
- When a size regression appears, measure symbol-by-symbol before redesigning;
  past experience says savings surprise in both directions. (And an
  optimization-level-correlated *crash* is usually latent UB being re-rolled by
  codegen, not a broken flag.)

## Direction (not yet built)

The API is shaped so a future macro layer can expand to plain constructor calls:

- **`custom_layer!` / `match!` / `if!`** — a declarative tree where a *setup*
  pass creates the layers of every branch and an *update* pass only toggles
  branch visibility; all data movement stays in the `State` chain, so the update
  pass carries no data. The existing constructors (`impl Into<State<T>>`
  everywhere, layer-local state via `for_layer`) are the expansion target —
  that's why nothing takes screen-specific context.
- **`split()`** — per-field `State`s destructured from a struct state.
- **`MutableState::focus_mut`** — a writable lens into a field of a parent cell.
- **`TempState` as a `State`** — `update_temp` is transitional; the intended end
  state folds staging into the same primitive family.
