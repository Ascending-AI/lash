async def demo():
    notify = True
    if notify:
        await display.show_message(text="Inside the notification branch")
    else:
        await display.show_message(text="Notifications disabled")
    return None
