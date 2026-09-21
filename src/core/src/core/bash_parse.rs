//! Bash command analysis for permission checks using `brush-parser`.
//! Parse failures return `None`; unmodeled constructs set [`Analysis::has_dynamic`].
//! Neither may be auto-allowed.

use brush_parser::ast;

/// Raw command words; expansions and quoting set [`Analysis::has_dynamic`].
pub type Argv = Vec<String>;

#[derive(Debug, Default, Clone)]
pub struct Analysis {
    /// Commands found in modeled portions of the input; incomplete if dynamic.
    pub commands: Vec<Argv>,
    /// A pipeline feeds an interpreter (`… | sh`).
    pub pipes_to_shell: bool,
    /// True when effects or expansions are not fully modeled.
    pub has_dynamic: bool,
    /// Output redirections mutate files independently of the command name.
    /// Input-only redirections do not set this.
    pub has_output_redirect: bool,
    /// Literal redirection targets, checked separately from command arguments.
    pub redirect_paths: Vec<String>,
}

const SHELL_INTERPRETERS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "ksh", "fish", "python", "python3", "node", "ruby", "perl", "php",
];

fn base_name(cmd: &str) -> &str {
    cmd.rsplit('/').next().unwrap_or(cmd)
}

pub fn analyze(raw: &str) -> Option<Analysis> {
    let mut a = Analysis::default();
    walk_program(raw, &mut a, 0)?;
    Some(a)
}

// Bound substitution recursion to prevent stack exhaustion.
const MAX_DEPTH: usize = 24;

fn walk_program(raw: &str, a: &mut Analysis, depth: usize) -> Option<()> {
    if depth > MAX_DEPTH {
        return None;
    }
    let opts = brush_parser::ParserOptions::default();
    let mut parser =
        brush_parser::Parser::new(std::io::Cursor::new(raw.as_bytes().to_vec()), &opts);
    let program = parser.parse_program().ok()?;
    for cc in &program.complete_commands {
        walk_compound_list(cc, a, depth)?;
    }
    Some(())
}

fn walk_compound_list(list: &ast::CompoundList, a: &mut Analysis, depth: usize) -> Option<()> {
    for item in &list.0 {
        walk_and_or(&item.0, a, depth)?;
    }
    Some(())
}

fn walk_and_or(list: &ast::AndOrList, a: &mut Analysis, depth: usize) -> Option<()> {
    walk_pipeline(&list.first, a, depth)?;
    for ao in &list.additional {
        let p = match ao {
            ast::AndOr::And(p) | ast::AndOr::Or(p) => p,
        };
        walk_pipeline(p, a, depth)?;
    }
    Some(())
}

fn walk_pipeline(p: &ast::Pipeline, a: &mut Analysis, depth: usize) -> Option<()> {
    for (i, cmd) in p.seq.iter().enumerate() {
        if i > 0 {
            if let ast::Command::Simple(sc) = cmd {
                if let Some(name) = simple_command_name(sc) {
                    if SHELL_INTERPRETERS.contains(&base_name(&name)) {
                        a.pipes_to_shell = true;
                    }
                }
            }
        }
        walk_command(cmd, a, depth)?;
    }
    Some(())
}

fn walk_command(cmd: &ast::Command, a: &mut Analysis, depth: usize) -> Option<()> {
    match cmd {
        ast::Command::Simple(sc) => walk_simple(sc, a, depth),
        ast::Command::Compound(cc, redirects) => {
            walk_compound(cc, a, depth)?;
            walk_redirect_list(redirects.as_ref(), a, depth)
        }
        // Inspect function bodies even though definitions do not execute them.
        ast::Command::Function(f) => {
            a.has_dynamic = true;
            walk_compound(&f.body.0, a, depth)
        }
        // `[[ … ]]` can execute substitutions despite having no command name.
        ast::Command::ExtendedTest(e, redirects) => {
            a.has_dynamic = true;
            walk_extended_test(&e.expr, a, depth)?;
            walk_redirect_list(redirects.as_ref(), a, depth)
        }
    }
}

