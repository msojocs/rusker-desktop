//! Application-wide operation feedback. Defer delivery so callers can use
//! this from ordinary entity updates as well as window event handlers.

use gpui_kit::{
    component::{
        notification::{Notification, NotificationDelivery},
        WindowExt as _,
    },
    Anchor, Context,
};

use super::RuskerApp;

impl RuskerApp {
    pub(super) fn notify_error(&self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.push_notification(Notification::error(message.into()).autohide(false), cx);
    }

    pub(super) fn notify_success(&self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.push_notification(Notification::success(message.into()), cx);
    }

    fn push_notification(&self, notification: Notification, cx: &mut Context<Self>) {
        let window = self.window_handle;
        cx.defer(move |cx| {
            _ = window.update(cx, |_, window, cx| {
                window.push_notification(
                    notification
                        .placement(Anchor::BottomRight)
                        .delivery(NotificationDelivery::InApp),
                    cx,
                );
            });
        });
    }
}
