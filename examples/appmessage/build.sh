#@IgnoreInspection BashAddShebang

target="thumbv7m-none-eabi"

# panic=immediate-abort lowers every panic (incl. bounds checks) straight to abort,
# so no format_args is built and the core::fmt tree gc's out (needs build-std).
# force-unwind-tables=no stops Rust emitting .ARM.exidx (drops the unwinder + shrinks .data/.bss).
export RUSTFLAGS="-C relocation-model=pie -C codegen-units=1 -C link-arg=--gc-sections -C link-arg=--build-id=sha1 -C link-arg=--emit-relocs -C debuginfo=2 -C panic=immediate-abort -C force-unwind-tables=no -Z unstable-options"

cargo --version
cargo build --target $target --release || exit 1

# Extract the self-contained crate-type=staticlib output into a FRESH dir each build.
PEBBLE_AR="$HOME/Library/Application Support/Pebble SDK/SDKs/current/toolchain/arm-none-eabi/bin/arm-none-eabi-ar"
LINK_OBJS="target/$target/release/link-objs"
rm -rf "$LINK_OBJS"; mkdir -p "$LINK_OBJS"
( cd "$LINK_OBJS" && "$PEBBLE_AR" x ../*.a )

# Compile TypeScript before waf bundles it
bunx pkts build
mkdir -p src/js
cp src/ts-build/index.js src/js/pebble-js-app.js
rm -rf src/ts-build

pebble build
