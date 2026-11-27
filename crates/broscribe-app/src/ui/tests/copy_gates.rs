use super::{copy_sources, interface_sources, shipping_literals, shipping_text};
use syn::visit::{self, Visit};
use syn::{Expr, Lit, Stmt};

fn check_literals(rule: &str, check: impl Fn(&str, &str) -> bool) {
    let findings: Vec<_> = copy_sources()
        .into_iter()
        .flat_map(|(path, source)| {
            let check = &check;
            shipping_literals(&source)
                .into_iter()
                .filter({
                    let path = path.clone();
                    move |(_, words)| check(&path, words)
                })
                .map(move |(line, words)| format!("{path}:{line}: {words}"))
        })
        .collect();
    assert!(findings.is_empty(), "{rule}\n{}", findings.join("\n"));
}

fn tokens(words: &str) -> impl Iterator<Item = &str> {
    words
        .split(|c: char| !c.is_alphanumeric() && !matches!(c, '_' | '\'' | '’'))
        .filter(|word| !word.is_empty())
}

#[test]
fn copy_uses_british_spelling_and_email() {
    check_literals(
        "Use British spelling and email in visible copy.",
        |_, words| {
            let lower = words.to_ascii_lowercase();
            tokens(&lower).any(|word| {
                matches!(
                    word,
                    "recognized"
                        | "recognizing"
                        | "summarize"
                        | "summarized"
                        | "summarizing"
                        | "organization"
                        | "organizations"
                        | "canceled"
                        | "canceling"
                        | "customize"
                        | "customized"
                        | "coloring"
                        | "behavior"
                        | "behaviors"
                )
            })
        },
    );
}

#[test]
fn field_hints_use_one_example_without_eg() {
    check_literals(
        "Use a single example without e.g. in field hints.",
        |_, words| words.to_ascii_lowercase().contains("e.g."),
    );
}

#[test]
fn voice_scores_are_labelled_and_never_percentages() {
    check_literals(
        "Use voice 0.74, spoken as voice similarity 0.74.",
        |path, words| {
            if !path.starts_with("views/") {
                return false;
            }
            let lower = words.to_ascii_lowercase();
            if lower == "voice similarity" || (lower.contains("similarity") && lower.contains('%'))
            {
                return true;
            }
            lower.match_indices(":.2}").any(|(at, _)| {
                let Some((label, _)) = lower[..at].rsplit_once('{') else {
                    return true;
                };
                !label.ends_with("voice ")
                    && !label.ends_with("voice similarity ")
                    && !(path == "views/setup.rs"
                        && (label.ends_with(" · needed ") || label.ends_with(", needed ")))
            })
        },
    );
}

#[test]
fn prose_has_no_commands_variables_or_filenames() {
    check_literals(
        "Move troubleshooting syntax out of prose and into selectable details.",
        |path, words| {
            let lower = words.to_ascii_lowercase();
            let verbatim = matches!(
                (path, words),
                ("ui.rs", "broscribe maintenance disable")
                | ("views/update.rs", "broscribe update settings set --spec-stdin")
                // A Rust compiler note and an export format's file extension.
                | ("jobs.rs", "await `finished` when cleanup must wait for the reaping")
                | ("views/export.rs", "JSON (.json)")
            );
            if verbatim {
                return false;
            }

            // A bare URI, filename, environment key or flag is also used by the
            // implementation. Look for those tokens embedded in human sentences.
            if !words.contains(' ') || words.contains("\\n") && words.contains("printf") {
                return false;
            }
            let command = [
                "agents",
                "app",
                "config",
                "daemon",
                "diagnostics",
                "integrations",
                "maintenance",
                "mcp",
                "providers",
                "setup",
                "update",
            ]
            .iter()
            .any(|verb| lower.contains(&format!("broscribe {verb} ")));
            let variable = words
                .split_whitespace()
                .any(|word| word.contains("BROSCRIBE_"));
            let filename = [".toml", ".json", ".yaml", ".plist", ".exe", ".ndjson"]
                .iter()
                .any(|suffix| lower.contains(suffix));
            command || variable || filename || words.contains('`')
        },
    );
}

#[test]
fn settings_paths_and_fact_lines_use_the_house_separator() {
    check_literals(
        "Use › for settings paths and middle dots for facts.",
        |_, words| {
            words.contains("Settings →")
                || words.contains("Settings >")
                || words.contains("Security →")
                || words.contains("Security >")
                || words.starts_with("Disk: {free}")
                || words.starts_with("Third-party licences: {} components")
        },
    );
}

#[test]
fn macos_permission_purpose_strings_match_in_app_and_agent() {
    let script = include_str!("../../../../../scripts/bundle-macos.sh");
    for (key, purpose) in [
        (
            "NSMicrophoneUsageDescription",
            "BroScribe records your side of meetings and your voice sample. Audio stays on this Mac.",
        ),
        (
            "NSAudioCaptureUsageDescription",
            "BroScribe records the other side of meetings using system audio. Audio stays on this Mac.",
        ),
        (
            "NSCalendarsFullAccessUsageDescription",
            "BroScribe reads only the calendars you select, to name meetings and participants. It never changes events.",
        ),
    ] {
        let field = format!("<key>{key}</key>\n    <string>{purpose}</string>");
        assert_eq!(script.matches(&field).count(), 2, "{key}");
        assert_eq!(
            script.matches(&format!("<key>{key}</key>")).count(),
            2,
            "{key}"
        );
    }
}

#[derive(Default)]
struct ControlLabels(Vec<(usize, String)>);

