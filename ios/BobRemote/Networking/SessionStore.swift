import Foundation
import Combine
import UIKit
import SwiftUI

/// A pending question or permission the host is waiting on.
enum PendingAsk: Identifiable, Equatable {
    case query(id: String, query: UserQueryDTO)
    case permission(id: String, request: PermissionReqDTO, options: [PermissionOptDTO])

    var id: String {
        switch self {
        case .query(let id, _): return id
        case .permission(let id, _, _): return id
        }
    }
    static func == (l: PendingAsk, r: PendingAsk) -> Bool { l.id == r.id }
}

/// One block in the transcript. Tool blocks correlate a call with its later
/// result by `tool_use_id` so the UI can render one expandable cell.
struct TranscriptItem: Identifiable {
    let id: String            // UUID for text/user, tool_use_id for tools
    var kind: Kind

    enum Kind {
        case user(String)
        case assistant(String)          // raw markdown, grows via text deltas
        case tool(ToolCell)
        case error(String)
        case note(String)
        /// An inline agent question (7a). `answer` is nil until resolved, then
        /// holds the chosen/typed text so the card shows a settled state.
        case question(UserQueryDTO, answer: String?)
        /// One `task` invocation's spawned subagents (10a/10b), grouped so they
        /// live inside a single collapsible card instead of the chat stream.
        case subagentGroup([Subagent])
    }
}

/// A todo item from the `todo_write` tool (for the Tasks view).
struct TodoItem: Identifiable {
    let id = UUID()
    let content: String
    let status: Status   // pending | in_progress | completed
    enum Status: String { case pending, in_progress, completed }
}

/// One spawned subagent (10a/10b). `tools` are its individual tool calls with
/// full input/output (correlated by tool_use_id), so the transcript page can
/// show real detail — not just labels.
struct Subagent: Identifiable {
    let id: String        // "task_N"
    let task: String      // the subagent's description
    var tools: [(id: String, cell: ToolCell)]
    var running: Bool
    /// The subagent's final output text. Populated for rehydrated subagents
    /// (parsed from the stored task result) since their live tool events aren't
    /// persisted. nil for live subagents (their tools[] carry the detail).
    var finalOutput: String? = nil

    /// Compact step labels (last-2 preview in the mini-block).
    var stepLabels: [String] { tools.map { ToolLabel.of($0.cell).display } }
}

/// A tool invocation and its (eventual) result, rendered as an expandable cell.
struct ToolCell {
    let name: String
    let input: JSONValue
    var output: String?
    var isError: Bool = false
    var running: Bool { output == nil }

    /// If the output carries a ```diff fenced block, extract it for diff view.
    var diff: String? {
        guard let output else { return nil }
        return Self.extractFence(output, lang: "diff")
    }

    /// Output with any leading diff fence stripped (the header line stays).
    var plainOutput: String? { output }

    static func extractFence(_ text: String, lang: String) -> String? {
        // Find ```<lang> ... ``` and return the inner body.
        guard let start = text.range(of: "```\(lang)") else { return nil }
        let afterOpen = text[start.upperBound...]
        guard let nl = afterOpen.firstIndex(of: "\n") else { return nil }
        let body = afterOpen[afterOpen.index(after: nl)...]
        guard let close = body.range(of: "```") else { return nil }
        return String(body[..<close.lowerBound])
    }
}

/// Owns the WebSocket connection to the relay and the derived UI state.
@MainActor
final class SessionStore: ObservableObject {
    /// Tools that surface as an inline question card (AskQuery frame) rather
    /// than a tool row, so we suppress their tool_call/tool_result rendering.
    static func isAskTool(_ name: String) -> Bool {
        name == "ask_user" || name == "exit_plan"
    }

    /// Reconstruct subagents from a stored `task` tool input:
    /// `{ "tasks": [ { "description": ... }, ... ] }`. Rehydrated subagents show
    /// as completed with no steps (subagent step events aren't persisted).
    static func subagentsFromTaskInput(_ input: JSONValue) -> [Subagent] {
        guard case .object(let o) = input, case .array(let tasks)? = o["tasks"] else {
            return []
        }
        return tasks.enumerated().map { i, t in
            let desc = t.field("description") ?? "Subtask \(i + 1)"
            return Subagent(id: "task_\(i + 1)", task: desc, tools: [], running: false)
        }
    }

