#@IgnoreInspection BashAddShebang

target="thumbv7m-none-eabi"

export RUSTFLAGS="-C relocation-model=pie -C codegen-units=1 -C link-arg=--gc-sections -C link-arg=--build-id=sha1 -C link-arg=--emit-relocs -C debuginfo=2 -C panic=abort"

# Build the project through Cargo
cargo --version
cargo build --target $target --release || exit 1

cd target/$target/release/deps

# Extract the archive (use GNU ar from Pebble SDK; macOS BSD ar can't handle GNU-format archives)
PEBBLE_AR="$HOME/Library/Application Support/Pebble SDK/SDKs/current/toolchain/arm-none-eabi/bin/arm-none-eabi-ar"
"$PEBBLE_AR" x *.a

# Remove all the mess produced by Rust (shouldn't be a problem if you use the 'compiler-builtins' crate).
find . -type f ! -name '*.rcgu.o' -delete

cd -

# Compile TypeScript before waf bundles it
bunx pkts build
mkdir -p src/js
cp src/ts-build/index.js src/js/pebble-js-app.js
rm -rf src/ts-build

# Build through waf
pebble build