fn walk_extended_test(e: &ast::ExtendedTestExpr, a: &mut Analysis, depth: usize) -> Option<()> {
    match e {
        ast::ExtendedTestExpr::And(l, r) | ast::ExtendedTestExpr::Or(l, r) => {
            walk_extended_test(l, a, depth)?;
            walk_extended_test(r, a, depth)
        }
        ast::ExtendedTestExpr::Not(inner) | ast::ExtendedTestExpr::Parenthesized(inner) => {
            walk_extended_test(inner, a, depth)
        }
        ast::ExtendedTestExpr::UnaryTest(_, w) => walk_word(&w.value, a, depth),
        ast::ExtendedTestExpr::BinaryTest(_, l, r) => {
            walk_word(&l.value, a, depth)?;
            walk_word(&r.value, a, depth)
        }
    }
}

fn walk_redirect_list(
    redirects: Option<&ast::RedirectList>,
    a: &mut Analysis,
    depth: usize,
) -> Option<()> {
    if let Some(list) = redirects {
        for r in &list.0 {
            note_redirect(r, a, depth)?;
        }
    }
    Some(())
}

// Input-only redirections can still execute substitutions in their targets.
fn note_redirect(r: &ast::IoRedirect, a: &mut Analysis, depth: usize) -> Option<()> {
    match r {
        ast::IoRedirect::File(_, kind, target) => {
            use ast::IoFileRedirectKind::*;
            if matches!(
                kind,
                Write | Append | Clobber | ReadAndWrite | DuplicateOutput
            ) {
                a.has_output_redirect = true;
            }
            match target {
                ast::IoFileRedirectTarget::Filename(w) => {
                    a.redirect_paths.push(w.value.clone());
                    walk_word(&w.value, a, depth)?;
                }
                ast::IoFileRedirectTarget::Duplicate(w) => {
                    a.has_dynamic = true;
                    walk_word(&w.value, a, depth)?;
                }
                ast::IoFileRedirectTarget::ProcessSubstitution(_, sub) => {
                    walk_compound_list(&sub.list, a, depth)?
                }
                ast::IoFileRedirectTarget::Fd(_) => {}
            }
        }
        ast::IoRedirect::OutputAndError(w, _) => {
            a.has_output_redirect = true;
            a.redirect_paths.push(w.value.clone());
            walk_word(&w.value, a, depth)?;
        }
        ast::IoRedirect::HereString(_, w) => walk_word(&w.value, a, depth)?,
        ast::IoRedirect::HereDocument(_, _) => a.has_dynamic = true,
    }
    Some(())
}

fn walk_compound(cc: &ast::CompoundCommand, a: &mut Analysis, depth: usize) -> Option<()> {
    match cc {
        ast::CompoundCommand::Subshell(s) => walk_compound_list(&s.list, a, depth),
        ast::CompoundCommand::BraceGroup(b) => walk_compound_list(&b.list, a, depth),
        ast::CompoundCommand::ForClause(f) => {
            // Expansion of the iteration list is not modeled.
            a.has_dynamic = true;
            walk_compound_list(&f.body.list, a, depth)
        }
        ast::CompoundCommand::WhileClause(w) | ast::CompoundCommand::UntilClause(w) => {
            walk_compound_list(&w.0, a, depth)?;
            walk_compound_list(&w.1.list, a, depth)
        }
        ast::CompoundCommand::IfClause(i) => {
            walk_compound_list(&i.condition, a, depth)?;
            walk_compound_list(&i.then, a, depth)?;
            if let Some(elses) = &i.elses {
                for elif in elses {
                    if let Some(cond) = &elif.condition {
                        walk_compound_list(cond, a, depth)?;
                    }
                    walk_compound_list(&elif.body, a, depth)?;
                }
            }
            Some(())
        }
        ast::CompoundCommand::CaseClause(c) => {
            a.has_dynamic = true;
            for case in &c.cases {
                if let Some(cmds) = &case.cmd {
                    walk_compound_list(cmds, a, depth)?;
                }
            }
            Some(())
        }
        ast::CompoundCommand::ArithmeticForClause(f) => {
            a.has_dynamic = true;
            walk_compound_list(&f.body.list, a, depth)
        }
        // These effects are not modeled; never auto-allow.
        ast::CompoundCommand::Arithmetic(_) | ast::CompoundCommand::Coprocess(_) => {
            a.has_dynamic = true;
            Some(())
        }
    }
}

