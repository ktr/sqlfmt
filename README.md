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
vim.keymap.set('n', '<F7>', function()
  local bufnr = vim.api.nvim_get_current_buf()
  local lines = vim.api.nvim_buf_get_lines(bufnr, 0, -1, false)
  local input = table.concat(lines, '\n')

  vim.system({ 'sqlfmt' }, { stdin = input, text = true }, function(res)
    vim.schedule(function()
      if res.code ~= 0 then
        vim.notify('sqlfmt failed: ' .. (res.stderr or ''), vim.log.levels.ERROR)
        return
      end

      if not res.stdout or res.stdout == '' then
        return
      end

      local formatted = vim.split(res.stdout, '\n', { plain = true })
      -- vim.split produces a trailing empty string if stdout ends with '\n'
      if formatted[#formatted] == '' then
        table.remove(formatted, #formatted)
      end

      vim.api.nvim_buf_set_lines(bufnr, 0, -1, false, formatted)
    end)
  end)
end, { desc = 'Format buffer with sqlfmt tool' })
```