    /// Parse the todo list from a `todo_write` tool input:
    /// `{ "todos": [ { "content": ..., "status": ... }, ... ] }`.
    static func todosFromInput(_ input: JSONValue) -> [TodoItem] {
        guard case .object(let o) = input, case .array(let arr)? = o["todos"] else {
            return []
        }
        return arr.compactMap { t in
            guard let content = t.field("content") else { return nil }
            let status = TodoItem.Status(rawValue: t.field("status") ?? "pending") ?? .pending
            return TodoItem(content: content, status: status)
        }
    }

    @Published var connected = false
    @Published var busy = false
    /// Finer-grained link state for the UI (header + composer).
    @Published var connState: ConnectionState = .offline
    @Published var transcript: [TranscriptItem] = []
    /// The ask currently shown in a sheet. Set from a FIFO queue so a second
    /// permission prompt in the same turn isn't dropped by SwiftUI's rule that
    /// you can't present a new sheet while another is dismissing.
    @Published var pending: PendingAsk?
    @Published var lastError: String?
    /// Stored sessions for the drawer, newest first.
    @Published var sessions: [SessionMeta] = []
    /// The latest todo list (from the `todo_write` tool), for the Tasks view.
    @Published var todos: [TodoItem] = []
    /// Current interaction mode: "normal" | "auto_accept" | "plan".
    @Published var mode: String = "normal"
    /// The active conversation session id.
    @Published var activeSessionId: String = ""

    /// Link lifecycle. `connected` stays true across blips so the UI stays on
    /// the session screen; `connState` carries the live detail.
    enum ConnectionState { case connecting, online, reconnecting, offline }

    private var askQueue: [PendingAsk] = []

    // Remembered credentials so reconnect needs no user input.
    private var creds: (relay: String, session: String, token: String)?
    // Reconnect bookkeeping.
    private var reconnectAttempt = 0
    private var reconnectWork: DispatchWorkItem?
    /// Set true by an explicit user disconnect or a fatal (bad-token) error, so
    /// the reconnect loop knows to stay down.
    private var intentionallyClosed = false
    /// Whether we've already requested notification permission this launch.
    private var askedNotifyAuth = false

    private var task: URLSessionWebSocketTask?
    private let decoder = JSONDecoder()
    private let encoder = JSONEncoder()

    // MARK: Connection

    /// Initial connect from the connect screen. Remembers credentials so the
    /// reconnect loop and background/foreground resumes can reuse them.
    func connect(relay: String, session: String, token: String) {
        // Trim stray whitespace/newlines that keyboards can append — a single
        // trailing space in the token makes the relay reject with "bad token".
        let relay = relay.trimmingCharacters(in: .whitespacesAndNewlines)
        let session = session.trimmingCharacters(in: .whitespacesAndNewlines)
        let token = token.trimmingCharacters(in: .whitespacesAndNewlines)
        creds = (relay, session, token)
        intentionallyClosed = false
        reconnectAttempt = 0
        openSocket()
    }

    /// Open (or re-open) the WebSocket using the remembered credentials and
    /// kick off a resync. Shared by first connect and every reconnect.
    private func openSocket() {
        guard let creds else { return }
        reconnectWork?.cancel()
        task?.cancel(with: .goingAway, reason: nil)

        guard let url = URL(string: creds.relay) else {
            lastError = "bad relay URL"
            connState = .offline
            return
        }
        connState = connected ? .reconnecting : .connecting
        let ws = URLSession.shared.webSocketTask(with: url)
        task = ws
        ws.resume()
        send(Hello(session: creds.session, token: creds.token))
        // ListSessions doubles as the resync trigger: the host replies with
        // History + any in-flight buffered events + Status.
        send(ControlFrame.listSessions)
        connected = true
        connState = .online
        reconnectAttempt = 0
        // Ask for notification permission once, after the first live connect.
        if !askedNotifyAuth {
            askedNotifyAuth = true
            Notifier.requestAuthorization()
        }
        receiveLoop()
    }

    /// Explicit user teardown — returns to the connect screen and stops the
    /// reconnect loop.
    func disconnect() {
        intentionallyClosed = true
        reconnectWork?.cancel()
        task?.cancel(with: .goingAway, reason: nil)
        task = nil
        connected = false
        busy = false
        connState = .offline
        creds = nil
    }