fn walk_simple(sc: &ast::SimpleCommand, a: &mut Analysis, depth: usize) -> Option<()> {
    let mut argv: Argv = Vec::new();
    if let Some(word) = &sc.word_or_name {
        if word.value.contains(['$', '`', '*', '?', '[', '~']) {
            a.has_dynamic = true;
        }
        argv.push(word.value.clone());
        walk_word(&word.value, a, depth)?;
    }
    if let Some(prefix) = &sc.prefix {
        for item in &prefix.0 {
            walk_prefix_suffix_item(item, &mut argv, a, depth)?;
        }
    }
    if let Some(suffix) = &sc.suffix {
        for item in &suffix.0 {
            walk_prefix_suffix_item(item, &mut argv, a, depth)?;
        }
    }
    if !argv.is_empty() {
        a.commands.push(argv);
    }
    Some(())
}

fn walk_prefix_suffix_item(
    item: &ast::CommandPrefixOrSuffixItem,
    argv: &mut Argv,
    a: &mut Analysis,
    depth: usize,
) -> Option<()> {
    match item {
        ast::CommandPrefixOrSuffixItem::Word(w) => {
            argv.push(w.value.clone());
            walk_word(&w.value, a, depth)?;
        }
        ast::CommandPrefixOrSuffixItem::AssignmentWord(_, w) => {
            // Assignments can change program loading and shell interpretation.
            a.has_dynamic = true;
            walk_word(&w.value, a, depth)?;
        }
        ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, sub) => {
            walk_compound_list(&sub.list, a, depth)?;
        }
        ast::CommandPrefixOrSuffixItem::IoRedirect(r) => note_redirect(r, a, depth)?,
    }
    Some(())
}

fn simple_command_name(sc: &ast::SimpleCommand) -> Option<String> {
    sc.word_or_name.as_ref().map(|w| w.value.clone())
}

// Brush leaves word substitutions unparsed; recursively analyze their commands.
fn walk_word(raw: &str, a: &mut Analysis, depth: usize) -> Option<()> {
    // Raw argv retains quoting and glob syntax; do not classify it as literal.
    if raw.contains(['\'', '"', '\\', '*', '?', '[', '~', '{', '}']) {
        a.has_dynamic = true;
    }
    if !raw.contains('$') && !raw.contains('`') {
        return Some(());
    }
    let opts = brush_parser::ParserOptions::default();
    let pieces = brush_parser::word::parse(raw, &opts).ok()?;
    for piece in &pieces {
        collect_substitutions(&piece.piece, a, depth)?;
    }
    Some(())
}

