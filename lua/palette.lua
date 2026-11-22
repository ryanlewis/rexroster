-- rexroster's command-palette actions. Load this file from Rex's init.lua
-- (see the README), then run `rex config reload`.
--
--   Roster: Rename session…  rename the session through the lead's pin, so
--                            the roster keeps the new name while workers run.
--   Roster: Show worker…     float one of the session's workers beside its
--                            lead, as Enter does in the roster.
--
-- Each takes its answer as an argument and asks only when it is missing, so
-- they run without a person too:
--
--   rex do roster_rename_session name=my-session
--   rex do roster_show_worker block=fix-login   (a block id or label)
--
-- `rexroster lua` puts a line in front of this that sets REXROSTER to its
-- own path. Without it, rexroster is looked up on PATH.

local REXROSTER = REXROSTER or "rexroster"

local TERMINAL = "com.superlogical.terminal"

-- Where the roster shows a worker: src/tui.rs SHOWN_BOUNDS.
local SHOWN_BOUNDS = { 0.1, 0.08, 0.8, 0.84 }

-- The prompt's answer, or nil. Cancelling is silent; any other reason is
-- logged.
local function answered(what, value, why, detail)
  if value ~= nil then return value end
  if why ~= "cancelled" then
    rex.log("warn", what .. ": " .. tostring(why) .. " " .. (detail or ""))
  end
end

-- The session's workers, as the roster reads them: blocks whose `meta`
-- record comes from rexlead and names a lead block in this session. Each is
-- { id, label, placed, lead, task, state }, with the state the roster shows.
local function workers()
  local list, err = rex.session.list_blocks{}
  if not list then error("list_blocks: " .. tostring(err), 0) end
  local labels, found = {}, {}
  for _, b in ipairs(list.blocks or {}) do
    labels[b.block_id] = b.label or b.block_id
  end
  for _, b in ipairs(list.blocks or {}) do
    local status = b.creator_name == TERMINAL
      and rex.block.call(TERMINAL, "program_status", { block_id = b.block_id })
    local records = {}
    for _, r in ipairs(type(status) == "table" and status.records or {}) do
      records[r.id or ""] = r
    end
    local m = records.meta
    local meta = m and m.app == "rexlead" and type(m.msg) == "string" and rex.json.decode(m.msg)
    if type(meta) == "table" and labels[meta.lead] and meta.lead ~= b.block_id then
      -- The root record's state; while that is idle or done, blocked or
      -- working if a subagent (same app, another id) is. src/model.rs.
      local root = records[""]
      local state = root and root.state or "starting"
      if state == "idle" or state == "done" then
        local sub = {}
        for id, r in pairs(records) do
          if id ~= "" and id ~= "meta" and r.app == root.app then sub[r.state] = true end
        end
        state = (sub.blocked and "blocked") or (sub.working and "working") or state
      end
      found[#found + 1] = {
        id = b.block_id,
        label = labels[b.block_id],
        placed = b.placement ~= "detached",
        lead = meta.lead,
        task = meta.task,
        state = state,
      }
    end
  end
  return found
end

-- The Rex server's address, as Rex sets REX_SERVER in its terminals. The pin
-- file's name includes it, so `rexroster pin` must see the same one.
local function server()
  local env = os.getenv("REX_SERVER")
  if env and env ~= "" then return env end
  local status = rex.call("server.status", {})
  for _, l in ipairs(status and status.listeners or {}) do
    if l.primary then return l.endpoint end
  end
end

local function pin(ctx, lead, name)
  local r, err = rex.exec({ REXROSTER, "pin", lead, name }, {
    timeout = 20,
    env = { REX_SESSION = ctx.session_id, REX_BLOCK = false, REX_SERVER = server() or false },
  })
  if not r then error("rexroster pin: " .. tostring(err), 0) end
  if not r.ok then error("rexroster pin: " .. r.stderr, 0) end
  if r.stderr ~= "" then rex.log("warn", r.stderr) end
end

rex.action{
  name = "roster_rename_session",
  title = "Roster: Rename session…",
  description = "Rename the session so the roster keeps the name while workers run",
  args = { name = "string?" },
  run = function(ctx, args)
    local name = args.name
    if name == nil then
      local view = rex.session.view{}
      name = answered("rename session", rex.ui.input{
        title = "Rename session",
        value = view and view.label or nil,
        confirm = "Rename",
      })
    end
    name = name and name:match("^%s*(.-)%s*$")
    if not name or name == "" then return end
    -- Every lead in the session with live workers holds a pin, and each
    -- one's roster reverts to its own, so all of them take the new name.
    local seen, leads = {}, {}
    for _, w in ipairs(workers()) do
      if not seen[w.lead] then
        seen[w.lead] = true
        leads[#leads + 1] = w.lead
      end
    end
    for _, lead in ipairs(leads) do pin(ctx, lead, name) end
    if #leads == 0 then
      local _, err = rex.session.set_label{ label = name }
      if err then error("set_label: " .. tostring(err), 0) end
    end
    return { label = name, leads = leads }
  end,
}

rex.action{
  name = "roster_show_worker",
  title = "Roster: Show worker…",
  description = "Float one of the session's workers beside its lead",
  args = { block = "string?" },
  run = function(ctx, args)
    local all = workers()
    local id = args.block
    if id == nil then
      if #all == 0 then return rex.log("info", "show worker: no workers in this session") end
      local items = {}
      for _, w in ipairs(all) do
        items[#items + 1] = {
          id = w.id,
          title = w.label,
          subtitle = w.task,
          detail = w.placed and (w.state .. " · shown") or w.state,
          icon = "terminal",
        }
      end
      id = answered("show worker", rex.ui.pick{ title = "Show worker", items = items })
      if not id then return end
    end
    local w
    for _, c in ipairs(all) do
      if c.id == id or c.label == id then w = c break end
    end
    if not w then error("no worker " .. id .. " in this session", 0) end
    -- A shown worker is in view already; leave it where it is.
    if w.placed then return { block = w.id, shown = false } end
    local _, err = rex.session.move_block{ block_id = w.id, anchor_block_id = w.lead }
    if err then error("move_block: " .. tostring(err), 0) end
    _, err = rex.session.float_block{ block_id = w.id, bounds = SHOWN_BOUNDS }
    if err then error("float_block: " .. tostring(err), 0) end
    rex.session.focus_block{ block_id = w.id }
    return { block = w.id, shown = true }
  end,
}
