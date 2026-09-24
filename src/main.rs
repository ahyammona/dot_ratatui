use std::{
    collections::VecDeque,
    f64::consts::PI,
    io,
    time::{Duration, Instant},
};

use ratatui::{
    crossterm::{
        event::{
            self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind,
            KeyModifiers, MouseButton, MouseEventKind,
        },
        execute,
    },
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        canvas::{self, Canvas, Circle, Context, Points},
        Block, Paragraph,
    },
    DefaultTerminal, Frame,
};

/// Radius of the big dot, in canvas units (the canvas is 100 units tall).
const DOT_RADIUS: f64 = 7.0;
/// How many past positions the comet trail remembers.
const TRAIL_LEN: usize = 36;
/// Seconds to rest at A or B before heading back (auto mode).
const PAUSE: f64 = 0.5;

// Points A and B, as fractions of the canvas (0.0 to 1.0 in each direction).
const A: (f64, f64) = (0.12, 0.25);
const B: (f64, f64) = (0.88, 0.75);

// ---------------------------------------------------------------- easing

#[derive(Clone, Copy, PartialEq)]
enum Easing {
    Linear,
    Smooth,
    Back,
    Elastic,
    Bounce,
}

impl Easing {
    const ALL: [Easing; 5] = [
        Easing::Linear,
        Easing::Smooth,
        Easing::Back,
        Easing::Elastic,
        Easing::Bounce,
    ];

    fn name(self) -> &'static str {
        match self {
            Easing::Linear => "linear",
            Easing::Smooth => "smooth",
            Easing::Back => "back (overshoot)",
            Easing::Elastic => "elastic (spring)",
            Easing::Bounce => "bounce",
        }
    }

    /// Seconds per trip. The bouncy ones read better when they're slower.
    fn duration(self) -> f64 {
        match self {
            Easing::Linear => 1.0,
            Easing::Smooth => 1.2,
            Easing::Back => 1.0,
            Easing::Elastic => 1.6,
            Easing::Bounce => 1.4,
        }
    }

    /// Maps time (0..1) to progress. Some curves go past 1.0, which is the overshoot.
    fn apply(self, t: f64) -> f64 {
        match self {
            Easing::Linear => t,
            Easing::Smooth => t * t * (3.0 - 2.0 * t),
            Easing::Back => {
                let c1 = 1.70158;
                let c3 = c1 + 1.0;
                let u = t - 1.0;
                1.0 + c3 * u * u * u + c1 * u * u
            }
            Easing::Elastic => {
                if t <= 0.0 {
                    0.0
                } else if t >= 1.0 {
                    1.0
                } else {
                    2f64.powf(-10.0 * t) * ((t * 10.0 - 0.75) * (2.0 * PI / 3.0)).sin() + 1.0
                }
            }
            Easing::Bounce => {
                let (n1, d1) = (7.5625, 2.75);
                if t < 1.0 / d1 {
                    n1 * t * t
                } else if t < 2.0 / d1 {
                    let t = t - 1.5 / d1;
                    n1 * t * t + 0.75
                } else if t < 2.5 / d1 {
                    let t = t - 2.25 / d1;
                    n1 * t * t + 0.9375
                } else {
                    let t = t - 2.625 / d1;
                    n1 * t * t + 0.984375
                }
            }
        }
    }
}

// ------------------------------------------------------------------ state

struct App {
    easing: Easing,
    from: (f64, f64),
    to: (f64, f64),
    started: f64,
    pos: (f64, f64),
    /// Where we are in the current trip, 0..1 (used to draw the curve marker).
    progress: f64,
    /// Auto mode: bounce between A and B by itself.
    auto: bool,
    trail: VecDeque<(f64, f64)>,
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

fn dist(p: (f64, f64), q: (f64, f64)) -> f64 {
    (p.0 - q.0).hypot(p.1 - q.1)
}

impl App {
    fn new() -> Self {
        Self {
            easing: Easing::Back,
            from: A,
            to: B,
            started: 0.0,
            pos: A,
            progress: 0.0,
            auto: true,
            trail: VecDeque::new(),
        }
    }

    /// Start a new trip from wherever the dot is right now.
    fn retarget(&mut self, to: (f64, f64), now: f64) {
        self.from = self.pos;
        self.to = to;
        self.started = now;
    }

    fn set_easing(&mut self, easing: Easing, now: f64) {
        self.easing = easing;
        // Restart the current trip with the new curve so the dot never jumps.
        self.retarget(self.to, now);
    }

    fn cycle_easing(&mut self, now: f64) {
        let i = Easing::ALL
            .iter()
            .position(|e| *e == self.easing)
            .unwrap_or(0);
        self.set_easing(Easing::ALL[(i + 1) % Easing::ALL.len()], now);
    }

