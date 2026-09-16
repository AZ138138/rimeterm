//! Pac-Man pane: renders the [`rimeterm_game`] engine and maps keys to
//! engine input, persisting the best score to a global JSON file.

use std::any::Any;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    prelude::Widget,
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
};
use rimeterm_core::pane::{PaneId, PaneProvider, PaneRenderCtx, RenderOutcome};
use rimeterm_game::engine::{Banner, BannerColor, FPS, Game, GhostId, GhostState, Input};
use rimeterm_game::map::{Maze, Pellet};

/// Upstream `FRAME_MS = 16`.
const TICK: Duration = Duration::from_millis(1000 / FPS);

/// Maze footprint in engine cells (transposed template: 21 rows × 26 cols).
const MAZE_ROWS: u16 = 21;
const MAZE_COLS: u16 = 26;
/// Maze + banner line; the HUD lives beside the maze, not below it.
const MIN_ROWS: u16 = MAZE_ROWS + 1;
/// Maze + one HUD column (~13 wide, fits "Current Score 999999").
const MIN_COLS: u16 = MAZE_COLS + 14;

const KEY_HINTS: &str = " ↑↓←→ move · r restart · y/n confirm ";

pub struct GamePane {
    id: PaneId,
    best_path: PathBuf,
    game: Game,
    next_tick: Instant,
    pending: Option<Input>,
    saved_best: u32,
    visible: bool,
}

impl GamePane {
    /// Build the pane, loading the persisted best score (a bare JSON
    /// number) from `best_path`.
    pub fn new(best_path: PathBuf) -> Self {
        let best = std::fs::read_to_string(&best_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<u32>(&raw).ok())
            .unwrap_or(0);
        let game = Game::new(best);
        Self {
            id: PaneId::next(),
            best_path,
            game,
            next_tick: Instant::now() + TICK,
            pending: None,
            saved_best: best,
            visible: true,
        }
    }

    /// Persist `max(best, current score)` so a mid-run exit keeps the high
    /// score (the engine itself only commits on game over).
    fn save_best(&mut self) {
        let best = self.game.best_score().max(self.game.score());
        self.saved_best = best;
        if let Some(parent) = self.best_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(encoded) = serde_json::to_string(&best) {
            let _ = std::fs::write(&self.best_path, encoded);
        }
    }
}

fn ghost_color(id: GhostId) -> Color {
    match id {
        GhostId::Blinky => Color::Red,
        GhostId::Pinky => Color::Magenta,
        GhostId::Inky => Color::Cyan,
        GhostId::Clyde => Color::Rgb(255, 165, 0),
    }
}

fn banner_color(color: BannerColor) -> Color {
    match color {
        BannerColor::Yellow => Color::Yellow,
        BannerColor::Cyan => Color::Cyan,
        BannerColor::Magenta => Color::Magenta,
        BannerColor::Green => Color::Green,
        BannerColor::Red => Color::Red,
        BannerColor::Gray => Color::DarkGray,
    }
}

fn banner_text(game: &Game, banner: Banner) -> String {
    match banner {
        Banner::None => String::new(),
        Banner::Countdown(n) => format!("{n}"),
        Banner::Ready => "Ready!".to_string(),
        Banner::Power => "Power Pellet Active!".to_string(),
        Banner::Fruit => "Fruit Collected!".to_string(),
        Banner::GhostEaten => "Ghost eaten!".to_string(),
        Banner::Waiting => "Ghosts retreating...".to_string(),
        Banner::LevelClear(n) => format!("Level Cleared! {n}"),
        Banner::Won => "You collected all the dots! · r to restart".to_string(),
        Banner::Lost => format!(
            "Game Over · score {} · best {} · r to restart",
            game.score(),
            game.best_score()
        ),
        Banner::ConfirmRestart => "Confirm restart? [Y] Yes / [N] No".to_string(),
    }
}

/// A HUD label line: dim, so the bright value under it reads first.
fn dim(text: impl Into<String>) -> Line<'static> {
    Line::styled(text.into(), Style::default().add_modifier(Modifier::DIM))
}

/// A HUD value line: full-brightness white.
fn bright(text: impl Into<String>) -> Line<'static> {
    Line::styled(text.into(), Style::default().fg(Color::White))
}

