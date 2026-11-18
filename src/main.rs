use pg_query::protobuf::Token;
use std::env;
use std::fs;
use std::io::{self, Read};

#[derive(Clone, Copy, PartialEq)]
enum ParenKind {
    CreateFuncParams,
    BooleanGroup,
    InlineCall,
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
    let mut paren_stack: Vec<ParenKind> = Vec::new();
    let mut in_create_function_header = false;

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

        // Track CREATE FUNCTION header so we know its '(' is a parameter list
        if kind == Token::Create {
            stmt_indent = indent;
            in_create_function_header = true;
        } else if matches!(kind, Token::Select | Token::Update | Token::DeleteP | Token::Insert) {
            stmt_indent = indent;
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

        // Major SQL clauses -> newline aligned with statement
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
        }

        match raw {
            "(" => {
                // Look ahead to matching ')' to see if this group contains AND / OR
                let mut depth = 1;
                let mut has_bool_op = false;
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

                let pkind = if in_create_function_header && paren_stack.is_empty() {
                    ParenKind::CreateFuncParams
                } else if has_bool_op {
                    ParenKind::BooleanGroup
                } else {
                    ParenKind::InlineCall
                };

                // Add a space before '(' for boolean groups (e.g., `where (`),
                // while keeping `cleanup(` and `unnest(` attached to their names
                if pkind == ParenKind::BooleanGroup {
                    if let Some(last) = out.chars().last() {
                        if !last.is_whitespace() && last != '(' {
                            out.push(' ');
                        }
                    }
                }

                out.push('(');
                if pkind != ParenKind::InlineCall {
                    push_newline(&mut out, stmt_indent + 2);
                }
                paren_stack.push(pkind);
            }
            ")" => {
                let pkind = paren_stack.pop().unwrap_or(ParenKind::InlineCall);
                if pkind != ParenKind::InlineCall {
                    push_newline(&mut out, stmt_indent);
                }
                out.push(')');
            }
            "," => {
                let pkind = paren_stack.last().copied().unwrap_or(ParenKind::CreateFuncParams);
                if pkind == ParenKind::InlineCall {
                    out.push_str(", ");
                } else {
                    // Leading comma aligned with start of statement
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
                    // Preserve string literals verbatim
                    out.push_str(raw);
                }
                Token::And | Token::Or => {
                    push_newline(&mut out, stmt_indent + 2);
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
