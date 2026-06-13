import { MessageKey } from ".."

export async function sendItemList(
    items: Partial<Record<keyof typeof MessageKey, string | number>>[],
    windowId: number,
): Promise<void> {
    if(items.length === 0) {
        await PebbleTS.sendAppMessage({
            taconite_window_id: windowId,
            ItemIndex: 0,
            ItemTotal: 0,
        })
        return
    }
    
    for (let i = 0; i < items.length; i++) {
        await PebbleTS.sendAppMessage({
            taconite_window_id: windowId,
            ItemIndex: i,
            ItemTotal: items.length,
            ...items[i],
        }).catch(e => console.error(e))
    }
}
