import SwiftUI
import UIKit

// MARK: - Inline agent question (7a)

/// An inline question from the agent, tinted amber so it's findable on
/// scrollback. Suggested answers are tappable chips; free text is always
/// available via the composer. Once answered, the card settles to show the
/// chosen answer instead of the chips.
struct QuestionCard: View {
    let id: String
    let query: UserQueryDTO
    let answer: String?          // nil = awaiting; non-nil = settled
    @ObservedObject var store: SessionStore

    private var settled: Bool { answer != nil }

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.s3) {
            header
            Text(query.title)
                .font(Theme.sf(16, .medium)).foregroundStyle(Theme.text)
                .fixedSize(horizontal: false, vertical: true)

            if settled {
                answeredRow
            } else {
                chips
                footer
            }
        }
        .padding(.horizontal, Theme.s3h).padding(.vertical, Theme.s3h)
        .background(Theme.surface)
        .clipShape(RoundedRectangle(cornerRadius: Theme.r18 - 2))
        .overlay(
            RoundedRectangle(cornerRadius: Theme.r18 - 2)
                .stroke(Theme.amber.opacity(settled ? 0.25 : 0.5), lineWidth: 1))
        .shadow(color: .black.opacity(0.07), radius: 5, x: 0, y: 2)
    }

    private var header: some View {
        HStack(spacing: 9) {
            Text("?")
                .font(Theme.sf(12, .bold)).foregroundStyle(Theme.amberText)
                .frame(width: 22, height: 22)
                .background(Theme.amberBadgeBg.opacity(0.25), in: RoundedRectangle(cornerRadius: 7))
            Text(settled ? "ANSWERED" : "A QUESTION FOR YOU")
                .font(Theme.sf(12, .semibold)).tracking(0.4)
                .foregroundStyle(Theme.amberText)
            Spacer()
        }
    }

    private var chips: some View {
        VStack(spacing: Theme.s2) {
            ForEach(Array(query.options.enumerated()), id: \.offset) { i, opt in
                Button {
                    UIImpactFeedbackGenerator(style: .medium).impactOccurred()
                    store.answerQuestion(id: id, answer: opt)
                } label: {
                    Text(opt)
                        .font(Theme.sf(13.5, i == 0 ? .semibold : .medium))
                        .foregroundStyle(i == 0 ? .white : Theme.text)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, Theme.s3h).padding(.vertical, Theme.s3)
                        .background(i == 0 ? Theme.text : Color.black.opacity(0.05))
                        .clipShape(RoundedRectangle(cornerRadius: 11))
                }
                .buttonStyle(.plain)
            }
        }
    }

    private var footer: some View {
        HStack(alignment: .top, spacing: 9) {
            if !query.detail.isEmpty {
                Text(query.detail)
                    .font(Theme.sf(12)).foregroundStyle(Theme.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
            // Free-text answering is always available (routes through the
            // composer), so the hint is always shown.
            Text("Type instead")
                .font(Theme.sf(12, .semibold)).foregroundStyle(Theme.accent)
                .fixedSize()
        }
        .padding(.top, 2)
    }

    private var answeredRow: some View {
        HStack(spacing: Theme.s2) {
            Image(systemName: "checkmark.circle.fill")
                .font(.system(size: 14)).foregroundStyle(Theme.added)
            Text(answer ?? "")
                .font(Theme.sf(13.5, .medium)).foregroundStyle(Theme.text)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.horizontal, Theme.s3h).padding(.vertical, Theme.s3)
        .background(Color.black.opacity(0.04))
        .clipShape(RoundedRectangle(cornerRadius: 11))
    }
}

// MARK: - Tool family classification

/// Tool families drive the badge tint + letter (from the 1b handoff).
enum ToolFamily {
    case read      // read / search — blue
    case shell     // shell / test — red
    case subagent  // spawned subtasks — purple
    case edit      // file edits — blue-ish (grouped with read here)

    static func of(_ name: String) -> ToolFamily {
        switch name {
        case "bash": return .shell
        case "task", "subagent": return .subagent
        case "write_file", "edit_file", "multi_edit": return .edit
        default: return .read   // read_file, list_dir, glob, grep, web_fetch…
        }
    }

    var badgeBg: Color {
        switch self {
        case .shell: return Theme.shellBadgeBg
        case .subagent: return Theme.subBadgeBg
        default: return Theme.toolBadgeBg
        }
    }
    var badgeFg: Color {
        switch self {
        case .shell: return Theme.shellBadgeFg
        case .subagent: return Theme.subBadgeFg
        default: return Theme.toolBadgeFg
        }
    }
    var letter: String {
        switch self {
        case .read: return "R"
        case .shell: return "T"
        case .subagent: return "S"
        case .edit: return "E"
        }
    }
}

/// Plain-language label for a tool call ("Searched for…", "Read scheduler.ts").
/// Returns the prose plus an optional monospace fragment (a file/path) to inline.
struct ToolLabel {
    let prose: String
    let mono: String?

    /// Flattened one-line label (prose + mono fragment) for compact contexts
    /// like subagent step lists.
    var display: String {
        if let m = mono { return "\(prose) \(m)" }
        return prose
    }

    static func of(_ cell: ToolCell) -> ToolLabel {
        let arg = cell.input.oneLineSummary
        switch cell.name {
        case "read_file":  return .init(prose: "Read", mono: lastPath(arg))
        case "write_file": return .init(prose: "Wrote", mono: lastPath(arg))
        case "edit_file", "multi_edit": return .init(prose: "Edited", mono: lastPath(arg))
        case "list_dir":   return .init(prose: "Listed", mono: lastPath(arg))
        case "glob":       return .init(prose: "Searched files", mono: arg.isEmpty ? nil : arg)
        case "grep":       return .init(prose: "Searched for", mono: arg.isEmpty ? nil : arg)
        case "web_fetch":  return .init(prose: "Fetched", mono: arg.isEmpty ? nil : arg)
        case "bash":       return .init(prose: "Ran", mono: arg.isEmpty ? nil : arg)
        case "task", "subagent": return .init(prose: "Delegated a subtask", mono: nil)
        default:           return .init(prose: cell.name, mono: arg.isEmpty ? nil : arg)
        }
    }

    private static func lastPath(_ s: String) -> String? {
        guard !s.isEmpty else { return nil }
        return s.split(separator: "/").last.map(String.init) ?? s
    }
}

// MARK: - Subagent group (10a / 10b)

/// One flat subagent row (16b): purple ring/check + "Subagent — task" + chevron,
/// on a subtle purple tint.
struct SubagentRow: View {
    let sub: Subagent

    var body: some View {
        HStack(spacing: Theme.s2h) {
            if sub.running {
                SpinnerRing(color: Theme.subBadgeFg, size: 9)
            } else {
                Image(systemName: "checkmark")
                    .font(.system(size: 9, weight: .bold)).foregroundStyle(Theme.subBadgeFg)
                    .frame(width: 9, height: 9)
            }
            Text("Subagent — \(sub.task.isEmpty ? sub.id : sub.task)")
                .font(Theme.sf(14)).foregroundStyle(Theme.text)
                .lineLimit(1).truncationMode(.tail)
            Spacer(minLength: Theme.s2)
            HStack(spacing: 5) {
                if !sub.tools.isEmpty {
                    Text("\(sub.tools.count)").font(Theme.sf(13)).foregroundStyle(Theme.tertiary)
                }
                Image(systemName: "chevron.right")
                    .font(.system(size: 11, weight: .semibold)).foregroundStyle(Theme.tertiary)
            }
        }
        .padding(.horizontal, Theme.s3h).padding(.vertical, 11)
        .background(Theme.subBadgeFg.opacity(0.05))
    }
}

/// The "Open transcript" destination: the subagent's full tool calls, each
/// expandable to its input/output (now forwarded by the host for any agent).
/// Observes the store by subagent id so a *running* subagent's tool calls appear
/// live, rather than showing a frozen snapshot from tap time.
struct SubagentTranscriptView: View {
    @ObservedObject var store: SessionStore
    let subId: String
    let fallback: Subagent   // used if the id can't be found (rehydrated views)

    private var sub: Subagent {
        store.allSubagents.first { $0.id == subId } ?? fallback
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: Theme.s3) {
                Text(sub.task).font(Theme.sf(17, .semibold)).foregroundStyle(Theme.text)
                Text(sub.running ? "Running…" : "Completed")
                    .font(Theme.sf(12)).foregroundStyle(Theme.secondary)
                if !sub.tools.isEmpty {
                    // Live subagent: real tool calls with input/output.
                    ToolGroupCard(cells: sub.tools)
                } else if let output = sub.finalOutput, !output.isEmpty {
                    // Rehydrated subagent: its individual tool steps weren't
                    // persisted, but the final output was — show that.
                    Text("OUTPUT").font(Theme.sf(10.5, .semibold)).tracking(0.6)
                        .foregroundStyle(Theme.tertiary)
                    Text(output)
                        .font(Theme.mono(12)).foregroundStyle(Theme.text)
                        .textSelection(.enabled)
                        .padding(Theme.s3)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(Theme.canvas)
                        .clipShape(RoundedRectangle(cornerRadius: Theme.r10))
                } else {
                    VStack(alignment: .leading, spacing: Theme.s2) {
                        Text(sub.running ? "This subagent hasn't run any tools yet."
                                         : "This subagent ran no tools.")
                            .font(Theme.sf(13)).foregroundStyle(Theme.secondary)
                        Text("Subagents from earlier turns keep only their final output, not each tool step.")
                            .font(Theme.sf(11)).foregroundStyle(Theme.tertiary)
                    }
                    .padding(.top, Theme.s2)
                }
            }
            .padding(Theme.s4)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .background(Theme.bg)
        .navigationTitle("Subagent")
        .navigationBarTitleDisplayMode(.inline)
    }
}