    /// Called when the socket drops unexpectedly. Enters reconnecting state and
    /// schedules a backoff retry (unless the user closed it or it was fatal).
    private func scheduleReconnect() {
        guard !intentionallyClosed, creds != nil else { return }
        connState = .reconnecting
        busy = false
        task = nil
        // Capped exponential backoff with light jitter: ~1, 2, 4, 8, 10, 10s…
        let delays: [Double] = [1, 2, 4, 8, 10]
        let base = delays[min(reconnectAttempt, delays.count - 1)]
        let delay = base + Double.random(in: 0...0.5)
        reconnectAttempt += 1
        let work = DispatchWorkItem { [weak self] in self?.openSocket() }
        reconnectWork = work
        DispatchQueue.main.asyncAfter(deadline: .now() + delay, execute: work)
    }

    /// Force an immediate reconnect (e.g. returning to foreground). No-op if
    /// already online or intentionally closed.
    func reconnectNow() {
        guard !intentionallyClosed, creds != nil, connState != .online else { return }
        reconnectAttempt = 0
        openSocket()
    }

    // MARK: Background lifecycle

    /// Whether the app is foreground-active (gates notifications: we only notify
    /// when the user isn't already looking).
    private(set) var isForeground = true
    private var bgTask: UIBackgroundTaskIdentifier = .invalid

    /// Driven by RootView's scenePhase. Keeps the socket alive briefly in the
    /// background (iOS grants ~30s), and resyncs immediately on foreground.
    func scenePhaseChanged(_ phase: ScenePhase) {
        switch phase {
        case .active:
            isForeground = true
            endBackgroundTask()
            // If the link dropped or suspended while away, reconnect now.
            reconnectNow()
        case .background:
            isForeground = false
            beginBackgroundTask()
        case .inactive:
            isForeground = false
        @unknown default:
            break
        }
    }

    private func beginBackgroundTask() {
        endBackgroundTask()
        bgTask = UIApplication.shared.beginBackgroundTask(withName: "bob-relay-socket") { [weak self] in
            // Expiration handler — iOS is about to suspend us; release cleanly.
            self?.endBackgroundTask()
        }
    }

    private func endBackgroundTask() {
        if bgTask != .invalid {
            UIApplication.shared.endBackgroundTask(bgTask)
            bgTask = .invalid
        }
    }

    // MARK: Sending

    private func send<T: Encodable>(_ value: T) {
        guard let task, let data = try? encoder.encode(value),
              let json = String(data: data, encoding: .utf8) else { return }
        task.send(.string(json)) { [weak self] err in
            if let err { Task { @MainActor in self?.lastError = err.localizedDescription } }
        }
    }

    func sendPrompt(_ text: String) {
        transcript.append(.init(id: UUID().uuidString, kind: .user(text)))
        send(ControlFrame.prompt(text: text))
    }

    func cancel() { send(ControlFrame.cancel) }

    func answer(_ ask: PendingAsk, choice: Int?) {
        switch ask {
        case .query(let id, let q):
            let answer = choice.flatMap { q.options.indices.contains($0) ? q.options[$0] : nil }
            send(ControlFrame.answerQuery(id: id, answer: answer))
        case .permission(let id, _, _):
            send(ControlFrame.answerPermission(id: id, choice: choice))
        }
        // Present the next queued ask, if any, on the next run-loop tick so the
        // current sheet finishes dismissing first.
        pending = nil
        DispatchQueue.main.async { [weak self] in self?.presentNextAsk() }
    }

    /// Enqueue an ask; present immediately if nothing is showing.
    private func enqueueAsk(_ ask: PendingAsk) {
        askQueue.append(ask)
        if pending == nil { presentNextAsk() }
    }

    private func presentNextAsk() {
        guard pending == nil, !askQueue.isEmpty else { return }
        pending = askQueue.removeFirst()
    }

    func setMode(_ mode: String) {
        self.mode = mode
        send(ControlFrame.setMode(mode: mode))
    }

    /// Answer an inline agent question (7a). `answer` is the chosen option label
    /// or free text; nil dismisses. Settles the card in place.
    func answerQuestion(id: String, answer: String?) {
        send(ControlFrame.answerQuery(id: id, answer: answer))
        if let idx = transcript.firstIndex(where: { $0.id == id }),
           case .question(let q, _) = transcript[idx].kind {
            transcript[idx].kind = .question(q, answer: answer ?? "")
        }
    }

    /// The id of the most recent unanswered inline question, if any — lets the
    /// composer route a typed reply to the pending question.
    var pendingQuestionId: String? {
        for item in transcript.reversed() {
            if case .question(_, let answer) = item.kind {
                return answer == nil ? item.id : nil
            }
        }
        return nil
    }

    // MARK: Sessions

