local M = {}
local user_cursors = {}
local cursor_namespace = vim.api.nvim_create_namespace("Ethersync")
local offset_encoding = "utf-32"

-- A new set of ranges means, we delete all existing ones for that user.
function M.setCursor(bufnr, user_id, ranges)
    if user_cursors[user_id] ~= nil then
        for _, cursor_buffer_tuple in ipairs(user_cursors[user_id]) do
            local old_cursor_id = cursor_buffer_tuple.cursor_id
            local old_bufnr = cursor_buffer_tuple.bufnr
            vim.api.nvim_buf_del_extmark(old_bufnr, cursor_namespace, old_cursor_id)
        end
    end
    user_cursors[user_id] = {}

    if not vim.api.nvim_buf_is_loaded(bufnr) then
        return
    end

    for _, range in ipairs(ranges) do
        -- Convert from LSP style ranges to Neovim style ranges.
        local e = {
            start_row = range.start.line,
            start_col = vim.lsp.util._get_line_byte_from_position(bufnr, range.start, offset_encoding),
            end_row = range["end"].line,
            end_col = vim.lsp.util._get_line_byte_from_position(bufnr, range["end"], offset_encoding),
        }

        -- TODO: Implement those two things?
        -- if head == anchor then
        --     anchor = head + 1
        -- end
        -- if head > anchor then
        --     head, anchor = anchor, head
        -- end

        -- TODO:
        -- -- If the cursor is at the end of the buffer, don't show it.
        -- -- This is because otherwise, the calculation that follows (to find the location for head+1) would fail.
        -- -- TODO: Find a way to display the cursor nevertheless.
        -- if head == utils.contentOfCurrentBuffer() then
        --     return
        -- end

        -- TODO:
        -- How can we display something at the end of lines?
        -- Virtual text, like the Copilot plugin?

        local cursor_id = vim.api.nvim_buf_set_extmark(bufnr, cursor_namespace, e.start_row, e.start_col, {
            hl_mode = "combine",
            hl_group = "TermCursor",
            end_col = e.end_col,
            end_row = e.end_row,
        })
        table.insert(user_cursors[user_id], { cursor_id = cursor_id, bufnr = bufnr })
    end
end

function M.trackCursor(bufnr, callback)
    vim.api.nvim_create_autocmd({ "CursorMoved", "CursorMovedI" }, {
        buffer = bufnr,
        callback = function()
            local row, col = unpack(vim.api.nvim_win_get_cursor(0))
            local line = row - 1
            local character = vim.lsp.util.character_offset(0, line, col, offset_encoding)
            local ranges = {
                { anchor = { line = line, character = character }, head = { line = line, character = character + 1 } },
            }
            callback(ranges)
        end,
    })
end

return M
