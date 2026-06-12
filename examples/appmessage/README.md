### Quick Start
1. [Install the Pebble SDK and Rust target](#initial-setup) if you haven't already
2. Install dependencies
    ```bash
    npm i
    ```
3. Build and run
    ```bash
    npm run build
    pebble install --emulator <aplite, basalt, chalk, etc.>
    ```


### Initial setup
1. Install the Pebble SDK

2. Install the `thumbv7m-none-eabi` Rust target
    ```bash
    rustup target add thumbv7m-none-eabi
    ```