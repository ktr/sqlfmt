# Overview

A (very opinionated) sql formatting tool.

## Run

```bash
cat filename.sql | sqlfmt

# or from clipboard
pbpaste | sqlfmt

# or in PowerShell
gcb | sqlfmt
```

## Neovim Shortcut

```lua
local function format_sql()
  local bufnr = vim.api.nvim_get_current_buf()
  local mode = vim.fn.mode()

  local start_line, end_line
  if mode:match('[vV\22]') then -- visual char, line, or block (<C-v>)
    local v_line = vim.fn.line('v')
    local cur_line = vim.fn.line('.')
    -- nvim_buf_* uses 0-based start and end-exclusive end
    start_line = math.min(v_line, cur_line) - 1
    end_line = math.max(v_line, cur_line)

    -- Exit visual mode cleanly so selection highlight doesn't linger
    local esc = vim.api.nvim_replace_termcodes('<Esc>', true, false, true)
    vim.api.nvim_feedkeys(esc, 'nx', false)
  else
    start_line = 0
    end_line = -1
  end

  local lines = vim.api.nvim_buf_get_lines(bufnr, start_line, end_line, false)
  local input = table.concat(lines, '\n')

  vim.system({ 'sqlfmt' }, { stdin = input, text = true }, function(res)
    vim.schedule(function()
      if not vim.api.nvim_buf_is_valid(bufnr) then
        return
      end

      if res.code ~= 0 then
        vim.notify('SQL formatter failed: ' .. (res.stderr or ''), vim.log.levels.ERROR)
        return
      end

      if not res.stdout or res.stdout == '' then
        return
      end

      local formatted = vim.split(res.stdout, '\n', { plain = true })
      if formatted[#formatted] == '' then
        table.remove(formatted, #formatted)
      end

      vim.api.nvim_buf_set_lines(bufnr, start_line, end_line, false, formatted)
    end)
  end)
end

vim.keymap.set({ 'n', 'x' }, '<F7>', format_sql, { desc = 'Format buffer or selection with SQL tool' })
```