/// Upstream `cell_visual` for one maze cell: `(char, color)`.
fn cell_visual(game: &Game, maze: &Maze, r: i32, c: i32) -> (char, Color) {
    let (pr, pc) = game.player();
    if pr == r && pc == c {
        let color = if game.power_seconds_left() > 0 {
            Color::LightCyan
        } else {
            Color::LightYellow
        };
        return ('@', color);
    }
    if let Some(ghost) = game.ghosts().iter().find(|g| g.r == r && g.c == c) {
        if ghost.state == GhostState::Eyes {
            return ('&', Color::White);
        }
        if ghost.state == GhostState::Frightened && !game.ghost_was_eaten(ghost.id) {
            return ('&', Color::LightBlue);
        }
        return ('&', ghost_color(ghost.id));
    }
    if let Some(fruit) = game.fruit()
        && fruit.r == r
        && fruit.c == c
    {
        return (fruit.symbol, Color::Magenta);
    }
    match maze.pellets[r as usize][c as usize] {
        Pellet::Dot => ('·', Color::White),
        Pellet::Power => ('*', Color::Yellow),
        Pellet::None => {
            if maze.is_wall_strict(r, c) {
                (maze.base[r as usize][c as usize], Color::Blue)
            } else if maze.is_door(r, c) {
                ('-', Color::White)
            } else {
                (' ', Color::White)
            }
        }
    }
}

fn draw_cell(buf: &mut Buffer, x: u16, y: u16, ch: char, color: Color) {
    buf[(x, y)]
        .set_char(ch)
        .set_style(Style::default().fg(color));
}