/// All subagents this session, listed as flat 16b rows — the "view all tasks"
/// destination from the session menu. Tap any to open its transcript.
struct SubagentsListView: View {
    @ObservedObject var store: SessionStore

    var body: some View {
        ScrollView {
            if store.allSubagents.isEmpty {
                Text("No subagents this session.")
                    .font(Theme.sf(13)).foregroundStyle(Theme.secondary)
                    .padding(Theme.s4)
            } else {
                VStack(spacing: 0) {
                    ForEach(Array(store.allSubagents.enumerated()), id: \.offset) { i, sub in
                        if i > 0 { Rectangle().fill(Theme.hairline).frame(height: 0.5) }
                        NavigationLink {
                            SubagentTranscriptView(store: store, subId: sub.id, fallback: sub)
                        } label: {
                            SubagentRow(sub: sub)
                        }
                        .buttonStyle(.plain)
                    }
                }
                .background(Theme.surface)
                .clipShape(RoundedRectangle(cornerRadius: Theme.r14))
                .cardShadow()
                .padding(Theme.s4)
            }
        }
        .background(Theme.canvas)
        .navigationTitle("Subagents")
        .navigationBarTitleDisplayMode(.inline)
    }
}

/// The agent's todo list (from `todo_write`) as a checklist — the Tasks
/// destination from the session menu.
struct TasksListView: View {
    @ObservedObject var store: SessionStore