fn collect_substitutions(
    piece: &brush_parser::word::WordPiece,
    a: &mut Analysis,
    depth: usize,
) -> Option<()> {
    use brush_parser::word::WordPiece;
    match piece {
        WordPiece::CommandSubstitution(s) | WordPiece::BackquotedCommandSubstitution(s) => {
            // Its output may become arguments or paths, even when the body is safe.
            a.has_dynamic = true;
            walk_program(s, a, depth + 1)?;
        }
        WordPiece::DoubleQuotedSequence(seq) | WordPiece::GettextDoubleQuotedSequence(seq) => {
            for p in seq {
                collect_substitutions(&p.piece, a, depth)?;
            }
        }
        // Arithmetic can contain unmodeled command substitutions.
        WordPiece::ArithmeticExpression(_) => a.has_dynamic = true,
        // Parameter expansion and other unmodeled pieces can hide execution.
        _ => a.has_dynamic = true,
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmodeled_expansions_fail_closed() {
        for (case, raw) in [
            ("heredoc substitution", "cat <<EOF\n$(printf hello)\nEOF\n"),
            ("indented heredoc", "cat <<-EOF\n\t`printf hello`\nEOF\n"),
            ("quoted heredoc", "cat <<'EOF'\nplain text\nEOF\n"),
            (
                "for substitution",
                "for item in $(printf hello); do printf ok; done",
            ),
            (
                "for backticks",
                "for item in `printf hello`; do printf ok; done",
            ),
            (
                "arithmetic for",
                "for ((i = $(printf 0); i < 1; i++)); do printf ok; done",
            ),
            (
                "case substitution",
                "case $(printf hello) in hello) printf ok;; esac",
            ),
            ("parameter default", "printf ${value:-$(printf hello)}"),
            ("arithmetic expansion", "printf $((1 + 2))"),
            ("dynamic command", "$(printf printf) hello"),
            ("assignment", "SHELL_VALUE=hello printf ok"),
            ("glob", "cat *.txt"),
            ("quoted path", "cat '../outside/file'"),
        ] {
            assert!(
                analyze(raw).is_none_or(|analysis| analysis.has_dynamic),
                "{case}: {raw}"
            );
            assert!(
                !crate::core::permissions::parse_bash(raw).analyzable,
                "{case}: {raw}"
            );
        }
    }

    #[test]
    fn malformed_input_fails_closed() {
        for (case, raw) in [
            ("unterminated substitution", "echo $("),
            ("unterminated quote", "echo 'unterminated"),
            ("unterminated double quote", "echo \"unterminated"),
            ("missing pipeline target", "cat file |"),
            ("missing redirect target", "cat >"),
        ] {
            assert!(analyze(raw).is_none(), "{case}: {raw}");
            let parsed = crate::core::permissions::parse_bash(raw);
            assert!(!parsed.analyzable, "{case}");
            assert!(
                parsed.commands.is_empty(),
                "partial analysis must not escape: {case}"
            );
            assert!(parsed.redirect_paths.is_empty(), "{case}");
            assert_eq!(parsed.raw, raw);
        }
    }

    #[test]
    fn redirection_paths_are_separate_from_argv() {
        let analysis = analyze("cat < input > output").unwrap();
        assert_eq!(analysis.commands, vec![vec!["cat".to_string()]]);
        assert_eq!(analysis.redirect_paths, ["input", "output"]);
        assert!(analysis.has_output_redirect);
    }

    fn names(raw: &str) -> Vec<String> {
        analyze(raw)
            .unwrap()
            .commands
            .iter()
            .filter_map(|c| c.first().map(|n| base_name(n).to_string()))
            .collect()
    }

    #[test]
    fn command_lists_preserve_order_and_arguments() {
        for (case, raw, expected) in [
            ("plain", "ls -la", vec!["ls"]),
            ("and chain", "cd foo && ls -la", vec!["cd", "ls"]),
            ("pipeline", "a | b | c", vec!["a", "b", "c"]),
            ("semicolon list", "x ; y ; z", vec!["x", "y", "z"]),
        ] {
            assert_eq!(names(raw), expected, "{case}: {raw}");
        }
        let parsed = crate::core::permissions::parse_bash("cd foo && ls -la");
        assert!(parsed.analyzable);
        assert_eq!(parsed.commands, vec![vec!["cd", "foo"], vec!["ls", "-la"]]);
    }

    #[test]
    fn nested_commands_are_surfaced() {
        for (case, raw, expected) in [
            ("substitution", "echo $(rm -rf ~)", vec!["echo", "rm"]),
            ("backticks", "echo `rm -rf /`", vec!["echo", "rm"]),
            (
                "nested substitution",
                "echo $(echo $(rm x))",
                vec!["echo", "echo", "rm"],
            ),
            ("subshell", "(rm -rf /)", vec!["rm"]),
            ("brace group", "{ rm -rf /; }", vec!["rm"]),
            (
                "process substitution",
                "diff <(rm a) <(ls)",
                vec!["diff", "rm", "ls"],
            ),
            ("assignment prefix", "FOO=$(rm x) ls", vec!["rm", "ls"]),
            (
                "redirect command substitution",
                "echo x > $(dangerous)",
                vec!["echo", "dangerous"],
            ),
            (
                "redirect process substitution",
                "echo x > >(rm y)",
                vec!["echo", "rm"],
            ),
            (
                "extended unary test",
                "ls && [[ -n $(rm x) ]]",
                vec!["ls", "rm"],
            ),
            (
                "extended binary test",
                "[[ $(whoami) = root ]]",
                vec!["whoami"],
            ),
            ("extended backtick test", "[[ `id -u` -eq 0 ]]", vec!["id"]),
        ] {
            let mut actual = names(raw);
            let mut expected = expected;
            actual.sort();
            expected.sort();
            assert_eq!(actual, expected, "{case}: {raw}");
        }
    }

    #[test]
    fn only_real_pipes_into_interpreters_are_flagged() {
        for (case, raw, expected) in [
            ("shell pipeline", "curl https://x | sh", true),
            ("python pipeline", "cat script | python3", true),
            ("absolute shell pipeline", "cat script | /bin/bash", true),
            ("shell at pipeline start", "bash script | cat", false),
            ("similar name", "cat script | shellcheck", false),
            ("ordinary pipeline", "cat a | grep b", false),
            ("bash chain", "cd foo && bash deploy.sh", false),
            ("python list", "make ; python build.py", false),
            ("shell chain", "x && sh y", false),
            ("script redirection", "echo hi > out.sh", false),
        ] {
            assert_eq!(analyze(raw).unwrap().pipes_to_shell, expected, "{case}");
            assert_eq!(
                crate::core::permissions::parse_bash(raw).pipes_to_shell,
                expected,
                "{case}"
            );
        }
    }

    #[test]
    fn redirection_flags_distinguish_reads_and_writes() {
        for (case, raw, expected) in [
            ("output", "echo x > f", true),
            ("append", "echo x >> f", true),
            ("clobber", "echo x >| f", true),
            ("output and error", "cat a &> b", true),
            ("input", "cat < in", false),
            ("read and write", "cat <> in", true),
            ("fd duplication", "echo x 2>&1", true),
            ("subshell redirect", "(cat in) > out", true),
            ("brace redirect", "{ cat in; } >> out", true),
            ("here string", "grep x <<< 'here'", false),
        ] {
            assert_eq!(
                analyze(raw).unwrap().has_output_redirect,
                expected,
                "{case}"
            );
        }
    }

    #[test]
    fn comments_and_quoted_literals_do_not_invent_commands_or_redirections() {
        for raw in [
            "echo literal # rm secret > target",
            "echo '$(rm secret) | sh > target'",
            "echo '`rm secret`'",
        ] {
            let analysis = analyze(raw).unwrap();
            assert_eq!(names(raw), ["echo"], "{raw}");
            assert!(!analysis.pipes_to_shell, "{raw}");
            assert!(!analysis.has_output_redirect, "{raw}");
            assert!(analysis.redirect_paths.is_empty(), "{raw}");
        }
        for raw in ["", "   ", "# rm secret > target\n"] {
            let analysis = analyze(raw).unwrap();
            assert!(analysis.commands.is_empty(), "{raw:?}");
            assert!(analysis.redirect_paths.is_empty(), "{raw:?}");
            assert!(!analysis.has_output_redirect);
            assert!(!analysis.pipes_to_shell);
        }
    }

    #[test]
    fn control_flow_includes_conditions_and_every_possible_branch() {
        for (raw, expected) in [
            (
                "if test -f x; then cat x; elif test -f y; then cat y; else printf missing; fi",
                vec![
                    vec!["test", "-f", "x"],
                    vec!["cat", "x"],
                    vec!["test", "-f", "y"],
                    vec!["cat", "y"],
                    vec!["printf", "missing"],
                ],
            ),
            (
                "while test -f x; do rm x; done",
                vec![vec!["test", "-f", "x"], vec!["rm", "x"]],
            ),
            (
                "until test -f x; do touch x; done",
                vec![vec!["test", "-f", "x"], vec!["touch", "x"]],
            ),
        ] {
            let analysis = analyze(raw).unwrap();
            assert!(!analysis.has_dynamic, "{raw}");
            assert_eq!(analysis.commands, expected, "{raw}");
            assert!(!analysis.has_output_redirect);
            assert!(analysis.redirect_paths.is_empty());
        }
    }

    #[test]
    fn deeply_nested_substitutions_fail_closed_without_leaking_partial_analysis() {
        let mut raw = "printf leaf".to_string();
        for _ in 0..40 {
            raw = format!("echo $({raw})");
        }
        assert!(analyze(&raw).is_none());
        let parsed = crate::core::permissions::parse_bash(&raw);
        assert!(!parsed.analyzable);
        assert!(parsed.commands.is_empty());
        assert_eq!(parsed.raw, raw);
    }
}