    fn toggle_auto(&mut self, now: f64) {
        self.auto = !self.auto;
        if self.auto {
            let far = if dist(self.pos, A) > dist(self.pos, B) {
                A
            } else {
                B
            };
            self.retarget(far, now);
        }
    }

    /// Click (or tap) at terminal cell (col, row): send the dot there.
    fn click(&mut self, col: u16, row: u16, stage: Rect, now: f64) {
        let inner = Block::bordered().inner(stage);
        let inside = col >= inner.x
            && col < inner.x + inner.width
            && row >= inner.y
            && row < inner.y + inner.height;
        if !inside {
            return;
        }
        let nx = ((col - inner.x) as f64 + 0.5) / inner.width as f64;
        let ny = 1.0 - ((row - inner.y) as f64 + 0.5) / inner.height as f64;
        self.auto = false;
        self.retarget((nx.clamp(0.05, 0.95), ny.clamp(0.08, 0.92)), now);
    }

    fn update(&mut self, now: f64) {
        let dur = self.easing.duration();
        let t = ((now - self.started) / dur).clamp(0.0, 1.0);
        self.progress = t;
        let e = self.easing.apply(t);
        self.pos = (
            lerp(self.from.0, self.to.0, e),
            lerp(self.from.1, self.to.1, e),
        );

        // Auto mode: rest for a moment, then head back the other way.
        if self.auto && now - self.started >= dur + PAUSE {
            let next = if self.to == B { A } else { B };
            self.retarget(next, now);
        }

        // Trail: grows while moving, fades away while standing still.
        let moved = self
            .trail
            .back()
            .map_or(true, |&last| dist(last, self.pos) > 0.002);
        if moved {
            self.trail.push_back(self.pos);
            if self.trail.len() > TRAIL_LEN {
                self.trail.pop_front();
            }
        } else {
            self.trail.pop_front();
        }
    }
}

// ------------------------------------------------------------------- main

fn main() -> io::Result<()> {
    let terminal = ratatui::init();
    execute!(io::stdout(), EnableMouseCapture)?;
    let result = run(terminal);
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn run(mut terminal: DefaultTerminal) -> io::Result<()> {
    let start = Instant::now();
    let mut app = App::new();

    loop {
        app.update(start.elapsed().as_secs_f64());
        terminal.draw(|frame| draw(frame, &app))?;

        // Spend the rest of this frame (~16ms, about 60fps) handling input.
        // Handle EVERY queued event, not just one: terminals send a flood of
        // mouse-move events, and reading one per frame makes the queue back up
        // until clicks and keys feel dead.
        let frame_end = Instant::now() + Duration::from_millis(16);
        loop {
            let wait = frame_end.saturating_duration_since(Instant::now());
            if !event::poll(wait)? {
                break;
            }
            let now = start.elapsed().as_secs_f64();
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(())
                    }
                    KeyCode::Char(c @ '1'..='5') => {
                        app.set_easing(Easing::ALL[c as usize - '1' as usize], now)
                    }
                    KeyCode::Tab => app.cycle_easing(now),
                    KeyCode::Char(' ') => app.toggle_auto(now),
                    _ => {}
                },
                Event::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Left) => {
                    let size = terminal.size()?;
                    let (stage, _) = layout(Rect::new(0, 0, size.width, size.height));
                    app.click(m.column, m.row, stage, now);
                }
                _ => {} // mouse moves, drags, releases, resizes: ignore
            }
        }
    }
}

// ---------------------------------------------------------------- drawing

/// Splits the screen into the stage (left) and the side panel (right).
/// The side panel is hidden on narrow terminals.
fn layout(area: Rect) -> (Rect, Rect) {
    let side_w = if area.width >= 70 { 30 } else { 0 };
    let [stage, side] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(side_w)]).areas(area);
    (stage, side)
}

fn draw(frame: &mut Frame, app: &App) {
    let (stage, side) = layout(frame.area());
    draw_stage(frame, app, stage);
    if side.width > 0 {
        draw_side(frame, app, side);
    }
}

/// A filled disc, painted as individual braille dots. Much cheaper than stacking
/// outline circles, which matters a lot on big terminals.
fn fill_disc(ctx: &mut Context<'_>, x: f64, y: f64, radius: f64, dot: f64, color: Color) {
    let step = dot * 0.75; // a bit finer than one braille dot, so there are no gaps
    let r2 = radius * radius;
    let mut coords = Vec::new();
    let mut dy = -radius;
    while dy <= radius {
        let mut dx = -radius;
        while dx <= radius {
            if dx * dx + dy * dy <= r2 {
                coords.push((x + dx, y + dy));
            }
            dx += step;
        }
        dy += step;
    }
    ctx.draw(&Points {
        coords: &coords,
        color,
    });
}

