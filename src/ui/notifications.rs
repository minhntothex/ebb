use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_foundation::{NSBundle, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
    UNNotificationSound, UNUserNotificationCenter,
};

use crate::alerts::{QuotaAlert, format_alert_copy};

/// Native macOS notification sender backed by `UNUserNotificationCenter`.
///
/// Safely becomes a no-op when executed outside a proper `.app` bundle (for example
/// bare CLI binaries or `cargo test`) because Apple's `UNUserNotificationCenter`
/// throws `NSInternalInconsistencyException` when `NSBundle.mainBundle.bundleIdentifier` is nil.
#[derive(Debug, Default)]
pub struct NotificationSender {
    bundle_available: bool,
    auth_requested: bool,
}

impl NotificationSender {
    pub fn new() -> Self {
        let bundle_available = NSBundle::mainBundle().bundleIdentifier().is_some();
        Self {
            bundle_available,
            auth_requested: false,
        }
    }

    pub fn is_bundle_available(&self) -> bool {
        self.bundle_available
    }

    /// Lazily request macOS alert & sound permission once when running inside `Ebb.app`.
    pub fn ensure_authorized(&mut self) {
        if !self.bundle_available || self.auth_requested {
            return;
        }
        self.auth_requested = true;

        let center = UNUserNotificationCenter::currentNotificationCenter();
        let options = UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound;
        let handler = RcBlock::new(|_granted: Bool, _err: *mut NSError| {});
        center.requestAuthorizationWithOptions_completionHandler(options, &handler);
    }

    /// Deliver one low-quota alert immediately through macOS Notification Center.
    pub fn send(&mut self, alert: &QuotaAlert, now_secs: u64) {
        if !self.bundle_available {
            return;
        }
        self.ensure_authorized();

        let (title, body) = format_alert_copy(alert, now_secs);
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&title));
        content.setBody(&NSString::from_str(&body));
        let sound = UNNotificationSound::defaultSound();
        content.setSound(Some(&sound));

        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(&alert.notification_id()),
            &content,
            None,
        );
        let center = UNUserNotificationCenter::currentNotificationCenter();
        center.addNotificationRequest_withCompletionHandler(&request, None);
    }
}