    var body: some View {
        ScrollView {
            if store.todos.isEmpty {
                Text("No tasks yet.")
                    .font(Theme.sf(13)).foregroundStyle(Theme.secondary).padding(Theme.s4)
            } else {
                VStack(spacing: 0) {
                    ForEach(Array(store.todos.enumerated()), id: \.offset) { i, todo in
                        if i > 0 { Rectangle().fill(Theme.hairline).frame(height: 0.5) }
                        HStack(spacing: Theme.s2h) {
                            icon(for: todo.status)
                            Text(todo.content)
                                .font(Theme.sf(14))
                                .foregroundStyle(todo.status == .completed ? Theme.secondary : Theme.text)
                                .strikethrough(todo.status == .completed, color: Theme.tertiary)
                            Spacer()
                        }
                        .padding(.horizontal, Theme.s3h).padding(.vertical, 11)
                    }
                }
                .background(Theme.surface)
                .clipShape(RoundedRectangle(cornerRadius: Theme.r14))
                .cardShadow()
                .padding(Theme.s4)
            }
        }
        .background(Theme.canvas)
        .navigationTitle("Tasks")
        .navigationBarTitleDisplayMode(.inline)
    }

    @ViewBuilder private func icon(for status: TodoItem.Status) -> some View {
        switch status {
        case .completed:
            Image(systemName: "checkmark.circle.fill")
                .font(.system(size: 16)).foregroundStyle(Theme.added)
        case .in_progress:
            SpinnerRing(color: Theme.amber, size: 14)
        case .pending:
            Image(systemName: "circle")
                .font(.system(size: 15)).foregroundStyle(Theme.tertiary)
        }
    }
}

/// A small indeterminate spinner ring (matches the design's purple rings).
struct SpinnerRing: View {
    let color: Color
    let size: CGFloat
    @State private var spin = false

    var body: some View {
        Circle()
            .trim(from: 0, to: 0.7)
            .stroke(color, style: StrokeStyle(lineWidth: 1.5, lineCap: .round))
            .frame(width: size, height: size)
            .rotationEffect(.degrees(spin ? 360 : 0))
            .animation(.linear(duration: 0.9).repeatForever(autoreverses: false), value: spin)
            .onAppear { spin = true }
    }
}

// MARK: - Tool-call card

/// A white card grouping a run of consecutive tool calls AND subagent groups —
/// flush rows with hairline dividers, no gaps (per the 1b/16b flattened style).
/// This unifies tool rows, "Dispatched N subagents" headers, and subagent rows
/// into one continuous card.
struct MixedToolCard: View {
    let entries: [GroupEntry]
    @ObservedObject var store: SessionStore

    var body: some View {
        VStack(spacing: 0) {
            ForEach(Array(flatRows.enumerated()), id: \.offset) { index, row in
                if index > 0 { Rectangle().fill(Theme.hairline).frame(height: 0.5) }
                row.view
            }
        }
        .background(Theme.surface)
        .clipShape(RoundedRectangle(cornerRadius: Theme.r14))
        .cardShadow()
    }

    /// Expand each entry into its flush rows: a tool = one ToolRow; a subagent
    /// group = a header row + one SubagentRow per subagent.
    private var flatRows: [FlatRow] {
        var rows: [FlatRow] = []
        for entry in entries {
            switch entry {
            case .tool(let id, let cell):
                rows.append(FlatRow(key: "t-\(id)", view: AnyView(ToolRow(cell: cell))))
            case .subagents(let id, let subs):
                rows.append(FlatRow(key: "sh-\(id)",
                    view: AnyView(SubagentHeaderRow(count: subs.count,
                                                    anyRunning: subs.contains { $0.running }))))
                for sub in subs {
                    rows.append(FlatRow(key: "s-\(sub.id)", view: AnyView(
                        NavigationLink(destination: SubagentTranscriptView(store: store, subId: sub.id, fallback: sub)) {
                            SubagentRow(sub: sub)
                        }
                        .buttonStyle(.plain))))
                }
            }
        }
        return rows
    }

    private struct FlatRow { let key: String; let view: AnyView }
}

/// The "Dispatched N subagents" header row inside a mixed card.
struct SubagentHeaderRow: View {
    let count: Int
    let anyRunning: Bool
    var body: some View {
        HStack(spacing: Theme.s2h) {
            Text("A")
                .font(Theme.sf(11, .semibold)).foregroundStyle(Theme.subBadgeFg)
                .frame(width: 22, height: 22)
                .background(Theme.subBadgeBg, in: RoundedRectangle(cornerRadius: 6))
            Text("Dispatched \(count) subagent\(count == 1 ? "" : "s")")
                .font(Theme.sf(14, .medium)).foregroundStyle(Theme.text)
            Spacer()
            if anyRunning { SpinnerRing(color: Theme.subBadgeFg, size: 10) }
        }
        .padding(.horizontal, Theme.s3h).padding(.vertical, 11)
    }
}

/// A white card grouping one *run* of consecutive tool calls (per the 1b
/// design): rows sit flush with 0.5pt hairline dividers between them — no gaps.
struct ToolGroupCard: View {
    /// The tool cells in this run, paired with a stable id (the transcript id).
    let cells: [(id: String, cell: ToolCell)]

    var body: some View {
        VStack(spacing: 0) {
            ForEach(Array(cells.enumerated()), id: \.element.id) { index, entry in
                if index > 0 {
                    Rectangle().fill(Theme.hairline).frame(height: 0.5)
                }
                ToolRow(cell: entry.cell)
            }
        }
        .background(Theme.surface)
        .clipShape(RoundedRectangle(cornerRadius: Theme.r14))
        .cardShadow()
    }
}

