import SwiftUI
import UIKit

/// Active Session (option 1b) — the chat + tool-call stream, rendered as
/// light-mode grouped iOS cards. Header (fixed) / stream (scrolls) / composer.
struct ActiveSessionView: View {
    @ObservedObject var store: SessionStore
    @State private var input = ""
    @State private var drawerOpen = false
    @State private var menuOpen = false
    @State private var followTail = true

    var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
                header
                stream
                composer
            }
            .background(Theme.bg)
            .toolbar(.hidden, for: .navigationBar)
        }
        .sheet(item: $store.pending) { ask in
            ApprovalSheet(store: store, ask: ask)
                .presentationDetents([.medium, .large])
        }
        .fullScreenCover(isPresented: $drawerOpen) {
            SessionBrowser(store: store, isOpen: $drawerOpen)
        }
        .sheet(isPresented: $menuOpen) {
            SessionMenu(store: store, isOpen: $menuOpen)
                .presentationDetents([.medium, .large])
        }
    }

    // MARK: Header

    private var header: some View {
        HStack(spacing: Theme.s3) {
            Button { openDrawer() } label: {
                Image(systemName: "square.stack")
                    .font(.system(size: 17)).foregroundStyle(Theme.accent)
                    .frame(width: 28, height: 28)
            }
            Button { menuOpen = true } label: {
                HStack(spacing: 0) {
                    VStack(alignment: .leading, spacing: 2) {
                        HStack(spacing: 4) {
                            Text(title)
                                .font(Theme.sf(15, .semibold)).foregroundStyle(Theme.text)
                                .tracking(-0.2).lineLimit(1)
                            Image(systemName: "chevron.down")
                                .font(.system(size: 10)).foregroundStyle(Theme.tertiary)
                        }
                        Text(subtitle)
                            .font(Theme.sf(12)).foregroundStyle(subtitleColor)
                            .lineLimit(1)
                    }
                    Spacer(minLength: 0)
                }
                .contentShape(Rectangle())   // whole width is tappable
            }
            .buttonStyle(.plain)
        }
        .padding(.horizontal, Theme.s4)
        .padding(.top, Theme.s2h).padding(.bottom, Theme.s2h)
        .background(Theme.bg.opacity(0.92))
        .overlay(alignment: .bottom) {
            Rectangle().fill(Color.black.opacity(0.09)).frame(height: 0.5)
        }
    }

    private var title: String {
        let t = store.sessions.first { $0.id == store.activeSessionId }?.title ?? ""
        return t.isEmpty ? "New session" : t
    }
    private var hasPendingQuestion: Bool { store.pendingQuestionId != nil }
    private var offline: Bool { store.connState != .online }
    private var subtitleColor: Color {
        if offline { return Theme.amberSubtitle }        // reconnecting/offline
        if hasPendingQuestion { return Theme.amberSubtitle }
        return Theme.secondary
    }
    private var subtitle: String {
        switch store.connState {
        case .reconnecting: return "reconnecting…"
        case .connecting: return "connecting…"
        case .offline: return "offline"
        case .online:
            if hasPendingQuestion { return "asked you a question" }
            return store.busy ? "working" : "idle"
        }
    }

    // MARK: Stream

    private var stream: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: Theme.s3h) {
                    ForEach(streamGroups) { group in
                        StreamGroupView(group: group, store: store).id(group.id)
                    }
                    if store.busy {
                        StatusLine()
                    }
                    Color.clear.frame(height: 1).id("tail")
                }
                .padding(.horizontal, Theme.s4)
                .padding(.top, Theme.s4)
                .padding(.bottom, Theme.s3)
            }
            .overlay(alignment: .bottom) {
                if !followTail {
                    Button {
                        followTail = true
                        withAnimation(.easeOut(duration: 0.2)) { proxy.scrollTo("tail", anchor: .bottom) }
                    } label: {
                        Label("Jump to latest", systemImage: "arrow.down")
                            .font(Theme.sf(12, .semibold)).foregroundStyle(.white)
                            .padding(.horizontal, Theme.s3).padding(.vertical, Theme.s2)
                            .background(Theme.text, in: .capsule)
                    }
                    .padding(.bottom, Theme.s2)
                }
            }
            .scrollDismissesKeyboard(.interactively)
            .onTapGesture { dismissKeyboard() }
            .onChange(of: store.transcript.count) { _, _ in
                guard followTail else { return }
                withAnimation(.easeOut(duration: 0.2)) { proxy.scrollTo("tail", anchor: .bottom) }
            }
        }
    }

    /// Resign first responder so the keyboard drops (tap-anywhere-to-dismiss).
    private func dismissKeyboard() {
        UIApplication.shared.sendAction(
            #selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
    }

    /// Collapse the flat transcript into render groups: a run of consecutive
    /// tool calls AND subagent groups becomes ONE flush card (dividers, no gaps);
    /// everything else is a standalone block.
    private var streamGroups: [StreamGroup] {
        var groups: [StreamGroup] = []
        var run: [GroupEntry] = []

        func flush() {
            guard !run.isEmpty else { return }
            groups.append(.card(id: run.first!.id, entries: run))
            run = []
        }

        for item in store.transcript {
            switch item.kind {
            case .tool(let cell):
                run.append(.tool(id: item.id, cell: cell))
            case .subagentGroup(let subs):
                run.append(.subagents(id: item.id, subs: subs))
            default:
                flush()
                groups.append(.single(item))
            }
        }
        flush()
        return groups
    }

    // MARK: Composer

    private var composer: some View {
        VStack(spacing: 0) {
            Rectangle().fill(Color.black.opacity(0.09)).frame(height: 0.5)
            HStack(spacing: Theme.s2h) {
                Image(systemName: "plus")
                    .font(.system(size: 15)).foregroundStyle(Theme.secondary)
                TextField(composerPlaceholder, text: $input, axis: .vertical)
                    .font(Theme.sf(14)).foregroundStyle(Theme.text).lineLimit(1...5)
                if sendEnabled {
                    Button { send() } label: {
                        Image(systemName: "arrow.up.circle.fill")
                            .font(.system(size: 22)).foregroundStyle(Theme.accent)
                    }
                } else {
                    Image(systemName: "mic.fill")
                        .font(.system(size: 14)).foregroundStyle(Theme.secondary)
                }
            }
            .padding(.horizontal, Theme.s3).padding(.vertical, 9)
            .background(Theme.surface)
            .overlay(RoundedRectangle(cornerRadius: 20).stroke(Color.black.opacity(0.1), lineWidth: 0.5))
            .clipShape(RoundedRectangle(cornerRadius: 20))
            .shadow(color: .black.opacity(0.04), radius: 1, x: 0, y: 1)
            .padding(.horizontal, Theme.s3)
            .padding(.top, Theme.s2h)
            .padding(.bottom, Theme.s2h)
        }
        .background(Theme.bg.opacity(0.95))
    }

    private var sendEnabled: Bool {
        !offline && !input.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }
    private var composerPlaceholder: String {
        if offline { return "Reconnecting…" }
        return store.pendingQuestionId != nil ? "Answer…" : "Message"
    }
    private func send() {
        let t = input.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !t.isEmpty else { return }
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
        followTail = true
        // Route free text to a pending inline question if one is waiting;
        // otherwise it's a normal prompt.
        if let qid = store.pendingQuestionId {
            store.answerQuestion(id: qid, answer: t)
        } else {
            store.sendPrompt(t)
        }
        input = ""
    }
    private func openDrawer() {
        store.refreshSessions()
        drawerOpen = true
    }
}