/// Trail color: dark orange at the tail, bright orange near the dot.
fn fade(f: f64) -> Color {
    Color::Rgb(lerp(90.0, 255.0, f) as u8, lerp(40.0, 150.0, f) as u8, 0)
}

fn draw_stage(frame: &mut Frame, app: &App, area: Rect) {
    let title = format!(
        " click: move | 1-5 / Tab: easing | space: auto {} | q: quit ",
        if app.auto { "ON" } else { "off" }
    );
    let block = Block::bordered().title(title);
    let inner = block.inner(area);

    // A braille cell is 2 dots wide and 4 dots tall, so dots are roughly square.
    // Matching the canvas units to that keeps the circle round instead of oval.
    let dots_w = inner.width.max(1) as f64 * 2.0;
    let dots_h = inner.height.max(1) as f64 * 4.0;
    let world_h = 100.0;
    let world_w = world_h * dots_w / dots_h;
    let px = world_h / dots_h; // size of one braille dot in canvas units

    let to_world = |p: (f64, f64)| (p.0 * world_w, p.1 * world_h);
    let (a, b) = (to_world(A), to_world(B));
    let head = to_world(app.pos);
    let trail: Vec<_> = app.trail.iter().map(|&p| to_world(p)).collect();

    let canvas = Canvas::default()
        .block(block)
        .marker(Marker::Braille)
        .x_bounds([0.0, world_w])
        .y_bounds([0.0, world_h])
        .paint(move |ctx| {
            // The path from A to B.
            ctx.draw(&canvas::Line {
                x1: a.0,
                y1: a.1,
                x2: b.0,
                y2: b.1,
                color: Color::DarkGray,
            });
            ctx.print(a.0 - 1.0, a.1 - 14.0, "A");
            ctx.print(b.0 - 1.0, b.1 + 14.0, "B");

            // New layer so everything below draws on top of the path.
            ctx.layer();

            // Comet trail, oldest first: small and dim at the tail, big and bright near the dot.
            let n = trail.len();
            for (i, &(x, y)) in trail.iter().enumerate() {
                if i % 2 == 1 {
                    continue; // every other point is plenty, and keeps it fast
                }
                let f = (i + 1) as f64 / n as f64;
                fill_disc(ctx, x, y, DOT_RADIUS * (0.25 + 0.6 * f), px, fade(f));
            }

            // The dot itself, last so it sits on top.
            fill_disc(ctx, head.0, head.1, DOT_RADIUS, px, Color::Yellow);
        });

    frame.render_widget(canvas, area);
}

fn draw_side(frame: &mut Frame, app: &App, area: Rect) {
    let [list_area, graph_area] = Layout::vertical([
        Constraint::Length(Easing::ALL.len() as u16 + 2),
        Constraint::Fill(1),
    ])
    .areas(area);

    // Easing list, current one highlighted.
    let lines: Vec<Line> = Easing::ALL
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let current = *e == app.easing;
            let style = if current {
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::Gray)
            };
            let arrow = if current { ">" } else { " " };
            Line::from(Span::styled(
                format!("{arrow} {} {}", i + 1, e.name()),
                style,
            ))
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" easing ")),
        list_area,
    );

    // The curve, with a marker riding along it as the dot moves.
    let easing = app.easing;
    let t_now = app.progress;
    let curve = Canvas::default()
        .block(Block::bordered().title(" curve "))
        .marker(Marker::Braille)
        .x_bounds([0.0, 1.0])
        .y_bounds([-0.1, 1.5])
        .paint(move |ctx| {
            // The target line: reaching 1.0 means arriving.
            ctx.draw(&canvas::Line {
                x1: 0.0,
                y1: 1.0,
                x2: 1.0,
                y2: 1.0,
                color: Color::DarkGray,
            });
            let n = 80;
            for i in 0..n {
                let t0 = i as f64 / n as f64;
                let t1 = (i + 1) as f64 / n as f64;
                ctx.draw(&canvas::Line {
                    x1: t0,
                    y1: easing.apply(t0),
                    x2: t1,
                    y2: easing.apply(t1),
                    color: Color::Cyan,
                });
            }
            ctx.layer();
            let y = easing.apply(t_now);
            for radius in [0.02, 0.035] {
                ctx.draw(&Circle {
                    x: t_now,
                    y,
                    radius,
                    color: Color::Yellow,
                });
            }
        });
    frame.render_widget(curve, graph_area);
}