/// A single expandable tool-call row inside a `ToolGroupCard`.
struct ToolRow: View {
    let cell: ToolCell
    @State private var expanded = false
    @State private var showAll = false

    private var family: ToolFamily { ToolFamily.of(cell.name) }
    private var label: ToolLabel { ToolLabel.of(cell) }

    var body: some View {
        VStack(spacing: 0) {
            Button { withAnimation(.easeOut(duration: 0.2)) { expanded.toggle() } } label: {
                row
            }
            .buttonStyle(.plain)

            if expanded {
                detail
            }
        }
        // Expanded rows get a whisper-subtle tint (6c).
        .background(expanded ? Color.black.opacity(0.015) : .clear)
    }

    private var row: some View {
        HStack(spacing: Theme.s2h) {
            badge
            (Text(label.prose + (label.mono != nil ? " " : ""))
                .font(Theme.sf(14, expanded ? .medium : .regular)).foregroundColor(Theme.text)
             + monoText)
                .lineLimit(1)
            Spacer(minLength: Theme.s2)
            trailing
        }
        .padding(.horizontal, Theme.s3h).padding(.vertical, 11)
    }

    private var monoText: Text {
        guard let m = label.mono else { return Text("") }
        return Text(m).font(Theme.mono(12.5)).foregroundColor(Theme.text)
    }

    private var badge: some View {
        Text(family.letter)
            .font(Theme.sf(11, .semibold)).foregroundStyle(family.badgeFg)
            .frame(width: 22, height: 22)
            .background(family.badgeBg, in: RoundedRectangle(cornerRadius: 6))
    }

    @ViewBuilder private var trailing: some View {
        if cell.running {
            ProgressView().controlSize(.small).tint(Theme.attention)
        } else {
            HStack(spacing: 5) {
                if let count = resultCount {
                    Text(count).font(Theme.sf(13)).foregroundStyle(Theme.tertiary)
                }
                Image(systemName: expanded ? "chevron.down" : "chevron.right")
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(Theme.tertiary)
            }
        }
    }

    /// A small result count for search-like tools (best-effort from output).
    private var resultCount: String? {
        guard let out = cell.output, family == .read else { return nil }
        let lines = out.split(separator: "\n").count
        return lines > 1 ? "\(lines)" : nil
    }

    // The 6c expanded layout: labeled COMMAND (dark card) + OUTPUT (light
    // card), capped height with "Show all" / "Copy". Diffs render inline.
    @ViewBuilder private var detail: some View {
        VStack(alignment: .leading, spacing: Theme.s2) {
            if let cmd = commandText {
                sectionLabel("COMMAND")
                commandCard(cmd)
            } else if case .object = cell.input {
                sectionLabel("INPUT")
                inputCard(cell.input.pretty())
            }

            if let diff = cell.diff {
                sectionLabel("CHANGES")
                DiffView(diff: diff)
            } else if let out = cell.plainOutput, !out.isEmpty {
                sectionLabel("OUTPUT")
                outputCard(out)
            }
        }
        .padding(.horizontal, Theme.s3h).padding(.bottom, Theme.s3)
    }

    /// The command line for shell-family tools (the dark COMMAND card).
    private var commandText: String? {
        guard family == .shell else { return nil }
        return cell.input.field("command") ?? cell.input.oneLineSummary
    }

    private func sectionLabel(_ text: String) -> some View {
        Text(text)
            .font(Theme.sf(9.5, .semibold)).tracking(0.6)
            .foregroundStyle(Theme.tertiary)
            .padding(.top, 2)
    }

    // Dark card, light mono text, single-line ellipsis (per 6c COMMAND).
    private func commandCard(_ text: String) -> some View {
        Text(text)
            .font(Theme.mono(11.5)).foregroundStyle(Color(hex: 0xEDECE8))
            .lineLimit(1).truncationMode(.tail)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 11).padding(.vertical, 9)
            .background(Theme.text)
            .clipShape(RoundedRectangle(cornerRadius: 9))
    }

    // Neutral card for structured input (non-shell tools).
    private func inputCard(_ text: String) -> some View {
        ScrollView(.horizontal, showsIndicators: false) {
            Text(text).font(Theme.mono(11.5)).foregroundStyle(Theme.text.opacity(0.8))
                .textSelection(.enabled).padding(.horizontal, 11).padding(.vertical, 9)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.canvas)
        .clipShape(RoundedRectangle(cornerRadius: 9))
    }

    // Light card, capped to ~10 lines, FAIL/error lines reddened; footer with
    // "Show all N lines" + "Copy" (per 6c OUTPUT).
    private func outputCard(_ text: String) -> some View {
        let lines = text.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
        let cap = 10
        let shown = showAll ? lines : Array(lines.prefix(cap))
        let hidden = lines.count - shown.count
        return VStack(alignment: .leading, spacing: 8) {
            VStack(alignment: .leading, spacing: 0) {
                ForEach(Array(shown.enumerated()), id: \.offset) { _, line in
                    Text(line.isEmpty ? " " : line)
                        .font(Theme.mono(11)).lineSpacing(4)
                        .foregroundStyle(isFailure(line) ? Theme.removed : Theme.text.opacity(0.7))
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .lineLimit(1).truncationMode(.tail)
                }
            }
            .padding(.horizontal, 11).padding(.vertical, 9)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.canvas)
            .clipShape(RoundedRectangle(cornerRadius: 9))

            HStack {
                if hidden > 0 {
                    Button { withAnimation { showAll = true } } label: {
                        Text("Show all \(lines.count) lines")
                            .font(Theme.sf(11.5, .medium)).foregroundStyle(Theme.accent)
                    }
                }
                Spacer()
                Button {
                    UIPasteboard.general.string = text
                } label: {
                    Text("Copy").font(Theme.sf(11.5, .medium)).foregroundStyle(Theme.accent)
                }
            }
        }
    }

    private func isFailure(_ line: String) -> Bool {
        let l = line.lowercased()
        return l.contains("fail") || l.contains("error") || l.hasPrefix("✗")
    }
}