    func refreshSessions() { send(ControlFrame.listSessions) }
    func loadSession(_ id: String) { send(ControlFrame.loadSession(id: id)) }
    func newSession() { send(ControlFrame.newSession) }

    // MARK: Derived state (for the session menu / debug)

    /// Count of subagents currently running across the transcript.
    var runningSubagentCount: Int {
        transcript.reduce(0) { acc, item in
            if case .subagentGroup(let subs) = item.kind {
                return acc + subs.filter { $0.running }.count
            }
            return acc
        }
    }

    /// Total subagents spawned this session.
    var totalSubagentCount: Int {
        transcript.reduce(0) { acc, item in
            if case .subagentGroup(let subs) = item.kind { return acc + subs.count }
            return acc
        }
    }

    /// Every subagent spawned this session, flattened across all task groups.
    var allSubagents: [Subagent] {
        transcript.flatMap { item -> [Subagent] in
            if case .subagentGroup(let subs) = item.kind { return subs }
            return []
        }
    }

    /// Count of completed todos.
    var todosDone: Int { todos.filter { $0.status == .completed }.count }

    /// A plain-text rendering of the transcript, for Export.
    func exportTranscript() -> String {
        var out: [String] = []
        for item in transcript {
            switch item.kind {
            case .user(let t): out.append("You: \(t)")
            case .assistant(let t): out.append("Bob: \(t)")
            case .tool(let c):
                out.append("[tool] \(c.name) \(c.input.oneLineSummary)")
                if let o = c.output { out.append(o) }
            case .error(let t): out.append("[error] \(t)")
            case .note(let t): out.append("— \(t) —")
            case .question(let q, let a):
                out.append("[question] \(q.title)" + (a.map { " → \($0)" } ?? ""))
            case .subagentGroup(let subs):
                out.append("[subagents] \(subs.count) dispatched")
                for s in subs { out.append("  • \(s.task) (\(s.running ? "running" : "done"))") }
            }
        }
        return out.joined(separator: "\n\n")
    }

    // MARK: Receiving

    private func receiveLoop() {
        task?.receive { [weak self] result in
            guard let self else { return }
            Task { @MainActor in
                switch result {
                case .failure(let err):
                    // Socket dropped — enter reconnecting and back off. Don't
                    // bounce to the connect screen; `connected` stays true.
                    self.lastError = err.localizedDescription
                    self.scheduleReconnect()
                case .success(.string(let text)):
                    self.handle(text)
                    self.receiveLoop()
                case .success:
                    self.receiveLoop()
                }
            }
        }
    }

    private func handle(_ text: String) {
        let data = Data(text.utf8)
        // The relay sends a bare {"error":"…"} frame (e.g. bad token) right
        // before closing. This is fatal (bad credentials) — stop retrying and
        // return to the connect screen.
        if let relayErr = try? decoder.decode(RelayError.self, from: data) {
            lastError = "relay: \(relayErr.error)"
            disconnect()
            return
        }
        guard let frame = try? decoder.decode(HostFrame.self, from: data) else {
            lastError = "decode failed: \(text.prefix(120))"
            return
        }
        switch frame {
        case .event(let e): apply(e)
        case .askQuery(let id, let q):
            // Agent questions render inline in the stream (7a), keyed by the
            // ask id so we can settle the card when answered.
            transcript.append(.init(id: id, kind: .question(q, answer: nil)))
            notifyIfBackground(id: "needs-you-\(id)", title: "Bob needs you", body: q.title)
        case .askPermission(let id, let req, let opts):
            enqueueAsk(.permission(id: id, request: req, options: opts))
            notifyIfBackground(id: "needs-you-\(id)", title: "Bob needs you",
                               body: "Allow \(req.tool)?")
        case .history(let messages, let sessionId, let subagentRuns):
            activeSessionId = sessionId
            rehydrate(from: messages, subagentRuns: subagentRuns)
        case .sessionList(let list):
            sessions = list
        case .status(let b):
            // A busy → idle transition means a turn just finished. Notify if the
            // user isn't watching. (Ignore the resync's initial busy=false so we
            // don't fire on reconnect.)
            if busy && !b {
                notifyIfBackground(id: "turn-done", title: "Bob finished",
                                   body: lastAssistantSnippet())
            }
            busy = b
        }
    }

    /// Fire a local notification only when the app isn't foreground-active.
    private func notifyIfBackground(id: String, title: String, body: String) {
        guard !isForeground else { return }
        Notifier.notify(id: id, title: title, body: body)
    }

