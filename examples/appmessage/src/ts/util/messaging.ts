import { AppMessageData } from ".."

export async function sendItemList(
    items: AppMessageData[],
    windowId: number,
): Promise<void> {
    if(items.length === 0) {
        await PebbleTS.sendAppMessage({
            taconite_WindowId: windowId,
            taconite_ItemIndex: 0,
            taconite_ItemTotal: 0,
        })
        return
    }
    
    for (let i = 0; i < items.length; i++) {
        await PebbleTS.sendAppMessage({
            taconite_WindowId: windowId,
            taconite_ItemIndex: i,
            taconite_ItemTotal: items.length,
            ...items[i],
        }).catch(e => console.error(e))
    }
}