// MARK: - Markdown (assistant prose)

/// Renders assistant text as markdown: headings, lists, blockquotes, and fenced
/// code blocks. Prose is SF; agent-typed code stays monospace.
struct MarkdownText: View {
    let raw: String
    init(_ raw: String) { self.raw = raw }

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.s2) {
            ForEach(Array(segments.enumerated()), id: \.offset) { _, seg in
                switch seg {
                case .text(let s): inlineMarkdown(s)
                case .code(let code, let lang): CodeBlock(code: code, lang: lang)
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private enum Segment { case text(String), code(String, String?) }

    private var segments: [Segment] {
        var out: [Segment] = []
        var inCode = false; var lang: String? = nil; var buf: [String] = []
        for line in raw.components(separatedBy: "\n") {
            if line.hasPrefix("```") {
                if inCode {
                    out.append(.code(buf.joined(separator: "\n"), lang)); buf = []; inCode = false; lang = nil
                } else {
                    if !buf.isEmpty { out.append(.text(buf.joined(separator: "\n"))); buf = [] }
                    inCode = true
                    let tag = line.dropFirst(3).trimmingCharacters(in: .whitespaces)
                    lang = tag.isEmpty ? nil : tag
                }
            } else { buf.append(line) }
        }
        if !buf.isEmpty {
            out.append(inCode ? .code(buf.joined(separator: "\n"), lang) : .text(buf.joined(separator: "\n")))
        }
        return out
    }

    @ViewBuilder private func inlineMarkdown(_ s: String) -> some View {
        let lines = s.components(separatedBy: "\n")
        VStack(alignment: .leading, spacing: Theme.s1) {
            ForEach(Array(lines.enumerated()), id: \.offset) { _, line in
                blockLine(line)
            }
        }
    }

    @ViewBuilder private func blockLine(_ line: String) -> some View {
        let t = line.trimmingCharacters(in: .whitespaces)
        if t.isEmpty {
            Spacer().frame(height: 2)
        } else if let (level, rest) = headerParse(t) {
            inline(rest).font(headerFont(level)).foregroundStyle(Theme.text)
                .padding(.top, level <= 2 ? Theme.s1 : 0)
        } else if t.hasPrefix("> ") {
            inline(String(t.dropFirst(2))).foregroundStyle(Theme.secondary)
                .padding(.leading, Theme.s2)
                .overlay(alignment: .leading) { Rectangle().fill(Theme.hairline).frame(width: 2) }
        } else if let bullet = bulletParse(t) {
            HStack(alignment: .top, spacing: 6) {
                Text("•").foregroundStyle(Theme.secondary)
                inline(bullet).foregroundStyle(Theme.text)
            }.padding(.leading, Theme.s1)
        } else if let (num, rest) = numberParse(t) {
            HStack(alignment: .top, spacing: 6) {
                Text("\(num).").font(Theme.sf(15)).foregroundStyle(Theme.secondary)
                inline(rest).foregroundStyle(Theme.text)
            }.padding(.leading, Theme.s1)
        } else {
            inline(line).foregroundStyle(Theme.text)
        }
    }

    private func inline(_ s: String) -> Text {
        if let a = try? AttributedString(
            markdown: s, options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace)) {
            return Text(a).font(Theme.sf(15))
        }
        return Text(s).font(Theme.sf(15))
    }

    private func headerParse(_ t: String) -> (Int, String)? {
        guard t.hasPrefix("#") else { return nil }
        let h = t.prefix(while: { $0 == "#" }).count
        guard h <= 6, t.dropFirst(h).hasPrefix(" ") else { return nil }
        return (h, String(t.dropFirst(h + 1)))
    }
    private func headerFont(_ level: Int) -> Font {
        switch level {
        case 1: return Theme.sf(22, .bold)
        case 2: return Theme.sf(19, .semibold)
        case 3: return Theme.sf(16, .semibold)
        default: return Theme.sf(15, .semibold)
        }
    }
    private func bulletParse(_ t: String) -> String? {
        for p in ["- ", "* ", "+ "] where t.hasPrefix(p) { return String(t.dropFirst(2)) }
        return nil
    }
    private func numberParse(_ t: String) -> (Int, String)? {
        let d = t.prefix(while: { $0.isNumber })
        guard !d.isEmpty, let n = Int(d) else { return nil }
        let after = t.dropFirst(d.count)
        if after.hasPrefix(". ") { return (n, String(after.dropFirst(2))) }
        return nil
    }
}

/// A fenced code block — dark card with white mono text (per handoff).
struct CodeBlock: View {
    let code: String
    let lang: String?
    var body: some View {
        if lang == "diff" {
            DiffView(diff: code)
        } else {
            ScrollView(.horizontal, showsIndicators: false) {
                Text(code).font(Theme.mono(12.5)).foregroundStyle(.white)
                    .padding(Theme.s3)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.text)   // #16161A card
            .clipShape(RoundedRectangle(cornerRadius: Theme.r10))
        }
    }
}

// MARK: - Diff

/// Unified diff: added green, removed red, hunks dim — on the light canvas.
struct DiffView: View {
    let diff: String
    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(Array(diff.components(separatedBy: "\n").enumerated()), id: \.offset) { _, line in
                Text(line.isEmpty ? " " : line)
                    .font(Theme.mono(11.5))
                    .foregroundStyle(color(for: line))
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, Theme.s2h).padding(.vertical, 1)
                    .background(bg(for: line))
            }
        }
        .padding(.vertical, Theme.s1)
        .background(Theme.canvas)
        .clipShape(RoundedRectangle(cornerRadius: Theme.r8))
    }
    private func color(for line: String) -> Color {
        if line.hasPrefix("+") { return Theme.added }
        if line.hasPrefix("-") { return Theme.removed }
        if line.hasPrefix("@@") { return Theme.secondary }
        return Theme.text.opacity(0.7)
    }
    private func bg(for line: String) -> Color {
        if line.hasPrefix("+") { return Theme.added.opacity(0.10) }
        if line.hasPrefix("-") { return Theme.removed.opacity(0.10) }
        return .clear
    }
}

