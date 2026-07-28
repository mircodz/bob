import SwiftUI

@main
struct BobRemoteApp: App {
    var body: some Scene {
        WindowGroup {
            RootView()
        }
    }
}

/// Switches between the connection screen and the active session.
struct RootView: View {
    @StateObject private var store = SessionStore()
    @Environment(\.scenePhase) private var scenePhase
    // Empty defaults; @AppStorage remembers what you enter after the first
    // connect. (Don't ship real relay URLs / tokens in source.)
    @AppStorage("relay") private var relay = ""
    @AppStorage("session") private var session = ""
    @AppStorage("token") private var token = ""

    var body: some View {
        ZStack {
            Theme.bg.ignoresSafeArea()
            if store.connected {
                ActiveSessionView(store: store)
            } else {
                ConnectView(relay: $relay, session: $session, token: $token) {
                    store.connect(relay: relay, session: session, token: token)
                }
                .overlay(alignment: .bottom) {
                    if let err = store.lastError {
                        Text(err).font(Theme.sf(12)).foregroundStyle(Theme.removed)
                            .padding(Theme.s3)
                    }
                }
            }
        }
        .tint(Theme.accent)
        .onChange(of: scenePhase) { _, phase in
            store.scenePhaseChanged(phase)
        }
    }
}

/// A light connect screen matching the 1b visual language (grouped card fields).
struct ConnectView: View {
    @Binding var relay: String
    @Binding var session: String
    @Binding var token: String
    let onConnect: () -> Void

    var body: some View {
        VStack(spacing: Theme.s4) {
            Spacer()
            VStack(spacing: Theme.s1) {
                Image(systemName: "cpu.fill")
                    .font(.system(size: 40)).foregroundStyle(Theme.accent)
                Text("Bob").font(Theme.sf(30, .semibold)).foregroundStyle(Theme.text)
                Text("remote control for your coding agent")
                    .font(Theme.sf(13)).foregroundStyle(Theme.secondary)
            }
            .padding(.bottom, Theme.s2)

            VStack(spacing: 0) {
                field("Relay", text: $relay, secure: false)
                Divider().overlay(Theme.hairline)
                field("Session", text: $session, secure: false)
                Divider().overlay(Theme.hairline)
                field("Token", text: $token, secure: true)
            }
            .background(Theme.surface)
            .clipShape(RoundedRectangle(cornerRadius: Theme.r14))
            .cardShadow()

            Button(action: onConnect) {
                Text("Connect").font(Theme.sf(15, .semibold)).foregroundStyle(.white)
                    .frame(maxWidth: .infinity).padding(.vertical, Theme.s3h)
                    .background(Theme.accent)
                    .clipShape(RoundedRectangle(cornerRadius: Theme.r12))
            }
            .disabled(relay.isEmpty || session.isEmpty)
            .opacity(relay.isEmpty || session.isEmpty ? 0.4 : 1)

            Spacer()
        }
        .padding(Theme.s4)
    }

    @ViewBuilder private func field(_ label: String, text: Binding<String>, secure: Bool) -> some View {
        HStack(spacing: Theme.s3) {
            Text(label).font(Theme.sf(14)).foregroundStyle(Theme.secondary)
                .frame(width: 70, alignment: .leading)
            Group {
                if secure { SecureField("", text: text) } else { TextField("", text: text) }
            }
            .font(Theme.sf(14)).foregroundStyle(Theme.text)
            .textInputAutocapitalization(.never).autocorrectionDisabled()
        }
        .padding(.horizontal, Theme.s3h).padding(.vertical, Theme.s3)
    }
}
