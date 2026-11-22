use pg_query::protobuf::Token;
use std::env;
use std::fs;
use std::io::{self, Read};

#[derive(Clone, Copy, PartialEq)]
enum ParenKind {
    CreateFuncParams,
    BooleanGroup,
    Subquery,
    WindowSpec,
    InlineCall,
}

#[derive(Clone, Copy)]
struct ParenFrame {
    kind: ParenKind,
    close_indent: usize,
    prev_stmt_indent: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum BlockKind {
    /// `begin ... [exception ...] end`
    Begin,
    /// `if ... then ... [elsif ...] [else ...] end if`
    If,
    /// `[for|foreach|while ...] loop ... end loop`
    Loop,
    /// PL/pgSQL `case` statement: `case ... when ... then ... end case`
    CaseStmt,
}

/// A PL/pgSQL control block. `indent` is the column of the keyword that opened it,
/// which is also where its `else`/`elsif`/`exception`/`end` go.
#[derive(Clone, Copy)]
struct Block {
    kind: BlockKind,
    indent: usize,
    /// Begin: inside the `exception` section. Loop: still in the `for`/`while` header.
    flag: bool,
    /// Byte offset of the line the block opened on (Loop headers only).
    line_start: usize,
    /// Index of the token that opened the block.
    first_tok: usize,
}

/// Joins a wrapped `for`/`while` loop header (everything in `out` from `start`) back onto
/// one line: each line break, with the indentation after it, becomes a single space, or
/// nothing just inside parentheses.
fn join_header_lines(out: &mut String, start: usize) {
    let header = out.split_off(start);
    let mut lines = header.split('\n');
    out.push_str(lines.next().unwrap_or(""));
    for line in lines {
        let line = line.trim_start();
        if !(out.ends_with('(') || line.starts_with(')') || line.is_empty()) {
            out.push(' ');
        }
        out.push_str(line);
    }
}

fn line_start(out: &str) -> usize {
    out.rfind('\n').map(|p| p + 1).unwrap_or(0)
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
    let mut in_declare_section = false;
    let mut in_create_function = false;
    let mut in_create_index = false;
    let mut storage_params_next = false;
    let mut case_stack: Vec<usize> = Vec::new();
    let mut block_stack: Vec<Block> = Vec::new();
    let mut declare_indent = base_indent;

    let tok_kind = |j: usize| Token::try_from(tokens[j].token).unwrap_or(Token::Nul);
    let tok_raw = |j: usize| &input[tokens[j].start as usize..tokens[j].end as usize];
    let is_word = |j: usize, w: &str| j < tokens.len() && tok_raw(j).eq_ignore_ascii_case(w);

    let push_newline = |out: &mut String, cols: usize| {
        while out.ends_with(' ') {
            out.pop();
        }
        if !out.ends_with('\n') && !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&" ".repeat(cols));
    };

    let is_comment = |j: usize| matches!(tok_kind(j), Token::SqlComment | Token::CComment);
    // Comments go on their own line(s) just above the line holding the next token. They're
    // collected here and placed once that token has been written (`place_comments_at`).
    let mut pending_comments: Vec<&str> = Vec::new();
    let mut place_comments_at: Option<usize> = None;

