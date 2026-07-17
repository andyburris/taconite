#@IgnoreInspection BashAddShebang

target="thumbv7m-none-eabi"

# panic=immediate-abort lowers every panic (incl. bounds checks) straight to abort,
# so no format_args is built and the core::fmt tree gc's out (needs build-std).
# force-unwind-tables=no stops Rust emitting .ARM.exidx (drops the unwinder + shrinks .data/.bss).
export RUSTFLAGS="-Zlocation-detail=none -C relocation-model=pie -C codegen-units=1 -C link-arg=--gc-sections -C link-arg=--build-id=sha1 -C link-arg=--emit-relocs -C debuginfo=2 -C panic=immediate-abort -C force-unwind-tables=no -Z unstable-options"

cargo --version
cargo build --target $target --release || exit 1

# Extract the self-contained crate-type=staticlib output into a FRESH dir each build.
TOOLCHAIN="$HOME/Library/Application Support/Pebble SDK/SDKs/current/toolchain/arm-none-eabi/bin"
PEBBLE_AR="$TOOLCHAIN/arm-none-eabi-ar"
LINK_OBJS="target/$target/release/link-objs"
rm -rf "$LINK_OBJS"; mkdir -p "$LINK_OBJS"
( cd "$LINK_OBJS" && "$PEBBLE_AR" x ../*.a )

# Strip .ARM.exidx/.extab from the extracted objects. The SDK linker script doesn't
# place them, so ld inserts them as orphans between .header and .note.gnu.build-id —
# the script warns that area's layout must not change (the firmware reads fixed
# offsets from the app header region; shifting the note corrupts app launch).
for o in "$LINK_OBJS"/*.o; do
  "$TOOLCHAIN/arm-none-eabi-objcopy" --remove-section '.ARM.exidx*' --remove-section '.ARM.extab*' "$o"
done

# Compile TypeScript before waf bundles it
bunx pkts build
mkdir -p src/js
cp src/ts-build/index.js src/js/pebble-js-app.js
rm -rf src/ts-build

pebble build