impl PaneProvider for GamePane {
    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }
    fn id(&self) -> PaneId {
        self.id
    }
    fn title(&self) -> &str {
        "Pac-Man"
    }

    fn render(
        &mut self,
        area: Rect,
        frame: &mut Frame<'_>,
        ctx: &PaneRenderCtx<'_>,
    ) -> RenderOutcome {
        let border_style = if ctx.focused {
            Style::default().fg(ctx.focus_color)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let block = Block::default()
            .title(" Pac-Man ")
            .title_bottom(Line::styled(KEY_HINTS, border_style))
            .borders(Borders::ALL)
            .border_style(border_style);
        let inner = block.inner(area);
        block.render(area, frame.buffer_mut());
        if inner.width == 0 || inner.height == 0 {
            return RenderOutcome::default();
        }
        if inner.width < MIN_COLS || inner.height < MIN_ROWS {
            frame.render_widget(
                Paragraph::new(format!(
                    "Pac-Man needs {}×{} cells · now {}×{}",
                    MIN_COLS, MIN_ROWS, inner.width, inner.height
                )),
                inner,
            );
            return RenderOutcome::default();
        }

        // Maze on the left, HUD column on its right — one screen row per
        // maze row keeps the pane compact vertically.
        let maze_x = inner.x;
        let maze_y = inner.y;
        let maze = self.game.maze().clone();
        let buf = frame.buffer_mut();
        for r in 0..MAZE_ROWS as i32 {
            for c in 0..MAZE_COLS as i32 {
                let (ch, color) = cell_visual(&self.game, &maze, r, c);
                draw_cell(buf, maze_x + c as u16, maze_y + r as u16, ch, color);
            }
        }

        // HUD column, vertically centered beside the maze.
        let fruits: String = if self.game.collected_fruits().is_empty() {
            "-".to_string()
        } else {
            self.game
                .collected_fruits()
                .iter()
                .map(|symbol| symbol.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let hud: Vec<Line> = vec![
            dim("High Score"),
            bright(self.game.best_score().to_string()),
            Line::from(""),
            dim("Current Score"),
            bright(self.game.score().to_string()),
            Line::from(""),
            dim("Game Time"),
            bright(format!("{}s", self.game.elapsed_seconds())),
            Line::from(""),
            dim("Level"),
            bright(self.game.level().to_string()),
            Line::from(""),
            dim("Lives"),
            bright("♥".repeat(self.game.lives() as usize)),
            Line::from(""),
            dim("Fruit"),
            bright(fruits),
        ];
        let hud_height = hud.len() as u16;
        let hud_y = inner.y + (inner.height.saturating_sub(hud_height)) / 2;
        let hud_width = inner.width.saturating_sub(MAZE_COLS + 1);
        if hud_width > 0 {
            frame.render_widget(
                Paragraph::new(hud).wrap(ratatui::widgets::Wrap { trim: true }),
                Rect {
                    x: maze_x + MAZE_COLS + 1,
                    y: hud_y,
                    width: hud_width,
                    height: hud_height.min(inner.height),
                },
            );
        }

        // Banner centered under the maze.
        let (banner, color) = self.game.banner();
        let style = Style::default().fg(banner_color(color));
        frame.render_widget(
            Paragraph::new(Line::styled(banner_text(&self.game, banner), style)).centered(),
            Rect {
                x: inner.x,
                y: maze_y + MAZE_ROWS,
                width: inner.width,
                height: 1,
            },
        );
        RenderOutcome::default()
    }

    fn on_key(&mut self, key: KeyEvent) -> bool {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return false;
        }
        let input = match key.code {
            KeyCode::Up => Input::Up,
            KeyCode::Down => Input::Down,
            KeyCode::Left => Input::Left,
            KeyCode::Right => Input::Right,
            KeyCode::Char('r' | 'R') => Input::Restart,
            KeyCode::Char('y' | 'Y') => Input::Yes,
            KeyCode::Char('n' | 'N') => Input::No,
            _ => return false,
        };
        self.pending = Some(input);
        true
    }

    fn poll_background(&mut self) -> bool {
        if !self.visible {
            return false;
        }
        let now = Instant::now();
        if now < self.next_tick {
            return false;
        }
        let mut frames = 0;
        while Instant::now() >= self.next_tick && frames < 8 {
            let input = self.pending.take().unwrap_or(Input::Idle);
            self.game.tick(input);
            self.next_tick += TICK;
            frames += 1;
        }
        if Instant::now() >= self.next_tick {
            self.next_tick = Instant::now() + TICK;
        }
        let live_best = self.game.best_score().max(self.game.score());
        if live_best != self.saved_best {
            self.save_best();
        }
        frames > 0
    }

    fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
        if visible {
            self.next_tick = Instant::now() + TICK;
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent};
    use ratatui::{Terminal as TestTerminal, backend::TestBackend};
    use rimeterm_core::pane::PaneProvider;

    use super::GamePane;

    fn rendered_content(pane: &mut GamePane, width: u16, height: u16) -> String {
        let mut terminal =
            TestTerminal::new(TestBackend::new(width, height)).expect("create test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                let ctx = rimeterm_core::pane::PaneRenderCtx {
                    focused: true,
                    title_override: None,
                    focus_color: ratatui::style::Color::Yellow,
                };
                pane.render(area, frame, &ctx);
            })
            .expect("render game pane");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn renders_maze_player_and_hud() {
        let best = tempfile::tempdir().expect("fixture dir").keep();
        let mut pane = GamePane::new(best.join("pacman-best.json"));
        let content = rendered_content(&mut pane, 60, 30);
        assert!(content.contains('@'), "player glyph visible");
        assert!(content.contains('·'), "pellet glyphs visible");
        assert!(content.contains("High Score"), "HUD visible");
    }

    #[test]
    fn small_area_shows_size_hint() {
        let best = tempfile::tempdir().expect("fixture dir").keep();
        let mut pane = GamePane::new(best.join("pacman-best.json"));
        let content = rendered_content(&mut pane, 20, 10);
        assert!(content.contains("needs"), "size hint shown");
    }

    #[test]
    fn loads_and_saves_best_score() {
        let dir = tempfile::tempdir().expect("fixture dir");
        let best_path = dir.path().join("pacman-best.json");
        std::fs::write(&best_path, "123").expect("seed best score");
        let mut pane = GamePane::new(best_path.clone());
        assert_eq!(pane.game.best_score(), 123);
        pane.game.add_score(500);
        pane.save_best();
        let saved = std::fs::read_to_string(&best_path).expect("best file written");
        assert_eq!(saved, "500");
    }

    #[test]
    fn key_presses_queue_engine_input() {
        let best = tempfile::tempdir().expect("fixture dir").keep();
        let mut pane = GamePane::new(best.join("pacman-best.json"));
        assert!(pane.on_key(KeyEvent::from(KeyCode::Up)));
        assert_eq!(pane.pending, Some(super::Input::Up));
        assert!(!pane.on_key(KeyEvent::from(KeyCode::Char('z'))));
    }

    #[test]
    fn hidden_pane_does_not_tick() {
        let best = tempfile::tempdir().expect("fixture dir").keep();
        let mut pane = GamePane::new(best.join("pacman-best.json"));
        pane.set_visible(false);
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!pane.poll_background());
    }
}