// MARK: - Session browser (8c)

/// Full-screen, search-first session browser. A search field, an "ACTIVE NOW"
/// card for the current session, a "RECENT" grouped list, and a pinned dark
/// "New session" button.
struct SessionBrowser: View {
    @ObservedObject var store: SessionStore
    @Binding var isOpen: Bool
    @State private var query = ""

    private var active: SessionMeta? {
        store.sessions.first { $0.id == store.activeSessionId }
    }
    private var recent: [SessionMeta] {
        store.sessions
            .filter { $0.id != store.activeSessionId }
            .filter { query.isEmpty || $0.title.localizedCaseInsensitiveContains(query) }
    }

    var body: some View {
        VStack(spacing: 0) {
            // Search row.
            HStack(spacing: Theme.s2h) {
                HStack(spacing: Theme.s2) {
                    Image(systemName: "magnifyingglass")
                        .font(.system(size: 14)).foregroundStyle(Theme.tertiary)
                    // Repo/file search is a seam — we only have titles today.
                    TextField("Search sessions", text: $query)
                        .font(Theme.sf(14)).foregroundStyle(Theme.text)
                        .autocorrectionDisabled()
                }
                .padding(.horizontal, Theme.s3).padding(.vertical, Theme.s2h)
                .background(Theme.surface)
                .clipShape(RoundedRectangle(cornerRadius: Theme.r12))
                .cardShadow()
                Button("Cancel") { isOpen = false }
                    .font(Theme.sf(15, .medium)).foregroundStyle(Theme.accent)
            }
            .padding(.horizontal, Theme.s4).padding(.top, Theme.s6)

            ScrollView {
                VStack(alignment: .leading, spacing: Theme.s2) {
                    if let active {
                        sectionLabel("ACTIVE NOW")
                        activeCard(active)
                    }
                    if !recent.isEmpty {
                        sectionLabel("RECENT").padding(.top, Theme.s3)
                        recentList
                    }
                    if store.sessions.isEmpty {
                        Text("No sessions yet")
                            .font(Theme.sf(13)).foregroundStyle(Theme.secondary)
                            .padding(.top, Theme.s4)
                    }
                }
                .padding(.horizontal, Theme.s4).padding(.top, Theme.s3h)
            }

            // Pinned New session button.
            Button {
                store.newSession(); isOpen = false
            } label: {
                Text("New session")
                    .font(Theme.sf(14.5, .semibold)).foregroundStyle(.white)
                    .frame(maxWidth: .infinity).padding(Theme.s3h)
                    .background(Theme.text)
                    .clipShape(RoundedRectangle(cornerRadius: Theme.r14))
            }
            .padding(.horizontal, Theme.s4).padding(.top, Theme.s2h)
            .padding(.bottom, Theme.s4)
        }
        .background(Theme.canvas.ignoresSafeArea())
    }

    private func sectionLabel(_ text: String) -> some View {
        Text(text)
            .font(Theme.sf(10.5, .semibold)).tracking(0.6)
            .foregroundStyle(Theme.tertiary)
            .padding(.horizontal, 2).padding(.bottom, Theme.s1)
    }

    // Green-bordered active-session card.
    private func activeCard(_ s: SessionMeta) -> some View {
        Button { isOpen = false } label: {
            VStack(alignment: .leading, spacing: 6) {
                HStack(spacing: Theme.s2) {
                    Circle().fill(Theme.added).frame(width: 7, height: 7)
                    Text(s.title.isEmpty ? "New conversation" : s.title)
                        .font(Theme.sf(14.5, .semibold)).foregroundStyle(Theme.text)
                        .lineLimit(1)
                    Spacer()
                }
                Text("\(s.message_count) messages · this session")
                    .font(Theme.mono(12)).foregroundStyle(Theme.secondary)
                    .padding(.leading, 15)
            }
            .padding(Theme.s3h)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.surface)
            .clipShape(RoundedRectangle(cornerRadius: Theme.r14))
            .overlay(RoundedRectangle(cornerRadius: Theme.r14)
                .stroke(Theme.added.opacity(0.35), lineWidth: 1))
            .cardShadow()
        }
        .buttonStyle(.plain)
    }

    private var recentList: some View {
        VStack(spacing: 0) {
            ForEach(Array(recent.enumerated()), id: \.element.id) { i, s in
                if i > 0 { Rectangle().fill(Theme.hairline).frame(height: 0.5) }
                Button {
                    UIImpactFeedbackGenerator(style: .light).impactOccurred()
                    store.loadSession(s.id); isOpen = false
                } label: {
                    VStack(alignment: .leading, spacing: 6) {
                        Text(s.title.isEmpty ? "New conversation" : s.title)
                            .font(Theme.sf(14.5, .semibold)).foregroundStyle(Theme.text)
                            .lineLimit(1)
                        // TODO status badge (needs you / done) — no session status
                        // on the wire yet.
                        Text("\(s.message_count) messages")
                            .font(Theme.sf(12)).foregroundStyle(Theme.secondary)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(Theme.s3h)
                }
                .buttonStyle(.plain)
            }
        }
        .background(Theme.surface)
        .clipShape(RoundedRectangle(cornerRadius: Theme.r14))
        .cardShadow()
    }
}

