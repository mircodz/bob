import Foundation

// MARK: - Wire protocol (mirrors bob-protocol's serde tags)

/// The relay's out-of-band error frame, sent just before it closes the socket
/// (e.g. a bad token or malformed hello). Not part of HostFrame.
struct RelayError: Decodable {
    let error: String
}

/// First frame the controller sends. Matches Rust `Hello` with
/// `#[serde(tag = "role", rename_all = "snake_case")]`.
struct Hello: Encodable {
    let role = "controller"
    let session: String
    let token: String
}

/// Controller -> Host. Matches Rust `ControlFrame`
/// `#[serde(tag = "type", rename_all = "snake_case")]`.
enum ControlFrame: Encodable {
    case prompt(text: String)
    case cancel
    case answerQuery(id: String, answer: String?)
    case answerPermission(id: String, choice: Int?)
    case setMode(mode: String)
    case listSessions
    case loadSession(id: String)
    case newSession

    private enum CodingKeys: String, CodingKey {
        case type, text, id, answer, choice, mode
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .prompt(let text):
            try c.encode("prompt", forKey: .type)
            try c.encode(text, forKey: .text)
        case .cancel:
            try c.encode("cancel", forKey: .type)
        case .answerQuery(let id, let answer):
            try c.encode("answer_query", forKey: .type)
            try c.encode(id, forKey: .id)
            try c.encode(answer, forKey: .answer)
        case .answerPermission(let id, let choice):
            try c.encode("answer_permission", forKey: .type)
            try c.encode(id, forKey: .id)
            try c.encode(choice, forKey: .choice)
        case .setMode(let mode):
            try c.encode("set_mode", forKey: .type)
            try c.encode(mode, forKey: .mode)
        case .listSessions:
            try c.encode("list_sessions", forKey: .type)
        case .loadSession(let id):
            try c.encode("load_session", forKey: .type)
            try c.encode(id, forKey: .id)
        case .newSession:
            try c.encode("new_session", forKey: .type)
        }
    }
}

// MARK: - Host -> Controller

struct UserQueryDTO: Decodable {
    let title: String
    let detail: String
    let options: [String]
    let allow_other: Bool
}

struct PermissionReqDTO: Decodable {
    let tool: String
    let input: JSONValue
    let cwd: String
    let preview: String?
}

struct PermissionOptDTO: Decodable {
    let label: String
    let allow: Bool
}

/// Drawer metadata for a stored conversation session.
struct SessionMeta: Decodable, Identifiable {
    let id: String
    let title: String
    let updated_at: String
    let message_count: Int
}

// MARK: - Persisted subagent runs (mirrors bob-core session types)

struct SubagentRunDTO: Decodable {
    let task_use_id: String
    let subagents: [PersistedSubagentDTO]
}
struct PersistedSubagentDTO: Decodable {
    let id: String
    let task: String
    let tools: [PersistedToolDTO]
}
struct PersistedToolDTO: Decodable {
    let id: String
    let name: String
    let input: JSONValue
    let output: String
    let is_error: Bool
}

// MARK: - Persisted messages (for rehydration)

/// A content block inside a stored Message (mirrors bob-core ContentBlock,
/// serde tag "type").
enum ContentBlock: Decodable {
    case text(String)
    case toolUse(id: String, name: String, input: JSONValue)
    case toolResult(toolUseId: String, content: String, isError: Bool)

    private enum CodingKeys: String, CodingKey {
        case type, text, id, name, input, tool_use_id, content, is_error
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .type) {
        case "text":
            self = .text((try? c.decode(String.self, forKey: .text)) ?? "")
        case "tool_use":
            self = .toolUse(
                id: (try? c.decode(String.self, forKey: .id)) ?? "",
                name: (try? c.decode(String.self, forKey: .name)) ?? "",
                input: (try? c.decode(JSONValue.self, forKey: .input)) ?? .null)
        case "tool_result":
            self = .toolResult(
                toolUseId: (try? c.decode(String.self, forKey: .tool_use_id)) ?? "",
                content: (try? c.decode(String.self, forKey: .content)) ?? "",
                isError: (try? c.decode(Bool.self, forKey: .is_error)) ?? false)
        case let other:
            throw DecodingError.dataCorruptedError(
                forKey: .type, in: c, debugDescription: "unknown content block \(other)")
        }
    }
}

/// A stored message (mirrors bob-core Message).
struct Message: Decodable {
    let role: String    // system | user | assistant | tool
    let content: [ContentBlock]
}

/// The streamed agent event. We only decode the fields the UI renders; the
/// `event` tag drives the switch. Unknown events decode as `.other`.
enum AgentEventDTO: Decodable {
    case turnStart(agentId: String)
    case textDelta(String)
    case toolCall(name: String, id: String, input: JSONValue, agentId: String)
    case toolResult(id: String, output: String, isError: Bool, agentId: String)
    case turnEnd(agentId: String)
    case subagentSpawn(parentId: String, agentId: String, task: String)
    case error(String)
    case other(String)

