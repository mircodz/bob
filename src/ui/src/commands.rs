pub const COMMANDS: &[(&str, &str)] = &[
    ("/new", "start a fresh session (new conversation)"),
    ("/clear", "clear the conversation context (agent forgets)"),
    ("/copy", "copy the last message to the clipboard"),
    ("/usage", "show token usage (session + all-time)"),
    ("/context", "show context-window usage"),
    ("/compact", "summarize & free up context now"),
    (
        "/yolo",
        "bypass permission prompts (denials and plan mode still apply)",
    ),
    ("/model", "show the current model"),
    ("/models", "list & switch models"),
    (
        "/reasoning",
        "set reasoning effort (off/low/medium/high/max)",
    ),
    ("/theme", "switch color theme (dark/light/terminal)"),
    ("/jobs", "list background jobs"),
    ("/memory", "show saved memories (project + global)"),
    ("/init", "generate an AGENTS.md describing this project"),
    ("/mcp", "list configured MCP servers"),
    ("/lsp", "show language servers & their health"),
    ("/exit", "quit bob"),
];

pub const INIT_PROMPT: &str = "Create an AGENTS.md file in the project root that will help future \
    coding assistants work effectively here. Explore the codebase first (build files, README, \
    directory layout, existing conventions), then write a concise AGENTS.md covering: what the \
    project is, how to build/test/lint/run it (the exact commands), the high-level architecture \
    and where key things live, and any important conventions or gotchas. Keep it tight and \
    factual — no fluff. If an AGENTS.md already exists, improve it rather than overwriting good \
    content.";