// MARK: - Session menu (14a)

/// The session menu bottom sheet (opened by tapping the header title). Grouped
/// sections: live counts, session actions, a TROUBLESHOOTING section (Debug
/// info + Export transcript), and a red "Stop the agent".
struct SessionMenu: View {
    @ObservedObject var store: SessionStore
    @Binding var isOpen: Bool
    @State private var showExport = false

    private var title: String {
        let t = store.sessions.first { $0.id == store.activeSessionId }?.title ?? ""
        return t.isEmpty ? "New session" : t
    }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(spacing: Theme.s3h) {
                    // Live counts: Tasks (todos) + Subagents.
                    if !store.todos.isEmpty {
                        card {
                            NavigationLink {
                                TasksListView(store: store)
                            } label: {
                                menuRow(icon: "☑", tint: Theme.amberText, tintBg: Theme.amberBadgeBg.opacity(0.25),
                                        label: "Tasks",
                                        trailing: "\(store.todosDone) of \(store.todos.count) ›")
                            }
                            .buttonStyle(.plain)
                        }
                    }
                    if store.totalSubagentCount > 0 {
                        card {
                            NavigationLink {
                                SubagentsListView(store: store)
                            } label: {
                                menuRow(icon: "A", tint: Theme.subBadgeFg, tintBg: Theme.subBadgeBg,
                                        label: "Subagents",
                                        trailing: store.runningSubagentCount > 0
                                            ? "\(store.runningSubagentCount) running ›"
                                            : "\(store.totalSubagentCount) total ›")
                            }
                            .buttonStyle(.plain)
                        }
                    }

                    // Interaction mode (checkmark on the active one).
                    card {
                        modeRow("Normal mode", "normal")
                        Divider().overlay(Theme.hairline)
                        modeRow("Auto-accept edits", "auto_accept")
                        Divider().overlay(Theme.hairline)
                        modeRow("Plan · read-only", "plan")
                    }

                    // Session actions.
                    card {
                        plainRow("New session") { store.newSession(); isOpen = false }
                    }

                    // Troubleshooting.
                    VStack(alignment: .leading, spacing: 7) {
                        sectionLabel("TROUBLESHOOTING")
                        card {
                            NavigationLink { DebugInfoView(store: store) } label: {
                                rowLabel("Debug info", trailing: connLabel, chevron: true)
                            }.buttonStyle(.plain)
                            Divider().overlay(Theme.hairline)
                            plainRow("Export transcript") { showExport = true }
                        }
                    }

                    // Stop.
                    Button {
                        store.cancel(); isOpen = false
                    } label: {
                        Text("Stop the agent")
                            .font(Theme.sf(15, .semibold)).foregroundStyle(Theme.removed)
                            .frame(maxWidth: .infinity).padding(Theme.s3h)
                            .background(Theme.surface)
                            .clipShape(RoundedRectangle(cornerRadius: Theme.r14))
                            .cardShadow()
                    }
                    .disabled(!store.busy)
                    .opacity(store.busy ? 1 : 0.5)
                }
                .padding(Theme.s4)
            }
            .background(Theme.bg)
            .navigationBarTitleDisplayMode(.inline)
        }
        .sheet(isPresented: $showExport) {
            ShareSheet(text: store.exportTranscript())
        }
    }

    private var connLabel: String {
        switch store.connState {
        case .online: return "online"
        case .connecting: return "connecting"
        case .reconnecting: return "reconnecting"
        case .offline: return "offline"
        }
    }

    // MARK: row builders

    @ViewBuilder private func card<C: View>(@ViewBuilder _ content: () -> C) -> some View {
        VStack(spacing: 0) { content() }
            .background(Theme.surface)
            .clipShape(RoundedRectangle(cornerRadius: Theme.r14))
            .cardShadow()
    }

    private func sectionLabel(_ t: String) -> some View {
        Text(t).font(Theme.sf(10.5, .semibold)).tracking(0.6)
            .foregroundStyle(Theme.tertiary).padding(.horizontal, 4)
    }

    private func menuRow(icon: String, tint: Color, tintBg: Color,
                         label: String, trailing: String) -> some View {
        HStack(spacing: 11) {
            Text(icon).font(Theme.sf(11, .semibold)).foregroundStyle(tint)
                .frame(width: 24, height: 24)
                .background(tintBg, in: RoundedRectangle(cornerRadius: 7))
            Text(label).font(Theme.sf(15)).foregroundStyle(Theme.text)
            Spacer()
            Text(trailing).font(Theme.sf(14)).foregroundStyle(Theme.tertiary)
        }
        .padding(Theme.s3h)
    }

    private func rowLabel(_ label: String, trailing: String? = nil, chevron: Bool = false) -> some View {
        HStack {
            Text(label).font(Theme.sf(15)).foregroundStyle(Theme.text)
            Spacer()
            if let trailing {
                Text(trailing).font(Theme.mono(12.5)).foregroundStyle(Theme.tertiary)
            }
            if chevron {
                Image(systemName: "chevron.right").font(.system(size: 12)).foregroundStyle(Theme.tertiary)
            }
        }
        .padding(Theme.s3h)
    }

    private func plainRow(_ label: String, _ action: @escaping () -> Void) -> some View {
        Button(action: action) { rowLabel(label) }.buttonStyle(.plain)
    }

    /// A selectable mode row with a checkmark on the active mode.
    private func modeRow(_ label: String, _ value: String) -> some View {
        Button {
            store.setMode(value); isOpen = false
        } label: {
            HStack {
                Text(label).font(Theme.sf(15)).foregroundStyle(Theme.text)
                Spacer()
                if store.mode == value {
                    Image(systemName: "checkmark")
                        .font(.system(size: 13, weight: .semibold)).foregroundStyle(Theme.accent)
                }
            }
            .padding(Theme.s3h)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}

/// UIKit share sheet for Export transcript.
struct ShareSheet: UIViewControllerRepresentable {
    let text: String
    func makeUIViewController(context: Context) -> UIActivityViewController {
        UIActivityViewController(activityItems: [text], applicationActivities: nil)
    }
    func updateUIViewController(_ vc: UIActivityViewController, context: Context) {}
}

// MARK: - Debug info (14b)

/// A deliberately plain diagnostics page: connection facts + a raw event log.
/// Metrics we don't track (round-trip, cost) are omitted rather than faked.
struct DebugInfoView: View {
    @ObservedObject var store: SessionStore

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: Theme.s4) {
                // Connection facts.
                VStack(alignment: .leading, spacing: 7) {
                    Text("CONNECTION").font(Theme.sf(10.5, .semibold)).tracking(0.6)
                        .foregroundStyle(Theme.tertiary)
                    VStack(spacing: 0) {
                        infoRow("Status", connLabel)
                        Divider().overlay(Theme.hairline)
                        infoRow("Session", store.activeSessionId.isEmpty ? "—" : String(store.activeSessionId.prefix(8)))
                        Divider().overlay(Theme.hairline)
                        infoRow("Messages", "\(store.transcript.count)")
                        Divider().overlay(Theme.hairline)
                        infoRow("Subagents", "\(store.totalSubagentCount) (\(store.runningSubagentCount) running)")
                    }
                    .background(Theme.surface)
                    .clipShape(RoundedRectangle(cornerRadius: Theme.r12))
                    .cardShadow()
                }

                if let err = store.lastError {
                    VStack(alignment: .leading, spacing: 7) {
                        Text("LAST ERROR").font(Theme.sf(10.5, .semibold)).tracking(0.6)
                            .foregroundStyle(Theme.tertiary)
                        Text(err).font(Theme.mono(11.5)).foregroundStyle(Theme.text)
                            .padding(Theme.s3).frame(maxWidth: .infinity, alignment: .leading)
                            .background(Theme.text.opacity(0.05))
                            .clipShape(RoundedRectangle(cornerRadius: Theme.r10))
                    }
                }

                Text("More diagnostics (round-trip, token cost) aren't tracked yet.")
                    .font(Theme.sf(11)).foregroundStyle(Theme.tertiary)
            }
            .padding(Theme.s4)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .background(Theme.canvas)
        .navigationTitle("Debug info")
        .navigationBarTitleDisplayMode(.inline)
    }

    private var connLabel: String {
        switch store.connState {
        case .online: return "online"
        case .connecting: return "connecting"
        case .reconnecting: return "reconnecting"
        case .offline: return "offline"
        }
    }

    private func infoRow(_ k: String, _ v: String) -> some View {
        HStack {
            Text(k).font(Theme.sf(13.5)).foregroundStyle(Theme.text)
            Spacer()
            Text(v).font(Theme.mono(12.5)).foregroundStyle(Theme.secondary)
        }
        .padding(.horizontal, Theme.s3h).padding(.vertical, 11)
    }
}

