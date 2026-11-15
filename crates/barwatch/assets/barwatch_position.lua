-- barwatch position reporter.
--
-- Written by the barwatch app into a folder of its own and handed only to the
-- engines barwatch starts (as SPRING_DATADIR); a game started from BAR's launcher
-- never sees this file. It tells the app where the replay is by writing lines
--
--   [barwatch] f=<game frame> speed=<speed factor> paused=<0|1>
--
-- to the engine's standard output, which the app captures (BAR gives user widgets
-- no socket), once a second and at once when the speed or pause changes. "paused"
-- means the game is not advancing: paused, or no frame for STALL seconds (below).
--
-- Straight to standard output, not Spring.Echo: an echo also lands in every
-- viewer's in-game console, once a second, and in the console history BAR saves.
-- With no standard output to write to, the widget stays silent and the app shows
-- the position as unknown.
--
-- Part of barwatch, MIT OR Apache-2.0.

function widget:GetInfo()
	return {
		name = "barwatch position",
		desc = "Tells the barwatch app where the replay is (loads only in games barwatch starts)",
		author = "barwatch",
		license = "MIT OR Apache-2.0",
		layer = 0,
		enabled = true,
	}
end

local GetGameFrame = Spring.GetGameFrame
local GetGameSpeed = Spring.GetGameSpeed
local GetTimer = Spring.GetTimer
local DiffTimers = Spring.DiffTimers
local format = string.format
local stdout = io and io.stdout

-- Wall-clock seconds between reports.
local INTERVAL = 1.0
-- A pause on the server reaches a spectator only as frames that stop coming (its
-- own pause flag stays off), so a game that has not moved for this long is
-- reported as paused. Frames come every 0.11 s at the slowest speed, 0.3x.
local STALL = 0.5

local lastFrame, frameTimer
local rawSpeed, speedText
local lastTimer, lastSpeed, lastPaused

function widget:Initialize()
	-- Only ever a watcher: never part of a game being played.
	if not (Spring.IsReplay() or Spring.GetSpectatingState()) or not stdout then
		widgetHandler:RemoveWidget(self)
	end
end

function widget:Update()
	local now = GetTimer()
	local frame = GetGameFrame()
	if frame ~= lastFrame then
		lastFrame, frameTimer = frame, now
	end
	local _, speed, paused = GetGameSpeed()
	if speed ~= rawSpeed then
		rawSpeed, speedText = speed, format("%.2f", speed or 0)
	end
	paused = (paused or DiffTimers(now, frameTimer) >= STALL) and 1 or 0
	if lastTimer == nil or speedText ~= lastSpeed or paused ~= lastPaused or DiffTimers(now, lastTimer) >= INTERVAL then
		lastTimer, lastSpeed, lastPaused = now, speedText, paused
		stdout:write(format("[barwatch] f=%d speed=%s paused=%d\n", frame, speedText, paused))
		stdout:flush()
	end
end
