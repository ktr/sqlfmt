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
    close_indent: usize,
    prev_stmt_indent: usize,
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
    let mut in_returns_type = false;
    let mut in_between = false;
    let mut next_line_indent: Option<usize> = None;
    let mut in_cte_list = false;
    let mut cte_indent = base_indent;

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
        let prev_raw = if i > 0 {
            &input[tokens[i - 1].start as usize..tokens[i - 1].end as usize]
        } else {
            ""
        };
        let next_kind = if i + 1 < tokens.len() {
            Token::try_from(tokens[i + 1].token).unwrap_or(Token::Nul)
        } else {
            Token::Nul
        };

        // Track CREATE FUNCTION header, RETURNS <type> boundary, and CTE blocks
        if kind == Token::Create {
            stmt_indent = indent;
            in_create_function_header = true;
            in_returns_type = false;
        } else if kind == Token::Returns {
            in_create_function_header = false;
            in_returns_type = true;
        } else if in_returns_type
            && paren_stack.is_empty()
            && matches!(
                kind,
                Token::Language
                    | Token::As
                    | Token::Set
                    | Token::Immutable
                    | Token::Stable
                    | Token::Volatile
                    | Token::StrictP
                    | Token::Security
                    | Token::Parallel
                    | Token::Cost
                    | Token::Rows
            )
        {
            in_returns_type = false;
            push_newline(&mut out, indent);
        } else if kind == Token::As && paren_stack.is_empty() && stmt_indent == indent {
            in_create_function_header = false;
            in_returns_type = false;
            push_newline(&mut out, indent);
        } else if kind == Token::As {
            in_create_function_header = false;
        } else if kind == Token::With && paren_stack.is_empty() {
            in_cte_list = true;
            cte_indent = stmt_indent;
        } else if in_cte_list
            && paren_stack.is_empty()
            && matches!(
                kind,
                Token::Select | Token::Insert | Token::Update | Token::DeleteP
            )
        {
            in_cte_list = false;
        }

        // Recursively format $$ ... $$ PL/pgSQL bodies using PostgreSQL's own lexer
        if kind == Token::Sconst && raw.starts_with('$') {
            if let Some(tag_end) = raw[1..].find('$').map(|idx| idx + 2) {
                let tag = &raw[..tag_end];
                if raw.ends_with(tag) && raw.len() >= tag.len() * 2 {
                    let inner = &raw[tag.len()..raw.len() - tag.len()];
                    if let Ok(formatted_inner) = format_sql(inner, indent + 2) {
                        if let Some(last) = out.chars().last() {
                            if !last.is_whitespace() && last != '(' {
                                out.push(' ');
                            }
                        }
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

        // 1. Major SQL clauses & PL/pgSQL block boundaries
        if matches!(kind, Token::Declare | Token::BeginP) {
            push_newline(&mut out, indent);
            stmt_indent = indent + 2;
            next_line_indent = None;
        } else if kind == Token::EndP {
            if next_kind == Token::IfP {
                // Closing `END IF;` inside PL/pgSQL dedents one level
                stmt_indent = stmt_indent.saturating_sub(2).max(indent + 2);
            } else {
                // Closing `END;` of a BEGIN..END block
                stmt_indent = indent;
            }
            push_newline(&mut out, stmt_indent);
            next_line_indent = None;
        } else if kind == Token::Ident
            && raw.eq_ignore_ascii_case("elsif")
            && stmt_indent > indent + 2
        {
            stmt_indent -= 2;
            push_newline(&mut out, stmt_indent);
            next_line_indent = None;
        } else if kind == Token::Else && stmt_indent > indent + 2 {
            // ELSE inside an IF block aligns with the matching IF
            push_newline(&mut out, stmt_indent - 2);
            next_line_indent = None;
        } else if matches!(
            kind,
            Token::From
                | Token::Where
                | Token::Order
                | Token::GroupP
                | Token::Limit
                | Token::Returning
                | Token::Union
        ) {
            push_newline(&mut out, stmt_indent);
            next_line_indent = None;
        }

        // 2. JOIN (and LEFT/INNER/RIGHT/FULL/CROSS modifiers) -> aligned with previous table (+2)
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

        // Capture the exact indentation where `WITH` was placed
        if kind == Token::With && paren_stack.is_empty() {
            in_cte_list = true;
            let current_line = out.rsplit('\n').next().unwrap_or("");
            cte_indent = current_line.chars().take_while(|c| *c == ' ').count();
        }

        // 4. Post-keyword triggers to indent the following token
        if kind == Token::Select {
            if next_kind != Token::Distinct {
                next_line_indent = Some(stmt_indent + 2);
            }
        } else if matches!(kind, Token::Distinct | Token::From | Token::Where) {
            next_line_indent = Some(stmt_indent + 2);
        } else if kind == Token::By && matches!(prev_kind, Token::Order | Token::GroupP) {
            next_line_indent = Some(stmt_indent + 2);
        } else if matches!(kind, Token::Declare | Token::BeginP) {
            next_line_indent = Some(stmt_indent);
        } else if kind == Token::Then {
            stmt_indent += 2;
            next_line_indent = Some(stmt_indent);
        } else if kind == Token::Else && stmt_indent > indent + 2 {
            next_line_indent = Some(stmt_indent);
        } else if kind == Token::Union {
            if next_kind != Token::All {
                next_line_indent = Some(stmt_indent);
            }
        } else if kind == Token::All && prev_kind == Token::Union {
            next_line_indent = Some(stmt_indent);
        }

        match raw {
            "(" => {
                let had_pending_indent = next_line_indent.take().is_some();
                let is_subquery = matches!(next_kind, Token::Select | Token::With);

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

                if matches!(pkind, ParenKind::BooleanGroup | ParenKind::Subquery)
                    && !had_pending_indent
                {
                    if let Some(last) = out.chars().last() {
                        if !last.is_whitespace() && last != '(' {
                            out.push(' ');
                        }
                    }
                } else if had_pending_indent {
                    push_newline(&mut out, stmt_indent + 2);
                }

                let current_line = out.rsplit('\n').next().unwrap_or("");
                let leading_spaces = current_line.chars().take_while(|c| *c == ' ').count();
                // Account for a leading ", " prefix in FROM/SELECT lists, but NOT for top-level CTE definitions
                let open_line_indent = if current_line[leading_spaces..].starts_with(", ")
                    && !(in_cte_list && paren_stack.is_empty())
                {
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

                // After closing a CTE `)`, put the following SELECT / INSERT / UPDATE / DELETE on a new line
                if frame.kind == ParenKind::Subquery
                    && matches!(
                        next_kind,
                        Token::Select | Token::With | Token::Insert | Token::Update | Token::DeleteP
                    )
                {
                    next_line_indent = Some(stmt_indent);
                }
            }
            "[" | "]" => {
                // Keep array brackets tight: `bigint[]`, `array[]`, `arr[1]`
                out.push_str(raw);
            }
            "," => {
                if paren_stack.is_empty() && in_cte_list {
                    push_newline(&mut out, cte_indent);
                    out.push_str(", ");
                } else {
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
            }
            ";" => {
                in_between = false;
                out.push(';');
                push_newline(&mut out, stmt_indent);
            }
            _ => match kind {
                Token::SqlComment | Token::CComment => {
                    out.push_str(raw);
                    push_newline(&mut out, stmt_indent);
                }
                Token::Sconst => {
                    if let Some(last) = out.chars().last() {
                        if !last.is_whitespace() && last != '(' && last != '[' {
                            out.push(' ');
                        }
                    }
                    out.push_str(raw);
                }
                Token::Between => {
                    in_between = true;
                    if let Some(last) = out.chars().last() {
                        if !last.is_whitespace() && last != '(' {
                            out.push(' ');
                        }
                    }
                    out.push_str("between");
                }
                Token::And if in_between => {
                    in_between = false;
                    out.push_str(" and");
                }
                Token::Or if prev_kind == Token::Create && next_kind == Token::Replace => {
                    out.push_str(" or");
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
                    let no_space_before =
                        lower == "." || lower == "%" || kind == Token::Typecast;
                    let after_tight_token = prev_raw == "("
                        || prev_raw == "["
                        || prev_raw == "."
                        || prev_raw == "%"
                        || prev_kind == Token::Typecast;

                    if let Some(last) = out.chars().last() {
                        if !last.is_whitespace() && !no_space_before && !after_tight_token {
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
