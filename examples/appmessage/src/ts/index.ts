// mirror your package.json keys here
export enum MessageKey {
    // First screen

    // Taconite
    taconite_window_id = 1413563215,
    ItemIndex = 1413563216,
    ItemTotal = 1413563217
}

// Signal the watch that the phone is ready; taconite will call
// on_messaging_initialized on all active screens so they send their window IDs.
Pebble.addEventListener('ready', async () => {
    await PebbleTS.sendAppMessage({ taconite_window_id: 0 })
})

// The watch sends its window ID from on_messaging_initialized; reply with the
// example message addressed to that window so taconite routes it to the right screen.
Pebble.addEventListener('appmessage', async (e: any) => {
    const windowId = e.payload.taconite_window_id
    if (windowId !== undefined && windowId !== 0) {
        await PebbleTS.sendAppMessage({
            taconite_window_id: windowId,
            App_ExampleKey: 'Hello from TypeScript!',
        })
    }
})
