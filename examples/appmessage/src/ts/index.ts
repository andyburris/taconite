// mirror your package.json keys here
export enum MessageKey {
    // First screen

    // Taconite
    taconite_WindowId = 1413563215,
    taconite_WindowType = 1413563216,
    taconite_ItemIndex = 1413563217,
    taconite_ItemTotal = 1413563218,
    taconite_SubscriptionEvent = 1413563219,
}
export type AppMessageData = Partial<Record<keyof typeof MessageKey, string | number>>
export enum SubscriptionEvent { Subscribe = 0, Unsubscribe = 1 }


// Signal the watch that the phone is ready; taconite will call
// on_messaging_initialized on all active screens so they send their window IDs.
Pebble.addEventListener('ready', async () => {
    await PebbleTS.sendAppMessage({ taconite_WindowId: 0 })
})

// The watch sends its window ID from on_messaging_initialized; reply with the
// example message addressed to that window so taconite routes it to the right screen.
Pebble.addEventListener('appmessage', async (e: { payload: AppMessageData }) => {
    if (typeof e.payload.taconite_WindowId === "number" && typeof e.payload.taconite_WindowType === "number") {
        await PebbleTS.sendAppMessage({
            taconite_WindowId: e.payload.taconite_WindowId,
            App_ExampleKey: 'Hello from TypeScript!',
        })
    } else {
        console.error(`got an appmessage an invalid windowId or an invalid windowType, payload = ${JSON.stringify(e.payload)}`)
    }
})