    for i in 0..tokens.len() {
        if let Some(pos) = place_comments_at.take() {
            let (at, added) = insert_comments(&mut out, pos, &pending_comments);
            for b in block_stack.iter_mut() {
                if b.line_start >= at {
                    b.line_start += added;
                }
            }
            pending_comments.clear();
        }

        let tok = &tokens[i];
        let raw = &input[tok.start as usize..tok.end as usize];
        let kind = Token::try_from(tok.token).unwrap_or(Token::Nul);
        if is_comment(i) {
            pending_comments.push(raw.trim_end());
            continue;
        }
        if !pending_comments.is_empty() {
            place_comments_at = Some(out.trim_end().len());
        }

        // Neighbouring tokens, skipping comments
        let prev_sig = (0..i).rev().find(|&j| !is_comment(j));
        let next_sig = (i + 1..tokens.len()).find(|&j| !is_comment(j));
        let prev_kind = prev_sig.map_or(Token::Nul, |j| tok_kind(j));
        let prev_raw = prev_sig.map_or("", |j| tok_raw(j));
        let next_kind = next_sig.map_or(Token::Nul, |j| tok_kind(j));

        // Start of a PL/pgSQL statement: first token, or right after something that ends
        // or opens a statement list.
        let at_stmt_start = match prev_sig {
            None => true,
            Some(j) => {
                tok_raw(j) == ";"
                    || matches!(tok_kind(j), Token::Then | Token::Else | Token::BeginP | Token::Declare)
                    || is_word(j, "loop")
                    || is_word(j, "exception")
            }
        };

        // Track CREATE FUNCTION header, RETURNS <type> boundary, and CTE blocks
        if kind == Token::Create {
            stmt_indent = indent;
            // Skip `or replace` / `unique` to find what's being created
            let mut j = i + 1;
            while j < tokens.len()
                && matches!(tok_kind(j), Token::Or | Token::Replace | Token::Unique)
            {
                j += 1;
            }
            in_create_index = j < tokens.len() && tok_kind(j) == Token::Index;
            in_create_function = true;
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
            if kind == Token::As {
                in_create_function = false;
            }
            push_newline(&mut out, indent);
        } else if kind == Token::As && in_create_function && paren_stack.is_empty() {
            in_create_function = false;
            in_create_function_header = false;
            in_returns_type = false;
            push_newline(&mut out, indent);
        } else if kind == Token::As && paren_stack.is_empty() {
            in_create_function_header = false;
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
        let top_block = block_stack.last().copied();
        let ends_block_kind = |k: BlockKind| match k {
            BlockKind::If => next_kind == Token::IfP,
            BlockKind::Loop => is_word(i + 1, "loop"),
            BlockKind::CaseStmt => next_kind == Token::Case,
            BlockKind::Begin => false,
        };
        // `select ... into x, y` / `returning ... into x` gets its own clause, unlike
        // `insert into` / `fetch c into` / `execute '...' into`.
        let is_clause_into = kind == Token::Into && {
            let mut depth = 0i32;
            let mut found = false;
            for j in (0..i).rev() {
                let r = tok_raw(j);
                if r == ")" {
                    depth += 1;
                } else if r == "(" {
                    depth -= 1;
                    if depth < 0 {
                        break;
                    }
                } else if depth == 0 {
                    let k = tok_kind(j);
                    if r == ";"
                        || matches!(k, Token::Then | Token::Else | Token::BeginP | Token::Declare)
                        || is_word(j, "loop")
                        || is_word(j, "exception")
                    {
                        break;
                    }
                    if matches!(k, Token::Select | Token::Returning) {
                        found = true;
                        break;
                    }
                }
            }
            found
        };
        // Row-locking clause: `for update` / `for no key update` / `for share` / `for key share`.
        // (`at_stmt_start` rules out a PL/pgSQL `for` loop with a variable named e.g. `no`.)
        let is_locking_clause = kind == Token::For
            && !at_stmt_start
            && (is_word(i + 1, "update")
                || is_word(i + 1, "share")
                || (is_word(i + 1, "no") && is_word(i + 2, "key"))
                || (is_word(i + 1, "key") && is_word(i + 2, "share")));
        let is_plain_begin = kind == Token::BeginP
            && !(i + 1 < tokens.len()
                && (tok_raw(i + 1) == ";" || matches!(next_kind, Token::Transaction | Token::Work)));

        if kind == Token::Declare {
            declare_indent = stmt_indent;
            push_newline(&mut out, declare_indent);
            stmt_indent = declare_indent + 2;
            in_declare_section = true;
            next_line_indent = None;
        } else if is_plain_begin {
            let block_indent = if in_declare_section { declare_indent } else { stmt_indent };
            push_newline(&mut out, block_indent);
            block_stack.push(Block { kind: BlockKind::Begin, indent: block_indent, flag: false, line_start: 0, first_tok: i });
            stmt_indent = block_indent + 2;
            in_declare_section = false;
            next_line_indent = None;
        } else if kind == Token::EndP {
            let closes_named = top_block.map(|b| b.kind != BlockKind::Begin && ends_block_kind(b.kind));
            if closes_named == Some(true) || (case_stack.is_empty() && top_block.map(|b| b.kind) == Some(BlockKind::Begin)) {
                let b = block_stack.pop().unwrap();
                stmt_indent = b.indent;
                push_newline(&mut out, stmt_indent);
            } else if let Some(case_col) = case_stack.pop() {
                push_newline(&mut out, case_col);
            } else {
                stmt_indent = indent;
                push_newline(&mut out, stmt_indent);
            }
            next_line_indent = None;
        } else if kind == Token::When && !case_stack.is_empty() {
            let case_col = *case_stack.last().unwrap();
            push_newline(&mut out, case_col + 2);
            next_line_indent = None;
        } else if kind == Token::When
            && !(prev_sig.is_some_and(|j| is_word(j, "exit") || is_word(j, "continue")))
            && top_block.is_some_and(|b| {
                b.kind == BlockKind::CaseStmt || (b.kind == BlockKind::Begin && b.flag)
            })
        {
            // `when` arm of a CASE statement or an exception handler
            push_newline(&mut out, top_block.unwrap().indent + 2);
            next_line_indent = None;
        } else if kind == Token::Ident
            && raw.eq_ignore_ascii_case("exception")
            && at_stmt_start
            && top_block.is_some_and(|b| b.kind == BlockKind::Begin)
        {
            let b = block_stack.last_mut().unwrap();
            b.flag = true;
            stmt_indent = b.indent + 2;
            push_newline(&mut out, b.indent);
            next_line_indent = None;
        } else if kind == Token::Ident
            && raw.eq_ignore_ascii_case("elsif")
            && top_block.is_some_and(|b| b.kind == BlockKind::If)
        {
            stmt_indent = top_block.unwrap().indent;
            push_newline(&mut out, stmt_indent);
            next_line_indent = None;
        } else if kind == Token::Else {
            if let Some(&case_col) = case_stack.last() {
                push_newline(&mut out, case_col + 2);
                next_line_indent = None;
            } else if let Some(b) = top_block {
                match b.kind {
                    BlockKind::If => push_newline(&mut out, b.indent),
                    BlockKind::CaseStmt => push_newline(&mut out, b.indent + 2),
                    _ => {}
                }
                next_line_indent = None;
            }
        } else if is_clause_into
            || is_locking_clause
            || matches!(
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
        if kind == Token::On && in_create_index && paren_stack.is_empty() {
            // create index <name>
            //   on <table> (<columns>)
            push_newline(&mut out, stmt_indent + 2);
            next_line_indent = None;
        } else if kind == Token::On && matches!(next_kind, Token::DeleteP | Token::Update) {
            // `references t(id) on delete set null` stays on one line
        } else if kind == Token::On {
            next_line_indent = Some(stmt_indent + 4);
        } else if raw != "(" {
            if let Some(cols) = next_line_indent.take() {
                push_newline(&mut out, cols);
            }
        }

        // PL/pgSQL block openers (`end if` / `end loop` / `end case` are handled above)
        let after_end = prev_kind == Token::EndP;
        let mut is_case_stmt = false;
        if at_stmt_start && paren_stack.is_empty() && !after_end {
            let header_loop = kind == Token::For
                || (kind == Token::Ident
                    && (raw.eq_ignore_ascii_case("while") || raw.eq_ignore_ascii_case("foreach")));
            let opener = if kind == Token::IfP {
                Some((BlockKind::If, false))
            } else if header_loop {
                Some((BlockKind::Loop, true))
            } else if kind == Token::Case {
                is_case_stmt = true;
                Some((BlockKind::CaseStmt, false))
            } else {
                None
            };
            if let Some((bk, flag)) = opener {
                block_stack.push(Block { kind: bk, indent: stmt_indent, flag, line_start: line_start(&out), first_tok: i });
            }
        }
        if kind == Token::Ident && raw.eq_ignore_ascii_case("loop") && !after_end {
            match block_stack.last_mut() {
                Some(b) if b.kind == BlockKind::Loop && b.flag => {
                    // End of a `for`/`while` header: keep it on one line with `loop`. If it
                    // has a comment or a multi-line string literal, joining it would change
                    // its meaning, so leave it wrapped and put `loop` on its own line.
                    b.flag = false;
                    let keep_wrapped = (b.first_tok..i).any(|j| {
                        matches!(tok_kind(j), Token::SqlComment | Token::CComment)
                            || tok_raw(j).contains('\n')
                    });
                    if !keep_wrapped {
                        join_header_lines(&mut out, b.line_start);
                    } else if line_start(&out) != b.line_start {
                        push_newline(&mut out, b.indent);
                    }
                    stmt_indent = b.indent + 2;
                    next_line_indent = Some(stmt_indent);
                }
                _ if at_stmt_start => {
                    block_stack.push(Block { kind: BlockKind::Loop, indent: stmt_indent, flag: false, line_start: 0, first_tok: i });
                    stmt_indent += 2;
                    next_line_indent = Some(stmt_indent);
                }
                _ => {}
            }
        }

        if kind == Token::Case && !after_end && !is_case_stmt {
            let current_line = out.rsplit('\n').next().unwrap_or("");
            let mut case_col = current_line.len();
            if let Some(last) = current_line.chars().last() {
                if !last.is_whitespace() && last != '(' && last != '[' {
                    case_col += 1; // account for the space inserted before `case`
                }
            }
            case_stack.push(case_col);
        }

        // Capture the exact indentation where `WITH` was placed
        let is_storage_params = kind == Token::With
            && paren_stack.is_empty()
            && i + 1 < tokens.len()
            && tok_raw(i + 1) == "(";
        if is_storage_params {
            // create table ... (...)
            // with(
            //   param = value
            // , ...
            // );
            push_newline(&mut out, stmt_indent);
            storage_params_next = true;
        } else if kind == Token::With && paren_stack.is_empty() {
            in_cte_list = true;
            let current_line = out.rsplit('\n').next().unwrap_or("");
            cte_indent = current_line.chars().take_while(|c| *c == ' ').count();
        }

        // 4. Post-keyword triggers to indent the following token
        if kind == Token::Select {
            if next_kind != Token::Distinct {
                next_line_indent = Some(stmt_indent + 2);
            }
        } else if kind == Token::Distinct {
            let in_inline_call = paren_stack
                .last()
                .map(|f| f.kind == ParenKind::InlineCall)
                .unwrap_or(false);
            if !in_inline_call {
                next_line_indent = Some(stmt_indent + 2);
            }
        } else if is_clause_into || matches!(kind, Token::From | Token::Where) {
            next_line_indent = Some(stmt_indent + 2);
        } else if kind == Token::By && matches!(prev_kind, Token::Order | Token::GroupP) {
            next_line_indent = Some(stmt_indent + 2);
        } else if matches!(kind, Token::Declare | Token::BeginP) {
            next_line_indent = Some(stmt_indent);
        } else if matches!(kind, Token::Then | Token::Else) && case_stack.is_empty() {
            // Body of an if/elsif/else, CASE statement arm, or exception handler
            let body = match top_block {
                Some(b) if b.kind == BlockKind::If => Some(b.indent + 2),
                Some(b) if b.kind == BlockKind::CaseStmt => Some(b.indent + 4),
                Some(b) if b.kind == BlockKind::Begin && b.flag && kind == Token::Then => {
                    Some(b.indent + 4)
                }
                None if kind == Token::Then => Some(stmt_indent + 2),
                _ => None,
            };
            if let Some(body) = body {
                stmt_indent = body;
                next_line_indent = Some(stmt_indent);
            }
        } else if kind == Token::Union {
            if next_kind != Token::All {
                next_line_indent = Some(stmt_indent);
            }
        } else if kind == Token::All && prev_kind == Token::Union {
            next_line_indent = Some(stmt_indent);
        }

        let in_window_spec = paren_stack
            .last()
            .map(|f| f.kind == ParenKind::WindowSpec)
            .unwrap_or(false);

        if in_window_spec {
            let win_frame = *paren_stack.last().unwrap();
            let win_clause_indent = win_frame.close_indent + 2;

            if matches!(kind, Token::Partition | Token::Order) {
                push_newline(&mut out, win_clause_indent);
                next_line_indent = None;
            } else if kind == Token::By && matches!(prev_kind, Token::Partition | Token::Order) {
                next_line_indent = Some(win_clause_indent + 2);
            } else if matches!(kind, Token::Rows | Token::Range | Token::Groups) {
                push_newline(&mut out, win_clause_indent);
                next_line_indent = Some(win_clause_indent + 2);
            }
        }

        match raw {
            "(" => {
                let is_window_spec = prev_kind == Token::Over;
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

                let pkind = if (in_create_function_header || prev_kind == Token::With && std::mem::take(&mut storage_params_next))
                    && paren_stack.is_empty()
                {
                    ParenKind::CreateFuncParams
                } else if is_subquery {
                    ParenKind::Subquery
                } else if is_window_spec {
                    ParenKind::WindowSpec
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
                } else if (in_create_index || prev_kind == Token::With) && paren_stack.is_empty() {
                    // `on tbl (col, ...)`, `include (col, ...)`, `with (param = ...)`
                    if out.chars().last().is_some_and(|c| !c.is_whitespace()) {
                        out.push(' ');
                    }
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
                    match paren_stack.last() {
                        Some(f) if f.kind == ParenKind::InlineCall => out.push_str(", "),
                        // Align with the line that opened the list, e.g. an index's
                        // `  on tbl (` line rather than the statement's start
                        Some(f) if f.kind == ParenKind::CreateFuncParams => {
                            push_newline(&mut out, f.close_indent);
                            out.push_str(", ");
                        }
                        _ => {
                            push_newline(&mut out, stmt_indent);
                            out.push_str(", ");
                        }
                    }
                }
            }
            ";" => {
                in_between = false;
                in_cte_list = false;
                in_create_index = false;
                if paren_stack.is_empty() {
                    // Don't leak `create ...` state into the next statement
                    in_create_function = false;
                    in_create_function_header = false;
                    in_returns_type = false;
                }
                out.push(';');

                let next_closes_block = matches!(next_kind, Token::EndP | Token::Else)
                    || (next_kind == Token::When && case_stack.is_empty())
                    || is_word(i + 1, "elsif")
                    || is_word(i + 1, "exception");

                // Blank line between statements only at the top level / directly in the outermost `begin`
                let top_level = match block_stack.as_slice() {
                    [] => true,
                    [b] => b.kind == BlockKind::Begin && !b.flag,
                    _ => false,
                };

                if top_level && !next_closes_block && !in_declare_section {
                    out.push_str("\n\n");
                    out.push_str(&" ".repeat(stmt_indent));
                } else {
                    push_newline(&mut out, stmt_indent);
                }
            }
            _ => match kind {
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
                        lower == "." || lower == ".." || lower == "%" || kind == Token::Typecast;
                    let after_tight_token = prev_raw == "("
                        || prev_raw == "["
                        || prev_raw == "."
                        || prev_raw == ".."
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

    if let Some(pos) = place_comments_at {
        insert_comments(&mut out, pos, &pending_comments);
    } else if !pending_comments.is_empty() {
        // Comments after the last token
        if !line_is_blank(&out) {
            push_newline(&mut out, stmt_indent);
        }
        let pad = " ".repeat(stmt_indent);
        out.truncate(line_start(&out));
        for c in &pending_comments {
            out.push_str(&pad);
            out.push_str(c);
            out.push('\n');
        }
    }

    Ok(out.trim_end().to_string() + "\n")
}

fn line_is_blank(out: &str) -> bool {
    out[line_start(out)..].trim().is_empty()
}

/// Puts `comments` on their own lines just above the line holding the first token written
/// at or after `pos`, indented to match that line's content (past a leading `, `).
/// Returns where the text was inserted and its length.
fn insert_comments(out: &mut String, pos: usize, comments: &[&str]) -> (usize, usize) {
    let pos = pos.min(out.len());
    let tok_at = out[pos..]
        .find(|c: char| !c.is_whitespace())
        .map_or(out.len(), |k| pos + k);
    let ls = line_start(&out[..tok_at]);
    let line = &out[ls..];
    let lead = line.len() - line.trim_start_matches(' ').len();
    let indent = lead + if line[lead..].starts_with(", ") { 2 } else { 0 };
    let pad = " ".repeat(indent);
    let text: String = comments.iter().map(|c| format!("{pad}{c}\n")).collect();
    out.insert_str(ls, &text);
    (ls, text.len())
}

/// Whether `input` is valid as PL/pgSQL statements (e.g. a selection from inside a
/// function body) rather than a complete SQL statement. It's checked by wrapping it in
/// a function. `return x` / `return;` / `return next` are only valid for some return
/// types, so a few are tried.
///
/// PL/pgSQL checks `into` targets when parsing, so a selection that doesn't include its
/// `declare` section is also tried with those targets declared. The wrapper is only used
/// for this check; it never reaches the formatted output.
fn is_plpgsql_fragment(input: &str) -> bool {
    let tag = (0..)
        .map(|n| format!("$sqlfmt{n}$"))
        .find(|t| !input.contains(t.as_str()))
        .unwrap();
    let Ok(scan) = pg_query::scan(input) else {
        return false;
    };
    let tokens = scan.tokens;
    let kind = |j: usize| Token::try_from(tokens[j].token).unwrap_or(Token::Nul);
    let raw = |j: usize| &input[tokens[j].start as usize..tokens[j].end as usize];
    let starts_with_declare = tokens.first().map(|t| t.token) == Some(Token::Declare as i32);

    // (name, used with a field)
    let mut targets: Vec<(&str, bool)> = Vec::new();
    for i in 0..tokens.len() {
        if kind(i) != Token::Into || (i > 0 && matches!(kind(i - 1), Token::Insert | Token::Merge)) {
            continue;
        }
        let mut j = i + 1;
        if j < tokens.len() && kind(j) == Token::StrictP {
            j += 1;
        }
        // `into a, b` / `into rec.field`: declare `a` and `b` as scalars, `rec` as a record
        // (a record can't be one of several targets, and a scalar can't have fields).
        while j < tokens.len() && kind(j) == Token::Ident {
            let dotted = j + 1 < tokens.len() && raw(j + 1) == ".";
            match targets.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case(raw(j))) {
                Some(t) => t.1 |= dotted,
                None => targets.push((raw(j), dotted)),
            }
            j += 1;
            while j + 1 < tokens.len() && raw(j) == "." {
                j += 2;
            }
            if j < tokens.len() && raw(j) == "," {
                j += 1;
            } else {
                break;
            }
        }
    }

    let decls: String = targets
        .iter()
        .map(|(name, dotted)| format!("{name} {};\n", if *dotted { "record" } else { "text" }))
        .collect();

    let mut bodies = Vec::new();
    // A fragment may bring its own `declare ... begin ... end`; otherwise wrap in one.
    if starts_with_declare {
        bodies.push(input.to_string());
        if !decls.is_empty() {
            // Declared in an outer block so the fragment's own declarations can shadow them
            let block = input.trim_end().trim_end_matches(';');
            bodies.push(format!("declare\n{decls}begin\n{block};\nend"));
        }
    } else {
        bodies.push(format!("begin\n{input}\nend"));
        if !decls.is_empty() {
            bodies.push(format!("declare\n{decls}begin\n{input}\nend"));
        }
    }

    bodies.iter().any(|body| {
        ["int", "void", "setof record"].iter().any(|ret| {
            let wrapped = format!(
                "create function pg_temp.sqlfmt_check() returns {ret} language plpgsql as {tag}\n{body}\n{tag};"
            );
            pg_query::parse_plpgsql(&wrapped).is_ok()
        })
    })
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
        if !is_plpgsql_fragment(&input) {
            eprintln!("PostgreSQL syntax error: {e}");
            std::process::exit(1);
        }
    }

    // Keep the first line's indentation, so a selection from inside a function body
    // stays where it was.
    let base_indent = input
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start_matches(' ').len())
        .unwrap_or(0);

    let formatted = format_sql(&input, base_indent).expect("Failed to format SQL");

    if in_place {
        let path = file_arg.expect("-i requires a file path");
        fs::write(path, formatted).expect("Failed to write file");
    } else {
        print!("{}", formatted);
    }
}