/// The "Working · 18s" status line at the tail of the stream while streaming.
struct StatusLine: View {
    var body: some View {
        HStack(spacing: Theme.s2) {
            Circle().fill(Theme.tertiary).frame(width: 6, height: 6)
            Text("Working").font(Theme.sf(13)).foregroundStyle(Theme.secondary.opacity(0.8))
        }
    }
}

/// One entry inside a flush card run: a tool call or a subagent group.
enum GroupEntry: Identifiable {
    case tool(id: String, cell: ToolCell)
    case subagents(id: String, subs: [Subagent])
    var id: String {
        switch self {
        case .tool(let id, _): return "tool-\(id)"
        case .subagents(let id, _): return "subs-\(id)"
        }
    }
}

/// A render group: a standalone block, or a flush card of consecutive tool
/// calls + subagent groups.
enum StreamGroup: Identifiable {
    case single(TranscriptItem)
    case card(id: String, entries: [GroupEntry])

    var id: String {
        switch self {
        case .single(let item): return item.id
        case .card(let id, _): return "card-\(id)"
        }
    }
}

/// Renders one stream group.
struct StreamGroupView: View {
    let group: StreamGroup
    @ObservedObject var store: SessionStore

    var body: some View {
        switch group {
        case .single(let item):
            block(item)
        case .card(_, let entries):
            MixedToolCard(entries: entries, store: store)
        }
    }

    @ViewBuilder private func block(_ item: TranscriptItem) -> some View {
        switch item.kind {
        case .user(let text):
            UserBubble(text: text)
        case .assistant(let md):
            MarkdownText(md)
        case .tool(let cell):
            MixedToolCard(entries: [.tool(id: item.id, cell: cell)], store: store)
        case .subagentGroup(let subs):
            MixedToolCard(entries: [.subagents(id: item.id, subs: subs)], store: store)
        case .error(let text):
            HStack(spacing: Theme.s2) {
                Image(systemName: "exclamationmark.triangle.fill")
                    .font(.system(size: 12)).foregroundStyle(Theme.removed)
                Text(text).font(Theme.sf(13)).foregroundStyle(Theme.removed)
            }
        case .note(let text):
            Text(text).font(Theme.sf(12)).foregroundStyle(Theme.tertiary)
                .frame(maxWidth: .infinity, alignment: .center)
        case .question(let q, let answer):
            QuestionCard(id: item.id, query: q, answer: answer, store: store)
        case .subagentGroup(let subs):
            MixedToolCard(entries: [.subagents(id: item.id, subs: subs)], store: store)
        }
    }
}

/// Right-aligned user message bubble (accent, asymmetric radius).
struct UserBubble: View {
    let text: String
    var body: some View {
        Text(text)
            .font(Theme.sf(15)).foregroundStyle(.white)
            .padding(.horizontal, Theme.s3h).padding(.vertical, Theme.s2h)
            .background(Theme.accent)
            .clipShape(.rect(topLeadingRadius: Theme.r18, bottomLeadingRadius: Theme.r18,
                             bottomTrailingRadius: 5, topTrailingRadius: Theme.r18))
            .frame(maxWidth: .infinity, alignment: .trailing)
            .padding(.leading, 60)   // enforce the ~78% max width feel
    }
}