    /// A short tail of the latest assistant text for the "finished" notification.
    private func lastAssistantSnippet() -> String {
        for item in transcript.reversed() {
            if case .assistant(let md) = item.kind {
                let flat = md.replacingOccurrences(of: "\n", with: " ")
                    .trimmingCharacters(in: .whitespaces)
                if flat.isEmpty { continue }
                return String(flat.prefix(120))
            }
        }
        return "Your turn is complete."
    }

    /// Rebuild the whole transcript from a stored message list, correlating
    /// tool_use blocks with their later tool_result by id.
    private func rehydrate(from messages: [Message], subagentRuns: [SubagentRunDTO] = []) {
        // Reset per-session derived state — otherwise a session with no todos
        // keeps showing the previous session's list.
        todos = []
        // Index persisted subagent runs by their task tool_use_id, so a stored
        // `task` call rehydrates with each subagent's real tool calls.
        let runsByTask = Dictionary(uniqueKeysWithValues: subagentRuns.map { ($0.task_use_id, $0) })
        var items: [TranscriptItem] = []
        var toolIndex: [String: Int] = [:]   // tool_use_id -> index in items
        var taskIndex: [String: Int] = [:]   // task tool_use_id -> subgroup index
        for m in messages {
            for block in m.content {
                switch block {
                case .text(let t):
                    let trimmed = t.trimmingCharacters(in: .whitespacesAndNewlines)
                    if trimmed.isEmpty { break }
                    if m.role == "user" {
                        items.append(.init(id: UUID().uuidString, kind: .user(t)))
                    } else {
                        items.append(.init(id: UUID().uuidString, kind: .assistant(t)))
                    }
                case .toolUse(let id, let name, let input):
                    // Skip ask_user / exit_plan — they aren't shown as tool rows.
                    if Self.isAskTool(name) { break }
                    // A stored `task` invocation → a subagent group. Prefer the
                    // persisted run (with real tool calls); fall back to the
                    // input descriptions if no run was saved.
                    if name == "task" {
                        let subs: [Subagent]
                        if let run = runsByTask[id] {
                            subs = run.subagents.map { ps in
                                Subagent(
                                    id: ps.id, task: ps.task,
                                    tools: ps.tools.map { pt in
                                        (id: pt.id,
                                         cell: ToolCell(name: pt.name, input: pt.input,
                                                        output: pt.output, isError: pt.is_error))
                                    },
                                    running: false)
                            }
                        } else {
                            subs = Self.subagentsFromTaskInput(input)
                        }
                        if !subs.isEmpty {
                            items.append(.init(id: "subgroup-\(id)", kind: .subagentGroup(subs)))
                            taskIndex[id] = items.count - 1
                        }
                        break
                    }
                    // todo_write updates the Tasks view, not the chat stream.
                    if name == "todo_write" {
                        todos = Self.todosFromInput(input)
                        break
                    }
                    items.append(.init(id: id, kind: .tool(ToolCell(name: name, input: input))))
                    toolIndex[id] = items.count - 1
                case .toolResult(let toolUseId, let content, let isError):
                    // A task result carries each subagent's final output as
                    // "### <description>\n<output>" sections — attach them so the
                    // rehydrated transcript page shows real content.
                    if let gi = taskIndex[toolUseId],
                       case .subagentGroup(var subs) = items[gi].kind {
                        let outputs = Self.parseTaskResult(content, descriptions: subs.map { $0.task })
                        for i in subs.indices {
                            subs[i].finalOutput = outputs[subs[i].task]
                        }
                        items[gi].kind = .subagentGroup(subs)
                        break
                    }
                    if let idx = toolIndex[toolUseId],
                       case .tool(var cell) = items[idx].kind {
                        cell.output = content
                        cell.isError = isError
                        items[idx].kind = .tool(cell)
                    }
                }
            }
        }
        transcript = items
    }

    /// Parse a `task` tool result into `description → output`. The task tool
    /// formats each subagent's result as "### <description>\n<body>", but the
    /// body itself can contain arbitrary `###` markdown headers — so we ONLY
    /// treat a `### ` line as a section boundary when it exactly matches one of
    /// the known subagent descriptions.
    static func parseTaskResult(_ text: String, descriptions: [String]) -> [String: String] {
        let known = Set(descriptions)
        var out: [String: String] = [:]
        var currentKey: String? = nil
        var buf: [String] = []
        func flush() {
            if let k = currentKey {
                out[k] = buf.joined(separator: "\n").trimmingCharacters(in: .whitespacesAndNewlines)
            }
            buf = []
        }
        for line in text.components(separatedBy: "\n") {
            if line.hasPrefix("### ") {
                let header = String(line.dropFirst(4)).trimmingCharacters(in: .whitespaces)
                if known.contains(header) {
                    flush()
                    currentKey = header
                    continue
                }
            }
            buf.append(line)   // ordinary content (incl. non-matching ### headers)
        }
        flush()
        return out
    }

