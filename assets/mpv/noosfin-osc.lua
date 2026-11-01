-- Noosfin : overlay de lecture pour mpv (remplace l'OSC intégré).
--
-- Commandes (manette relayée par Noosfin via « script-message noosfin-pad <bouton> », ou clavier) :
--   A / Entrée      : afficher, puis activer le bouton sélectionné (pause sur la barre)
--   B / Échap       : masquer l'overlay / fermer un menu
--   ← →             : déplacer la sélection (sur la barre : reculer / avancer de 10 s)
--   ↑ ↓             : barre de progression / rangée de boutons
--   LB / RB         : −30 s / +30 s
--   X / Y           : pistes audio / sous-titres
--   Start / m       : menu « Image » (filtres et agrandissement IA selon le GPU)
--   Select / i      : informations de lecture

local mp = require "mp"
local utils = require "mp.utils"

-- Fournis par Noosfin : puissance du GPU (low / mid / high), son nom, dossier des shaders.
local opts = { gpu = "low", gpuname = "GPU", shaders = "" }
require("mp.options").read_options(opts, "noosfin")

local W, H = 1920, 1080
local ov = mp.create_osd_overlay("ass-events")
ov.res_x, ov.res_y = W, H

-- Palette Catppuccin Mocha (ASS : &HBBGGRR&)
local C = {
    base = "1B1111", surface = "443231", surface1 = "5A4745", text = "F4D6CD",
    sub = "C8ADA6", mauve = "F7A6CB", blue = "FAB489", green = "A1E3A6", yellow = "AFE2F9", pink = "A88BF3",
}

local state = {
    visible = false,
    area = "buttons", -- "bar" | "buttons"
    focus = 3,
    menu = nil, -- { kind, title, items = { {label, action} }, idx }
    info = false,
    last_input = 0,
}

local function is_paused() return mp.get_property_native("pause") == true end

local buttons = {
    { id = "back30", label = function() return "−30 s" end, run = function() mp.commandv("seek", -30, "relative") end },
    { id = "back10", label = function() return "−10 s" end, run = function() mp.commandv("seek", -10, "relative") end },
    { id = "pause", label = function() return is_paused() and "▶  Lecture" or "❚❚  Pause" end, run = function() mp.commandv("cycle", "pause") end },
    { id = "fwd10", label = function() return "+10 s" end, run = function() mp.commandv("seek", 10, "relative") end },
    { id = "fwd30", label = function() return "+30 s" end, run = function() mp.commandv("seek", 30, "relative") end },
    { id = "skip", label = function() return "Passer l'opening" end, run = function() mp.commandv("seek", 85, "relative") end },
    { id = "audio", label = function() return "Audio" end, run = function() open_tracks("audio") end },
    { id = "sub", label = function() return "Sous-titres" end, run = function() open_tracks("sub") end },
    { id = "image", label = function() return "Image" end, run = function() open_image() end },
    { id = "info", label = function() return "Infos" end, run = function() state.info = not state.info end },
    { id = "quit", label = function() return "Quitter" end, run = function() mp.command("quit") end },
}

-- ─────────────────────────────── Dessin ───────────────────────────────

local function esc(s)
    s = tostring(s or "")
    return (s:gsub("\\", "\\\\"):gsub("{", "\\{"):gsub("}", "\\}"):gsub("\n", "\\N"))
end

local function rrect(x, y, w, h, r)
    r = math.min(r, h / 2, w / 2)
    return string.format(
        "m %d %d l %d %d b %d %d %d %d %d %d l %d %d b %d %d %d %d %d %d l %d %d b %d %d %d %d %d %d l %d %d b %d %d %d %d %d %d",
        x + r, y, x + w - r, y, x + w, y, x + w, y, x + w, y + r,
        x + w, y + h - r, x + w, y + h, x + w, y + h, x + w - r, y + h,
        x + r, y + h, x, y + h, x, y + h, x, y + h - r,
        x, y + r, x, y, x, y, x + r, y)
end

local function shape(path, color, alpha)
    return string.format("{\\an7\\pos(0,0)\\bord0\\shad0\\1c&H%s&\\1a&H%s&\\p1}%s{\\p0}", color, alpha or "00", path)
end

local function text(x, y, an, size, color, s, bold, alpha)
    return string.format("{\\an%d\\pos(%d,%d)\\fs%d\\bord0\\shad0\\1c&H%s&\\1a&H%s&\\b%d\\fnNoto Sans}%s",
        an, x, y, size, color, alpha or "00", bold and 1 or 0, esc(s))
end

local function fmt_time(t)
    if not t or t < 0 then return "--:--" end
    t = math.floor(t)
    local h, m, s = math.floor(t / 3600), math.floor(t % 3600 / 60), t % 60
    if h > 0 then return string.format("%d:%02d:%02d", h, m, s) end
    return string.format("%d:%02d", m, s)
end

local function text_width(s, size)
    -- Estimation suffisante pour des pastilles centrées.
    local n = 0
    for _ in tostring(s):gmatch("[%z\1-\127\194-\244][\128-\191]*") do n = n + 1 end
    return n * size * 0.56
end

local function title_parts()
    local t = mp.get_property("media-title") or ""
    local main, rest = t:match("^(.-) — (.+)$")
    if main then return main, rest end
    return t, nil
end

local function draw_bar(out)
    -- Dégradé bas
    for i = 0, 11 do
        local y = 640 + i * 37
        local a = string.format("%02X", math.max(0x30, 0xF0 - i * 0x12))
        out[#out + 1] = shape(string.format("m 0 %d l %d %d l %d %d l 0 %d", y, W, y, W, y + 37, y + 37), C.base, a)
    end
    local main, rest = title_parts()
    out[#out + 1] = text(80, 752, 1, 46, C.text, main, true)
    if rest then out[#out + 1] = text(80, 800, 1, 28, C.mauve, rest, false) end

    -- Heure et heure de fin
    local pos = mp.get_property_number("time-pos")
    local dur = mp.get_property_number("duration")
    local clock = os.date("%H:%M")
    if pos and dur and dur > 0 then
        local speed = mp.get_property_number("speed") or 1
        clock = clock .. "   ·   fin à " .. os.date("%H:%M", os.time() + math.floor((dur - pos) / speed))
    end
    out[#out + 1] = text(W - 80, 752, 3, 28, C.sub, clock, false)

    -- Progression
    local bx, by, bw, bh = 80, 840, W - 160, 10
    local bar_focus = state.area == "bar" and not state.menu
    if dur and dur > 0 and pos then
        local f = math.max(0, math.min(1, pos / dur))
        out[#out + 1] = shape(rrect(bx, by, bw, bh, 5), C.surface1, "30")
        local cache = mp.get_property_number("demuxer-cache-time")
        if cache and cache > pos then
            local cf = math.min(1, cache / dur)
            out[#out + 1] = shape(rrect(bx, by, math.max(bh, bw * cf), bh, 5), C.sub, "B0")
        end
        out[#out + 1] = shape(rrect(bx, by, math.max(bh, bw * f), bh, 5), bar_focus and C.text or C.mauve)
        local kx = bx + bw * f
        local kr = bar_focus and 14 or 9
        out[#out + 1] = shape(rrect(kx - kr, by + bh / 2 - kr, kr * 2, kr * 2, kr), C.text)
        out[#out + 1] = text(bx, by + 46, 1, 26, C.text, fmt_time(pos), false)
        out[#out + 1] = text(bx + bw, by + 46, 3, 26, C.sub, "−" .. fmt_time(dur - pos), false)
    else
        out[#out + 1] = shape(rrect(bx, by - 8, 150, 36, 18), C.pink)
        out[#out + 1] = text(bx + 75, by + 10, 5, 22, C.base, "EN DIRECT", true)
    end

    -- Boutons
    local sizes, total = {}, 0
    for i, b in ipairs(buttons) do
        sizes[i] = text_width(b.label(), 24) + 56
        total = total + sizes[i] + (i > 1 and 16 or 0)
    end
    local x = math.floor((W - total) / 2)
    local y = 945
    for i, b in ipairs(buttons) do
        local focused = state.area == "buttons" and state.focus == i and not state.menu
        out[#out + 1] = shape(rrect(x, y, sizes[i], 60, 30), focused and C.text or C.surface, focused and "00" or "30")
        out[#out + 1] = text(x + sizes[i] / 2, y + 30, 5, 24, focused and C.base or C.text, b.label(), true)
        x = x + sizes[i] + 16
    end
end

local function track_label(t)
    local parts = {}
    if t.lang then parts[#parts + 1] = t.lang:upper() end
    if t.title then parts[#parts + 1] = t.title end
    if t.codec then parts[#parts + 1] = t.codec:upper() end
    if t["demux-channel-count"] then parts[#parts + 1] = t["demux-channel-count"] .. " canaux" end
    if t.forced then parts[#parts + 1] = "forcés" end
    if t.default then parts[#parts + 1] = "par défaut" end
    if t.external then parts[#parts + 1] = "externe" end
    local s = table.concat(parts, "  ·  ")
    return s ~= "" and s or ("Piste " .. t.id)
end

function open_tracks(kind)
    local items = {}
    local current = mp.get_property(kind == "audio" and "aid" or "sid")
    if kind == "sub" then
        items[#items + 1] = { label = "Désactivés", selected = (current == "no"), run = function() mp.set_property("sid", "no") end }
    end
    for _, t in ipairs(mp.get_property_native("track-list") or {}) do
        if t.type == kind then
            local id = t.id
            items[#items + 1] = {
                label = track_label(t),
                selected = t.selected,
                run = function() mp.set_property_number(kind == "audio" and "aid" or "sid", id) end,
            }
        end
    end
    if #items == 0 then
        mp.osd_message(kind == "audio" and "Aucune piste audio" or "Aucun sous-titre", 2)
        return
    end
    local idx = 1
    for i, it in ipairs(items) do if it.selected then idx = i end end
    state.menu = { title = kind == "audio" and "Pistes audio" or "Sous-titres", items = items, idx = idx }
    state.visible = true
end

-- ─────────────────────────────── Filtres d'image ───────────────────────────────

local TIERS = { low = 1, mid = 2, high = 3 }
local A4K = "Anime4K_"
local presets = {
    { id = "none", label = "Aucun (rendu mpv standard)", tier = 1, files = {} },
    { id = "fsr", label = "AMD FSR 1 · agrandissement léger", tier = 1, files = { "FSR.glsl" } },
    { id = "nis", label = "NVIDIA Image Scaling · agrandissement léger", tier = 1, files = { "NVScaler.glsl" } },
    { id = "sharp", label = "Netteté seule (NVIDIA Sharpen)", tier = 1, files = { "NVSharpen.glsl" } },
    { id = "a4k-fast", label = "Anime4K rapide · animes, IA", tier = 1, files = {
        A4K .. "Clamp_Highlights.glsl", A4K .. "Restore_CNN_M.glsl", A4K .. "Upscale_CNN_x2_M.glsl",
        A4K .. "AutoDownscalePre_x2.glsl", A4K .. "AutoDownscalePre_x4.glsl", A4K .. "Upscale_CNN_x2_S.glsl" } },
    { id = "artcnn", label = "ArtCNN rapide · films et séries, IA", tier = 2, files = { "ArtCNN_C4F16.glsl" } },
    { id = "a4k-hq", label = "Anime4K qualité · animes, IA", tier = 3, files = {
        A4K .. "Clamp_Highlights.glsl", A4K .. "Restore_CNN_VL.glsl", A4K .. "Upscale_CNN_x2_VL.glsl",
        A4K .. "AutoDownscalePre_x2.glsl", A4K .. "AutoDownscalePre_x4.glsl", A4K .. "Upscale_CNN_x2_M.glsl" } },
    { id = "artcnn-hq", label = "ArtCNN qualité · films et séries, IA", tier = 3, files = { "ArtCNN_C4F32.glsl" } },
}
local RECOMMENDED = { low = "fsr", mid = "artcnn", high = "artcnn-hq" }
local current_preset = "none"

-- Choix mémorisé d'une lecture à l'autre.
local function choice_file() return opts.shaders ~= "" and (opts.shaders .. "/../mpv/image-preset") or nil end

local function apply_preset(p, quiet)
    local paths = {}
    for _, f in ipairs(p.files) do paths[#paths + 1] = opts.shaders .. "/" .. f end
    if #paths == 0 then
        mp.commandv("change-list", "glsl-shaders", "clr", "")
    else
        mp.commandv("change-list", "glsl-shaders", "set", table.concat(paths, ":"))
    end
    current_preset = p.id
    local f = choice_file() and io.open(choice_file(), "w")
    if f then f:write(p.id); f:close() end
    if not quiet then mp.osd_message("Image : " .. p.label, 2) end
end

local function allowed(p) return p.tier <= (TIERS[opts.gpu] or 1) end

function open_image()
    local items = {}
    for _, p in ipairs(presets) do
        if allowed(p) then
            local rec = RECOMMENDED[opts.gpu] == p.id and "  ★ recommandé" or ""
            items[#items + 1] = { label = p.label .. rec, selected = p.id == current_preset, run = function() apply_preset(p) end }
        end
    end
    local idx = 1
    for i, it in ipairs(items) do if it.selected then idx = i end end
    state.menu = { title = "Image · " .. opts.gpuname, items = items, idx = idx }
    state.visible = true
end

local function restore_preset()
    local f = choice_file() and io.open(choice_file(), "r")
    if not f then return end
    local id = f:read("*l")
    f:close()
    for _, p in ipairs(presets) do
        if p.id == id and allowed(p) and p.id ~= "none" then apply_preset(p, true) end
    end
end

local function draw_menu(out)
    local m = state.menu
    local rows = math.min(#m.items, 9)
    local first = math.max(1, math.min(m.idx - 4, #m.items - rows + 1))
    local w, rh = 900, 64
    local h = 110 + rows * rh + 20
    local x, y = (W - w) / 2, (H - h) / 2 - 80
    out[#out + 1] = shape(rrect(x, y, w, h, 28), C.base, "10")
    out[#out + 1] = text(x + 40, y + 60, 1, 34, C.mauve, m.title, true)
    for i = first, first + rows - 1 do
        local it = m.items[i]
        local ry = y + 100 + (i - first) * rh
        local f = i == m.idx
        if f then out[#out + 1] = shape(rrect(x + 20, ry, w - 40, rh - 8, 16), C.text) end
        local col = f and C.base or C.text
        out[#out + 1] = text(x + 50, ry + (rh - 8) / 2, 4, 26, col, (it.selected and "●  " or "    ") .. it.label, it.selected)
    end
end

local function prop(name)
    local v = mp.get_property(name)
    if v == nil or v == "" then return nil end
    return v
end

local function info_lines()
    local L = {}
    local vp = mp.get_property_native("video-params") or {}
    local tp = mp.get_property_native("video-target-params") or {}
    local function add(k, v) if v and v ~= "" then L[#L + 1] = { k, v } end end

    local codec = prop("video-format") and prop("video-format"):upper() or prop("video-codec")
    local fps = mp.get_property_number("container-fps") or mp.get_property_number("estimated-vf-fps")
    add("Vidéo", (codec or "?") .. (vp.w and string.format("  ·  %d×%d", vp.w, vp.h) or "") .. (fps and string.format("  ·  %.3g i/s", fps) or ""))
    local range = "SDR"
    if vp.gamma == "pq" then range = "HDR10 (PQ)" elseif vp.gamma == "hlg" then range = "HLG" end
    if vp["light"] == "scene-referred" and vp.gamma == "pq" then range = range end
    local dovi = mp.get_property_native("current-tracks/video/dolby-vision-profile")
    if dovi then range = "Dolby Vision (profil " .. dovi .. ")" end
    add("Plage dynamique", range .. (vp.primaries and ("  ·  " .. vp.primaries) or "") .. (vp["max-luma"] and string.format("  ·  %d nits", vp["max-luma"]) or ""))
    add("Sortie écran", (tp.gamma == "pq" and "HDR (PQ) transmis à KWin" or (tp.gamma or "?")) .. (tp.primaries and ("  ·  " .. tp.primaries) or ""))
    local hw = prop("hwdec-current")
    add("Décodage", (hw and hw ~= "no") and ("matériel (" .. hw .. ")") or "logiciel")
    add("Rendu", (prop("current-vo") or "?") .. "  ·  " .. (prop("gpu-api") or prop("options/gpu-api") or "?"))
    local ap = mp.get_property_native("audio-params") or {}
    add("Audio", (prop("audio-codec-name") or "?") .. (ap["hr-channels"] and ("  ·  " .. ap["hr-channels"]) or "") .. (ap.samplerate and string.format("  ·  %d Hz", ap.samplerate) or "") .. (prop("current-tracks/audio/lang") and ("  ·  " .. prop("current-tracks/audio/lang"):upper()) or ""))
    local ao = mp.get_property_native("audio-out-params") or {}
    if ao.format then
        local out = ao.format:match("^spdif") and ("bitstream " .. ao.format:gsub("^spdif%-", ""):upper() .. " vers l'ampli")
            or string.format("PCM %s  ·  %d Hz", ao.format, ao.samplerate or 0)
        add("Sortie audio", out .. ((ao.samplerate and ap.samplerate and ao.samplerate ~= ap.samplerate and not ao.format:match("^spdif")) and "  ·  rééchantillonné" or ""))
    end
    for _, p in ipairs(presets) do
        if p.id == current_preset and p.id ~= "none" then add("Image", p.label) end
    end
    add("Sous-titres", prop("current-tracks/sub/lang") and prop("current-tracks/sub/lang"):upper() or "aucun")
    local vb, ab = mp.get_property_number("video-bitrate"), mp.get_property_number("audio-bitrate")
    if vb or ab then add("Débit", string.format("%.1f Mb/s", ((vb or 0) + (ab or 0)) / 1e6)) end
    local cache, speed = mp.get_property_number("demuxer-cache-duration"), mp.get_property_number("cache-speed")
    if cache then add("Cache", string.format("%.0f s", cache) .. (speed and string.format("  ·  %.1f Mo/s", speed / 1e6) or "")) end
    local drop = (mp.get_property_number("frame-drop-count") or 0) + (mp.get_property_number("decoder-frame-drop-count") or 0)
    add("Images perdues", tostring(drop))
    local av = mp.get_property_number("avsync")
    if av then add("Synchro A/V", string.format("%+.3f s", av)) end
    local size = mp.get_property_number("file-size")
    if size then add("Taille", string.format("%.2f Go", size / 1e9)) end
    return L
end

local function draw_info(out)
    local L = info_lines()
    local w, lh = 760, 46
    local h = 90 + #L * lh
    local x, y = W - w - 60, 60
    out[#out + 1] = shape(rrect(x, y, w, h, 24), C.base, "18")
    out[#out + 1] = text(x + 36, y + 52, 1, 30, C.mauve, "Informations de lecture", true)
    for i, kv in ipairs(L) do
        local ly = y + 70 + i * lh
        out[#out + 1] = text(x + 36, ly, 1, 22, C.sub, kv[1], false)
        out[#out + 1] = text(x + 250, ly, 1, 22, C.text, kv[2], true)
    end
end

local function render()
    local out = {}
    if state.visible or is_paused() or state.menu then draw_bar(out) end
    if state.menu then draw_menu(out) end
    if state.info then draw_info(out) end
    ov.data = table.concat(out, "\n")
    ov:update()
end

-- ─────────────────────────────── Logique ───────────────────────────────

local hide_timer
local function poke()
    state.last_input = mp.get_time()
    state.visible = true
    if hide_timer then hide_timer:kill() end
    hide_timer = mp.add_timeout(5, function()
        if not is_paused() and not state.menu then
            state.visible = false
            state.area = "buttons"
            render()
        end
    end)
    render()
end

local function seek(sec)
    mp.commandv("seek", sec, "relative")
end

local function handle(btn)
    local m = state.menu
    if m then
        if btn == "up" then m.idx = math.max(1, m.idx - 1)
        elseif btn == "down" then m.idx = math.min(#m.items, m.idx + 1)
        elseif btn == "accept" then m.items[m.idx].run(); state.menu = nil
        elseif btn == "back" then state.menu = nil
        end
        poke()
        return
    end
    local shown = state.visible or is_paused()
    if btn == "select" then state.info = not state.info; render(); return end
    if btn == "menu" then open_image(); poke(); return end
    if btn == "lb" then seek(-30); poke(); return end
    if btn == "rb" then seek(30); poke(); return end
    if btn == "x" then open_tracks("audio"); poke(); return end
    if btn == "y" then open_tracks("sub"); poke(); return end
    if not shown then
        if btn == "left" then seek(-10)
        elseif btn == "right" then seek(10)
        elseif btn == "accept" then
            state.area, state.focus = "buttons", 3
        end
        poke()
        return
    end
    if btn == "back" then
        if state.info then state.info = false
        else state.visible = false; state.area = "buttons"; if hide_timer then hide_timer:kill() end end
        render()
        return
    end
    if btn == "up" then state.area = "bar"
    elseif btn == "down" then state.area = "buttons"
    elseif btn == "left" or btn == "right" then
        local d = btn == "left" and -1 or 1
        if state.area == "bar" then seek(10 * d)
        else state.focus = math.max(1, math.min(#buttons, state.focus + d)) end
    elseif btn == "accept" then
        if state.area == "bar" then mp.commandv("cycle", "pause")
        else buttons[state.focus].run() end
    end
    poke()
end

mp.register_script_message("noosfin-pad", handle)

local keys = {
    UP = "up", DOWN = "down", LEFT = "left", RIGHT = "right", ENTER = "accept", KP_ENTER = "accept",
    ESC = "back", BS = "back", TAB = "select", i = "select", PGUP = "lb", PGDWN = "rb", a = "x", s = "y", m = "menu",
    GAMEPAD_DPAD_UP = "up", GAMEPAD_DPAD_DOWN = "down", GAMEPAD_DPAD_LEFT = "left", GAMEPAD_DPAD_RIGHT = "right",
    GAMEPAD_ACTION_DOWN = "accept", GAMEPAD_ACTION_RIGHT = "back", GAMEPAD_ACTION_LEFT = "x", GAMEPAD_ACTION_UP = "y",
    GAMEPAD_START = "menu", GAMEPAD_BACK = "select", GAMEPAD_LEFT_SHOULDER = "lb", GAMEPAD_RIGHT_SHOULDER = "rb",
}
for key, btn in pairs(keys) do
    mp.add_forced_key_binding(key, "noosfin-" .. key, function() handle(btn) end, { repeatable = true })
end
mp.add_forced_key_binding("MOUSE_MOVE", "noosfin-mouse", function() poke() end)

mp.observe_property("pause", "bool", function() render() end)
mp.add_periodic_timer(0.5, function()
    if state.visible or is_paused() or state.info or state.menu then render() end
end)
mp.register_event("file-loaded", function() poke() end)
restore_preset()