    private enum CodingKeys: String, CodingKey {
        case event, text, name, tool_use_id, input, output, is_error, message
        case agent_id, parent_id, task
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let tag = try c.decode(String.self, forKey: .event)
        let agent = (try? c.decode(String.self, forKey: .agent_id)) ?? "root"
        switch tag {
        case "turn_start": self = .turnStart(agentId: agent)
        case "text_delta": self = .textDelta(try c.decode(String.self, forKey: .text))
        case "tool_call":
            self = .toolCall(
                name: try c.decode(String.self, forKey: .name),
                id: try c.decode(String.self, forKey: .tool_use_id),
                input: (try? c.decode(JSONValue.self, forKey: .input)) ?? .null,
                agentId: agent)
        case "tool_result":
            self = .toolResult(
                id: (try? c.decode(String.self, forKey: .tool_use_id)) ?? "",
                output: (try? c.decode(String.self, forKey: .output)) ?? "",
                isError: (try? c.decode(Bool.self, forKey: .is_error)) ?? false,
                agentId: agent)
        case "turn_end": self = .turnEnd(agentId: agent)
        case "subagent_spawn":
            self = .subagentSpawn(
                parentId: (try? c.decode(String.self, forKey: .parent_id)) ?? "root",
                agentId: agent,
                task: (try? c.decode(String.self, forKey: .task)) ?? "")
        case "error": self = .error((try? c.decode(String.self, forKey: .message)) ?? "")
        default: self = .other(tag)
        }
    }
}

enum HostFrame: Decodable {
    case event(AgentEventDTO)
    case askQuery(id: String, query: UserQueryDTO)
    case askPermission(id: String, request: PermissionReqDTO, options: [PermissionOptDTO])
    case history(messages: [Message], sessionId: String, subagentRuns: [SubagentRunDTO])
    case sessionList(sessions: [SessionMeta])
    case status(busy: Bool)

    private enum CodingKeys: String, CodingKey {
        case type, id, query, request, options, messages, session_id, sessions, busy, subagent_runs
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .type) {
        case "event":
            // The event payload is inline alongside "type":"event".
            self = .event(try AgentEventDTO(from: decoder))
        case "ask_query":
            self = .askQuery(
                id: try c.decode(String.self, forKey: .id),
                query: try c.decode(UserQueryDTO.self, forKey: .query))
        case "ask_permission":
            self = .askPermission(
                id: try c.decode(String.self, forKey: .id),
                request: try c.decode(PermissionReqDTO.self, forKey: .request),
                options: try c.decode([PermissionOptDTO].self, forKey: .options))
        case "history":
            self = .history(
                messages: (try? c.decode([Message].self, forKey: .messages)) ?? [],
                sessionId: (try? c.decode(String.self, forKey: .session_id)) ?? "",
                subagentRuns: (try? c.decode([SubagentRunDTO].self, forKey: .subagent_runs)) ?? [])
        case "session_list":
            self = .sessionList(
                sessions: (try? c.decode([SessionMeta].self, forKey: .sessions)) ?? [])
        case "status":
            self = .status(busy: try c.decode(Bool.self, forKey: .busy))
        case let other:
            throw DecodingError.dataCorruptedError(
                forKey: .type, in: c, debugDescription: "unknown host frame \(other)")
        }
    }
}

/// Minimal JSON value for opaque fields (tool input, history messages).
enum JSONValue: Decodable {
    case string(String), number(Double), bool(Bool), object([String: JSONValue])
    case array([JSONValue]), null

    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() { self = .null }
        else if let b = try? c.decode(Bool.self) { self = .bool(b) }
        else if let n = try? c.decode(Double.self) { self = .number(n) }
        else if let s = try? c.decode(String.self) { self = .string(s) }
        else if let a = try? c.decode([JSONValue].self) { self = .array(a) }
        else if let o = try? c.decode([String: JSONValue].self) { self = .object(o) }
        else { self = .null }
    }

    /// The string value of a top-level object key, if present.
    func field(_ key: String) -> String? {
        if case .object(let o) = self, let v = o[key] {
            let s = v.oneLineSummary
            return s.isEmpty ? nil : s
        }
        return nil
    }

    /// Compact single-line summary for a tool-call header (e.g. the path/pattern).
    var oneLineSummary: String {
        switch self {
        case .string(let s): return s
        case .number(let n): return n == n.rounded() ? String(Int(n)) : String(n)
        case .bool(let b): return String(b)
        case .null: return ""
        case .array(let a): return "[\(a.count)]"
        case .object(let o):
            for k in ["path", "pattern", "url", "command", "query", "file_path"] {
                if let v = o[k]?.oneLineSummary, !v.isEmpty { return v }
            }
            return o.keys.sorted().joined(separator: ", ")
        }
    }

    /// Pretty multi-line rendering for the expanded tool detail.
    func pretty(indent: Int = 0) -> String {
        let pad = String(repeating: "  ", count: indent)
        switch self {
        case .string(let s): return "\"\(s)\""
        case .number(let n): return n == n.rounded() ? String(Int(n)) : String(n)
        case .bool(let b): return String(b)
        case .null: return "null"
        case .array(let a):
            return "[\n" + a.map { pad + "  " + $0.pretty(indent: indent + 1) }
                .joined(separator: ",\n") + "\n\(pad)]"
        case .object(let o):
            return "{\n" + o.sorted { $0.key < $1.key }.map {
                "\(pad)  \($0.key): \($0.value.pretty(indent: indent + 1))"
            }.joined(separator: ",\n") + "\n\(pad)}"
        }
    }
}