// MARK: - Approval sheet


/// The approval / question sheet (interrupts the stream). Light, grouped.
struct ApprovalSheet: View {
    @ObservedObject var store: SessionStore
    let ask: PendingAsk

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: Theme.s4) {
                    switch ask {
                    case .query(_, let q):
                        head(q.title, sub: q.detail)
                        options(q.options.enumerated().map { ($0.offset, $0.element, true) })
                    case .permission(_, let req, let opts):
                        head("Allow \(req.tool)?", sub: req.cwd)
                        if let p = req.preview {
                            if let diff = ToolCell.extractFence(p, lang: "diff") {
                                DiffView(diff: diff)
                            } else {
                                Text(p).font(Theme.mono(12)).foregroundStyle(Theme.text)
                                    .padding(Theme.s3).frame(maxWidth: .infinity, alignment: .leading)
                                    .background(Theme.canvas).clipShape(RoundedRectangle(cornerRadius: Theme.r10))
                            }
                        }
                        options(opts.enumerated().map { ($0.offset, $0.element.label, $0.element.allow) })
                    }
                }
                .padding(Theme.s4)
            }
            .background(Theme.bg)
            .navigationTitle("Bob needs you")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Dismiss") { store.answer(ask, choice: nil) }
                }
            }
        }
    }

    private func head(_ title: String, sub: String) -> some View {
        VStack(alignment: .leading, spacing: Theme.s1) {
            Text(title).font(Theme.sf(20, .semibold)).foregroundStyle(Theme.text)
            if !sub.isEmpty { Text(sub).font(Theme.sf(12)).foregroundStyle(Theme.secondary) }
        }
    }

    private func options(_ items: [(Int, String, Bool)]) -> some View {
        VStack(spacing: Theme.s2) {
            ForEach(items, id: \.0) { i, label, allow in
                Button {
                    UIImpactFeedbackGenerator(style: .medium).impactOccurred()
                    store.answer(ask, choice: i)
                } label: {
                    Text(label).font(Theme.sf(15, .semibold))
                        .foregroundStyle(allow ? .white : Theme.removed)
                        .frame(maxWidth: .infinity).padding(.vertical, Theme.s3)
                        .background(allow ? Theme.accent : Theme.removed.opacity(0.12))
                        .clipShape(RoundedRectangle(cornerRadius: Theme.r12))
                }
            }
        }
    }
}
