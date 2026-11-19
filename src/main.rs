use pg_query::protobuf::Token;
use std::env;
use std::fs;
use std::io::{self, Read};

#[derive(Clone, Copy, PartialEq)]
enum ParenKind {
    CreateFuncParams,
    BooleanGroup,
    Subquery,
    InlineCall,
}

#[derive(Clone, Copy)]
struct ParenFrame {
    kind: ParenKind,
    close_indent: usize,      // indent of the line where '(' appeared
    prev_stmt_indent: usize,  // outer statement indent to restore on ')'
}

fn format_sql(input: &str, base_indent: usize) -> Result<String, pg_query::Error> {
    let scan_res = pg_query::scan(input)?;
    let tokens = scan_res.tokens;

    let mut out = String::with_capacity(input.len() + 256);
    if base_indent > 0 {
        out.push_str(&" ".repeat(base_indent));
    }

    let indent = base_indent;
    let mut stmt_indent = base_indent;
    let mut paren_stack: Vec<ParenFrame> = Vec::new();
    let mut in_create_function_header = false;
    let mut next_line_indent: Option<usize> = None;

    let push_newline = |out: &mut String, cols: usize| {
        while out.ends_with(' ') {
            out.pop();
        }
        if !out.ends_with('\n') && !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&" ".repeat(cols));
    };

    for i in 0..tokens.len() {
        let tok = &tokens[i];
        let raw = &input[tok.start as usize..tok.end as usize];
        let kind = Token::try_from(tok.token).unwrap_or(Token::Nul);
        let prev_kind = if i > 0 {
            Token::try_from(tokens[i - 1].token).unwrap_or(Token::Nul)
        } else {
            Token::Nul
        };

        // Only reset stmt_indent for top-level statements; subqueries set stmt_indent when '(' opens
        if kind == Token::Create {
            stmt_indent = indent;
            in_create_function_header = true;
        } else if matches!(kind, Token::Select | Token::Update | Token::DeleteP | Token::Insert | Token::With) {
            if paren_stack.last().map(|f| f.kind) != Some(ParenKind::Subquery) {
                stmt_indent = indent;
            }
        } else if kind == Token::Returns || kind == Token::As {
            in_create_function_header = false;
        }

        // Recursively format $$ ... $$ PL/pgSQL bodies using PostgreSQL's own lexer
        if kind == Token::Sconst && raw.starts_with('$') {
            if let Some(tag_end) = raw[1..].find('$').map(|idx| idx + 2) {
                let tag = &raw[..tag_end];
                if raw.ends_with(tag) && raw.len() >= tag.len() * 2 {
                    let inner = &raw[tag.len()..raw.len() - tag.len()];
                    if let Ok(formatted_inner) = format_sql(inner, indent + 2) {
                        out.push_str(tag);
                        push_newline(&mut out, indent + 2);
                        out.push_str(formatted_inner.trim_start());
                        push_newline(&mut out, indent);
                        out.push_str(tag);
                        continue;
                    }
                }
            }
        }

        // 1. Major SQL clauses -> newline aligned with statement
        if matches!(
            kind,
            Token::From
                | Token::Where
                | Token::Order
                | Token::GroupP
                | Token::Limit
                | Token::Returning
                | Token::Into
        ) {
            push_newline(&mut out, stmt_indent);
            next_line_indent = None;
        }

        // 2. JOIN (and LEFT/INNER/RIGHT/FULL/CROSS modifiers) -> aligned with previous table (+2)
        let next_kind = if i + 1 < tokens.len() {
            Token::try_from(tokens[i + 1].token).unwrap_or(Token::Nul)
        } else {
            Token::Nul
        };

        let is_join_modifier = matches!(
            kind,
            Token::Left | Token::Right | Token::InnerP | Token::Full | Token::Cross
        ) && matches!(next_kind, Token::Join | Token::OuterP);

        let after_join_modifier = matches!(
            prev_kind,
            Token::Left | Token::Right | Token::InnerP | Token::Full | Token::Cross | Token::OuterP
        );

        if is_join_modifier || (kind == Token::Join && !after_join_modifier) {
            push_newline(&mut out, stmt_indent + 2);
            next_line_indent = None;
        }

        // 3. ON -> keep on the same line as JOIN, then indent the ON condition (+4)
        if kind == Token::On {
            next_line_indent = Some(stmt_indent + 4);
        } else if raw != "(" {
            if let Some(cols) = next_line_indent.take() {
                push_newline(&mut out, cols);
            }
        }

        // 4. After SELECT (or DISTINCT), FROM, or WHERE -> indent the next token (+2)
        if kind == Token::Select {
            if next_kind != Token::Distinct {
                next_line_indent = Some(stmt_indent + 2);
            }
        } else if matches!(kind, Token::Distinct | Token::From | Token::Where) {
            next_line_indent = Some(stmt_indent + 2);
        }

        match raw {
            "(" => {
                let had_pending_indent = next_line_indent.take().is_some();

                // Detect subquery immediately starting with SELECT or WITH
                let is_subquery = matches!(next_kind, Token::Select | Token::With);

                // Look ahead to matching ')' to see if this group contains AND / OR
                let mut depth = 1;
                let mut has_bool_op = false;
                if !is_subquery {
                    for ahead in &tokens[i + 1..] {
                        let ahead_raw = &input[ahead.start as usize..ahead.end as usize];
                        let ahead_kind = Token::try_from(ahead.token).unwrap_or(Token::Nul);
                        if ahead_raw == "(" {
                            depth += 1;
                        } else if ahead_raw == ")" {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        } else if depth == 1 && matches!(ahead_kind, Token::And | Token::Or) {
                            has_bool_op = true;
                        }
                    }
                }

                let pkind = if in_create_function_header && paren_stack.is_empty() {
                    ParenKind::CreateFuncParams
                } else if is_subquery {
                    ParenKind::Subquery
                } else if has_bool_op {
                    ParenKind::BooleanGroup
                } else {
                    ParenKind::InlineCall
                };

                if matches!(pkind, ParenKind::BooleanGroup | ParenKind::Subquery) && !had_pending_indent {
                    if let Some(last) = out.chars().last() {
                        if !last.is_whitespace() && last != '(' {
                            out.push(' ');
                        }
                    }
                } else if had_pending_indent {
                    // e.g. `from (\n  select ...` or `where coalesce(a, b) = 1`
                    push_newline(&mut out, stmt_indent + 2);
                }

                let current_line = out.rsplit('\n').next().unwrap_or("");
                let leading_spaces = current_line.chars().take_while(|c| *c == ' ').count();
                // Account for a leading ", " prefix so the block aligns under the item (e.g. `lateral`), not the comma
                let open_line_indent = if current_line[leading_spaces..].starts_with(", ") {
                    leading_spaces + 2
                } else {
                    leading_spaces
                };

                out.push('(');

                let prev_stmt_indent = stmt_indent;
                if pkind == ParenKind::Subquery {
                    stmt_indent = open_line_indent + 2;
                    push_newline(&mut out, stmt_indent);
                } else if pkind != ParenKind::InlineCall {
                    push_newline(&mut out, open_line_indent + 2);
                }

                paren_stack.push(ParenFrame {
                    kind: pkind,
                    close_indent: open_line_indent,
                    prev_stmt_indent,
                });
            }
            ")" => {
                let frame = paren_stack.pop().unwrap_or(ParenFrame {
                    kind: ParenKind::InlineCall,
                    close_indent: stmt_indent,
                    prev_stmt_indent: stmt_indent,
                });

                if frame.kind != ParenKind::InlineCall {
                    push_newline(&mut out, frame.close_indent);
                }
                out.push(')');
                stmt_indent = frame.prev_stmt_indent;
            }
            "," => {
                let pkind = paren_stack
                    .last()
                    .map(|f| f.kind)
                    .unwrap_or(ParenKind::CreateFuncParams);
                if pkind == ParenKind::InlineCall {
                    out.push_str(", ");
                } else {
                    push_newline(&mut out, stmt_indent);
                    out.push_str(", ");
                }
            }
            ";" => {
                out.push(';');
                push_newline(&mut out, indent);
            }
            _ => match kind {
                Token::SqlComment | Token::CComment => {
                    out.push_str(raw);
                    push_newline(&mut out, indent);
                }
                Token::Sconst => {
                    if let Some(last) = out.chars().last() {
                        if !last.is_whitespace() && last != '(' {
                            out.push(' ');
                        }
                    }
                    out.push_str(raw);
                }
                Token::And | Token::Or => {
                    let bool_indent = match paren_stack.last() {
                        Some(frame) if frame.kind == ParenKind::BooleanGroup => {
                            frame.close_indent + 2
                        }
                        _ => stmt_indent + 2,
                    };
                    push_newline(&mut out, bool_indent);
                    out.push_str(&raw.to_lowercase());
                }
                _ => {
                    let lower = raw.to_lowercase();
                    if let Some(last) = out.chars().last() {
                        if !last.is_whitespace()
                            && last != '('
                            && last != '.'
                            && last != '%'
                            && lower != "."
                            && lower != "%"
                        {
                            out.push(' ');
                        }
                    }
                    out.push_str(&lower);
                }
            },
        }
    }

    Ok(out.trim_end().to_string() + "\n")
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let in_place = args.iter().any(|a| a == "-i" || a == "--in-place");
    let file_arg = args.iter().find(|a| !a.starts_with('-'));

    let input = match file_arg {
        Some(path) if path != "-" => fs::read_to_string(path).expect("Failed to read file"),
        _ => {
            let mut buf = String::new();
            io::stdin().read_to_string(&mut buf).expect("Failed to read stdin");
            buf
        }
    };

    // Validate syntax with PostgreSQL's real parser before formatting
    if let Err(e) = pg_query::parse(&input) {
        eprintln!("PostgreSQL syntax error: {e}");
        std::process::exit(1);
    }

    let formatted = format_sql(&input, 0).expect("Failed to format SQL");

    if in_place {
        let path = file_arg.expect("-i requires a file path");
        fs::write(path, formatted).expect("Failed to write file");
    } else {
        print!("{}", formatted);
    }
}