    private func apply(_ e: AgentEventDTO) {
        switch e {
        case .textDelta(let t):
            // Only root text streams into the chat (subagent text isn't sent).
            if let last = transcript.last, case .assistant(let s) = last.kind {
                transcript[transcript.count - 1].kind = .assistant(s + t)
            } else {
                transcript.append(.init(id: UUID().uuidString, kind: .assistant(t)))
            }

        case .subagentSpawn(_, let agentId, let task):
            // Group all subagents of one `task` invocation into a single block.
            let sub = Subagent(id: agentId, task: task, tools: [], running: true)
            if let last = transcript.last, case .subagentGroup(var subs) = last.kind {
                subs.append(sub)
                transcript[transcript.count - 1].kind = .subagentGroup(subs)
            } else {
                transcript.append(.init(id: "subgroup-\(agentId)",
                                        kind: .subagentGroup([sub])))
            }

        case .toolCall(let name, let id, let input, let agentId):
            if agentId != "root" {
                // A tool call inside a subagent — record it on that subagent,
                // never in the root chat.
                addSubagentTool(agentId: agentId, id: id,
                                cell: ToolCell(name: name, input: input))
                break
            }
            // ask_user / exit_plan surface as the inline question card; the `task`
            // tool surfaces as the subagentGroup block — neither is a tool row.
            if Self.isAskTool(name) || name == "task" { break }
            // todo_write updates the Tasks view; don't show it as a chat row.
            if name == "todo_write" {
                todos = Self.todosFromInput(input)
                break
            }
            transcript.append(.init(id: id,
                kind: .tool(ToolCell(name: name, input: input))))

        case .toolResult(let id, let output, let isError, let agentId):
            if agentId != "root" {
                attachSubagentResult(agentId: agentId, toolId: id,
                                     output: output, isError: isError)
                break
            }
            if let idx = transcript.firstIndex(where: { $0.id == id }),
               case .tool(var cell) = transcript[idx].kind {
                cell.output = output
                cell.isError = isError
                transcript[idx].kind = .tool(cell)
            } else if isError {
                transcript.append(.init(id: UUID().uuidString, kind: .error(output)))
            }

        case .turnEnd(let agentId):
            if agentId != "root" { markSubagentDone(agentId: agentId) }

        case .error(let m):
            transcript.append(.init(id: UUID().uuidString, kind: .error(m)))

        case .turnStart, .other:
            break
        }
    }

    /// Add a tool call to the matching subagent (searching trailing groups).
    private func addSubagentTool(agentId: String, id: String, cell: ToolCell) {
        for i in stride(from: transcript.count - 1, through: 0, by: -1) {
            if case .subagentGroup(var subs) = transcript[i].kind,
               let j = subs.firstIndex(where: { $0.id == agentId }) {
                subs[j].tools.append((id: id, cell: cell))
                transcript[i].kind = .subagentGroup(subs)
                return
            }
        }
    }

    /// Attach a tool result to the matching subagent's tool by tool_use_id.
    private func attachSubagentResult(agentId: String, toolId: String,
                                      output: String, isError: Bool) {
        for i in stride(from: transcript.count - 1, through: 0, by: -1) {
            if case .subagentGroup(var subs) = transcript[i].kind,
               let j = subs.firstIndex(where: { $0.id == agentId }),
               let k = subs[j].tools.firstIndex(where: { $0.id == toolId }) {
                var cell = subs[j].tools[k].cell
                cell.output = output
                cell.isError = isError
                subs[j].tools[k].cell = cell
                transcript[i].kind = .subagentGroup(subs)
                return
            }
        }
    }

    private func markSubagentDone(agentId: String) {
        for i in stride(from: transcript.count - 1, through: 0, by: -1) {
            if case .subagentGroup(var subs) = transcript[i].kind,
               let j = subs.firstIndex(where: { $0.id == agentId }) {
                subs[j].running = false
                transcript[i].kind = .subagentGroup(subs)
                return
            }
        }
    }
}