impl ControlLabels {
    fn literal(&mut self, expr: &Expr) {
        match expr {
            Expr::Lit(expr) => {
                if let Lit::Str(text) = &expr.lit {
                    self.0.push((text.span().start().line, text.value()));
                }
            }
            Expr::If(expr) => {
                self.branch(&expr.then_branch);
                if let Some((_, other)) = &expr.else_branch {
                    self.literal(other);
                }
            }
            Expr::Block(expr) => self.branch(&expr.block),
            Expr::Call(expr) if matches!(expr.func.as_ref(), Expr::Path(path) if path.path.segments.last().is_some_and(|segment| matches!(segment.ident.to_string().as_str(), "line" | "confirm_line"))) => {
                if let Some(text) = expr.args.iter().nth(1) {
                    self.literal(text);
                }
            }
            Expr::Match(expr) => {
                for arm in &expr.arms {
                    self.literal(&arm.body);
                }
            }
            Expr::MethodCall(expr)
                if matches!(expr.method.to_string().as_str(), "into" | "to_owned") =>
            {
                self.literal(&expr.receiver);
            }
            Expr::Macro(expr) if expr.mac.path.is_ident("format") => {
                if let Some(proc_macro2::TokenTree::Literal(token)) =
                    expr.mac.tokens.clone().into_iter().next()
                    && let Ok(text) = syn::parse_str::<syn::LitStr>(&token.to_string())
                {
                    self.0.push((token.span().start().line, text.value()));
                }
            }
            _ => {}
        }
    }

    fn branch(&mut self, block: &syn::Block) {
        if let Some(Stmt::Expr(expr, None)) = block.stmts.last() {
            self.literal(expr);
        }
    }
}

impl<'ast> Visit<'ast> for ControlLabels {
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if matches!(
            call.method.to_string().as_str(),
            "label" | "accessibility_label" | "title"
        ) && let Some(label) = call.args.first()
        {
            self.literal(label);
        }
        visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Expr::Path(func) = call.func.as_ref()
            && let Some(segment) = func.path.segments.last()
        {
            let label = match segment.ident.to_string().as_str() {
                "heading" | "button" | "disclosure" | "section" | "settings_link" => {
                    call.args.iter().nth(1)
                }
                "new"
                    if func
                        .path
                        .segments
                        .iter()
                        .any(|segment| segment.ident == "ConfirmSpec") =>
                {
                    if let Some(label) = call.args.first() {
                        self.literal(label);
                    }
                    call.args.last()
                }
                _ => None,
            };
            if let Some(label) = label {
                self.literal(label);
            }
        }
        visit::visit_expr_call(self, call);
    }
}

fn control_words(label: &str) -> Vec<&str> {
    // Product names retain their spelling. Replace by token filtering
    // rather than permitting arbitrary title-case actions around a name.
    tokens(label)
        .filter(|word| {
            !matches!(
                *word,
                "BroScribe"
                    | "Agent"
                    | "Settings"
                    | "System"
                    | "Calendars"
                    | "People"
                    | "Library"
                    | "Transcription"
                    | "Recording"
                    | "Storage"
                    | "Templates"
                    | "Code"
                    | "Claude"
                    | "Microsoft"
                    | "Google"
                    | "Windows"
                    | "Linux"
                    | "Mac"
                    | "Zoom"
                    | "Review"
                    | "Finder"
                    | "File"
                    | "Explorer"
                    | "Permission"
                    | "Apple"
                    | "OpenAI"
                    | "Codex"
                    | "Setup"
                    | "Return"
                    | "Escape"
                    | "Me"
                    | "Others"
                    | "Standard"
                    | "Large"
                    | "Largest"
                    | "Slack"
                    | "Webex"
                    | "Teams"
                    | "Actions"
                    | "Documents"
                    | "Applications"
            ) && !word.chars().all(|c| c.is_uppercase() || c.is_ascii_digit())
        })
        .collect()
}

#[test]
fn in_window_controls_use_sentence_case_plain_glyph_free_labels_and_second_person() {
    let mut findings = Vec::new();
    for (path, source) in interface_sources() {
        let mut labels = ControlLabels::default();
        let shipping = shipping_text(&source);
        let live_lines: Vec<_> = shipping
            .lines()
            .map(|line| !line.trim().is_empty())
            .collect();
        let parsed = syn::parse_file(&source).unwrap_or_else(|e| panic!("{path}: {e}"));
        labels.visit_file(&parsed);
        for (line, label) in labels.0 {
            if !live_lines
                .get(line.saturating_sub(1))
                .copied()
                .unwrap_or(false)
            {
                continue;
            }
            let plain = label
                .split('}')
                .map(|piece| piece.split('{').next().unwrap_or_default())
                .collect::<Vec<_>>()
                .join(" ");
            let first_person = tokens(&plain).any(|word| {
                matches!(
                    word.to_ascii_lowercase().replace('’', "'").as_str(),
                    "i" | "i'm" | "i've" | "i'd" | "i'll" | "my" | "mine"
                )
            });
            let glyph = plain.contains([
                '✓', '✔', '✕', '×', '☑', '☐', '▸', '▾', '▶', '▼', '←', '→', '➜', '●', '○',
            ]);
            let title_case = control_words(&plain)
                .into_iter()
                .skip(1)
                .any(|word| word.starts_with(char::is_uppercase));
            if first_person || glyph || title_case {
                findings.push(format!("{path}:{line}: {label} (first person {first_person}, glyph {glyph}, capitalisation {title_case})"));
            }
        }
    }
    assert!(
        findings.is_empty(),
        "Use plain sentence case for in-window controls.\n{}",
        findings.join("\n")
    );
}
