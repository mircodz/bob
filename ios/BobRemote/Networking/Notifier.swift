import Foundation
import UserNotifications

/// Thin wrapper over local user notifications. This is the single call site for
/// "notify me" so a remote (APNs) path can be added later without touching the
/// callers: `Notifier.notify(...)` would just gain a server-push branch.
///
/// Free-Apple-ID limitation: local notifications only fire while the app is
/// running (foreground or iOS's brief background tail, ~30s). A suspended or
/// closed app stays silent until APNs push is added (needs a paid account).
enum Notifier {
    /// Ask once for permission. Safe to call on every connect.
    static func requestAuthorization() {
        UNUserNotificationCenter.current().requestAuthorization(
            options: [.alert, .sound, .badge]
        ) { _, _ in }
    }

    /// Post a local notification. `id` de-dupes (a later post with the same id
    /// replaces the pending one).
    static func notify(id: String, title: String, body: String) {
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        content.sound = .default
        let request = UNNotificationRequest(
            identifier: id, content: content, trigger: nil)  // nil = deliver now
        UNUserNotificationCenter.current().add(request)
    }
}
