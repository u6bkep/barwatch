//! PROTOTYPE window: the timeline on its own, with a clock standing in for the
//! game. The real app will embed [`Timeline`] in its party view and feed it the
//! game position and a server's statistics.

use super::{axis::clock, shaders, Action, Data, Icons, Timeline};
use anyhow::Result;
use damascene_core::prelude::*;
use damascene_core::widgets::toggle;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How fast a real engine catches up after a jump, measured on one machine.
const CATCH_UP_SPEED: f32 = 55.0;

#[derive(Default)]
pub struct ProtoOptions {
    pub start_s: f32,
    /// Add made-up milestones (flagged as samples) to try the event lane.
    pub sample_markers: bool,
    /// DEVELOPMENT FIXTURE: a Beyond-All-Reason checkout to read marker
    /// pictures from. Nothing of it is copied into barwatch.
    pub game_art: Option<PathBuf>,
    /// Pretend statistics exist only up to this game time.
    pub computed_to: Option<f32>,
    /// Start paused.
    pub paused: bool,
}

pub struct Proto {
    title: String,
    timeline: Timeline,
    speed: f32,
    last: Instant,
}

impl Proto {
    pub fn open(path: &Path, opts: ProtoOptions) -> Result<Proto> {
        let mut data = Data::from_replay(path)?;
        if opts.sample_markers {
            data.add_sample_markers();
        }
        if let Some(c) = opts.computed_to {
            data.computed_s = c.clamp(0.0, data.duration_s);
        }
        let icons = match &opts.game_art {
            Some(root) => {
                let names = data.picture_names();
                let icons = Icons::from_game_checkout(root, names.iter().map(String::as_str));
                eprintln!("read {} marker pictures from {}", icons.len(), root.display());
                icons
            }
            None => Icons::default(),
        };
        let title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let mut timeline = Timeline::new(data);
        timeline.set_icons(icons);
        timeline.set_game(opts.start_s);
        Ok(Proto { title, timeline, speed: if opts.paused { 0.0 } else { 1.0 }, last: Instant::now() })
    }
}

impl App for Proto {
    fn before_build(&mut self) {
        let dt = self.last.elapsed().as_secs_f32().min(0.5);
        self.last = Instant::now();
        let speed = if self.timeline.jumping_to().is_some() { CATCH_UP_SPEED } else { self.speed };
        let mut next = self.timeline.game_s() + dt * speed;
        if let Some(target) = self.timeline.jumping_to() {
            next = next.min(target);
        }
        self.timeline.set_game(next);
    }

    fn build(&self, cx: &BuildCx) -> El {
        let speeds = [(0.0, "Paused"), (1.0, "1x"), (4.0, "4x"), (20.0, "20x")];
        let stand_in = row([
            text(self.title.clone()).caption().muted().ellipsis().width(Size::Fill(1.0)),
            text("Stand-in for the game:").caption().muted(),
            toggle_group("proto:speed", &self.speed, speeds).small(),
        ])
        .gap(tokens::SPACE_3)
        .align(Align::Center);
        let frame = if self.speed > 0.0 { Duration::from_millis(50) } else { Duration::from_millis(500) };
        let main = page([stand_in, scroll([self.timeline.view(cx)]).key("proto-scroll").fill_height()])
            .gap(tokens::SPACE_3)
            .redraw_within(frame);
        overlays(main, self.timeline.layers().into_iter().map(Some))
    }

    fn on_event(&mut self, event: UiEvent, cx: &EventCx) {
        if toggle::apply_event_single(&mut self.speed, &event, "proto:speed", |s| s.parse().ok()) {
            return;
        }
        // A real client would send the jump to the server; here the clock runs ahead.
        if let Some(Action::Jump(target)) = self.timeline.on_event(&event, cx) {
            eprintln!("jump asked for: {}", clock(target));
        }
    }

    fn drain_focus_requests(&mut self) -> Vec<String> {
        self.timeline.take_focus_request().into_iter().collect()
    }

    fn shaders(&self) -> Vec<AppShader> {
        shaders()
    }
}

impl barwatch_host::WinitWgpuApp for Proto {
    fn pointer_moved(&mut self, position: Option<(f32, f32)>, cx: &EventCx) -> bool {
        self.timeline.pointer_moved(position, cx)
    }
}
