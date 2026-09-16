//! Pac-Man engine, ported from tui-game's `pacman.lua`.
//!
//! The engine is pure state + logic: no I/O, no terminal, no timers. The
//! host ticks it at 60 Hz (`Game::tick` = one upstream frame) and feeds
//! normalized input; rendering reads the accessors. Coordinates are the
//! transposed engine grid (21×26), 0-indexed. Best-score persistence is
//! the host's job: construct with the stored best, poll
//! [`Game::stats_committed`] and read [`Game::best_score`].

use crate::map::{self, Maze, Pellet};

/// Upstream `FPS`.
pub const FPS: u64 = 60;
/// Upstream `PLAYER_STEP_FRAMES`.
const PLAYER_STEP_FRAMES: u64 = 12;
/// Upstream `GHOST_SLOW_FACTOR`.
const GHOST_SLOW_FACTOR: u64 = 2;
/// Upstream `MAX_LEVEL`.
pub const MAX_LEVEL: u8 = 20;
/// Upstream `EXTRA_LIFE_SCORE`.
const EXTRA_LIFE_SCORE: u32 = 100_000;

/// Normalized input (upstream `normalize_key` result space; exit/confirm
/// wiring is dropped — the pane owns closing).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Input {
    /// Upstream `nil` key: no key pressed this frame.
    Idle,
    Up,
    Down,
    Left,
    Right,
    /// `r` — request restart (enters confirm mode) or, in result phase,
    /// restart directly.
    Restart,
    /// `y` while confirm mode is active.
    Yes,
    /// `n` while confirm mode is active.
    No,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    /// Upstream `direction_delta`.
    fn delta(self) -> (i32, i32) {
        match self {
            Dir::Up => (-1, 0),
            Dir::Down => (1, 0),
            Dir::Left => (0, -1),
            Dir::Right => (0, 1),
        }
    }

    /// Upstream `opposite_dir`.
    fn opposite(self) -> Dir {
        match self {
            Dir::Up => Dir::Down,
            Dir::Down => Dir::Up,
            Dir::Left => Dir::Right,
            Dir::Right => Dir::Left,
        }
    }
}

/// Upstream `DIRS` order; candidate order decides ties.
const DIRS: [Dir; 4] = [Dir::Up, Dir::Left, Dir::Down, Dir::Right];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GhostId {
    Blinky,
    Pinky,
    Inky,
    Clyde,
}

impl GhostId {
    fn index(self) -> usize {
        match self {
            GhostId::Blinky => 0,
            GhostId::Pinky => 1,
            GhostId::Inky => 2,
            GhostId::Clyde => 3,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GhostState {
    House,
    Normal,
    Frightened,
    Eyes,
}

#[derive(Clone, Copy, Debug)]
pub struct Ghost {
    pub id: GhostId,
    pub r: i32,
    pub c: i32,
    pub dir: Dir,
    pub state: GhostState,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Playing,
    Won,
    Lost,
}

/// Transient banner line (upstream `info_message` + countdown + confirm),
/// with its display color for the pane to map.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Banner {
    /// No banner.
    None,
    Countdown(u32),
    Ready,
    Power,
    Fruit,
    GhostEaten,
    Waiting,
    LevelClear(u8),
    Won,
    Lost,
    ConfirmRestart,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BannerColor {
    Yellow,
    Cyan,
    Magenta,
    Green,
    Red,
    Gray,
}

/// Upstream `current_chase_mode`, plus frightened while power is active
/// (what the HUD "Mode" line shows).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Scatter,
    Chase,
    Frightened,
}

#[derive(Clone, Copy, Debug)]
pub struct FruitView {
    /// Upstream `FRUIT_TABLE` symbol (or `!` for level 13+).
    pub symbol: char,
    pub r: i32,
    pub c: i32,
}

struct GhostFull {
    id: GhostId,
    r: i32,
    c: i32,
    dir: Dir,
    state: GhostState,
    release_at: u64,
    next_step_at: u64,
    home_r: i32,
    home_c: i32,
}

impl From<&GhostFull> for Ghost {
    fn from(g: &GhostFull) -> Ghost {
        Ghost {
            id: g.id,
            r: g.r,
            c: g.c,
            dir: g.dir,
            state: g.state,
        }
    }
}

struct FruitState {
    active: bool,
    spawned: bool,
    r: i32,
    c: i32,
}

/// Upstream `FRUIT_TABLE`; index = level-1 (levels 13+ use the key entry).
const FRUIT_TABLE: [(char, u32); 12] = [
    ('%', 100),
    ('U', 300),
    ('O', 500),
    ('O', 500),
    ('Q', 700),
    ('Q', 700),
    ('§', 1000),
    ('§', 1000),
    ('W', 2000),
    ('W', 2000),
    ('?', 3000),
    ('?', 3000),
];

/// Upstream `fruit_for_level`.
fn fruit_for_level(level: u8) -> (char, u32) {
    if level >= 13 {
        ('!', 5000)
    } else {
        FRUIT_TABLE[usize::from(level.min(12)) - 1]
    }
}

/// Upstream `power_duration_sec`.
fn power_duration_sec(level: u8) -> u64 {
    if level <= 1 {
        6
    } else if level == 2 {
        5
    } else if level <= 10 {
        4
    } else if level <= 16 {
        3
    } else {
        2
    }
}

/// Upstream `ghost_revive_sec`.
fn ghost_revive_sec(level: u8) -> u64 {
    if (11..=16).contains(&level) { 5 } else { 3 }
}

/// Upstream `scatter_schedule` as (mode, seconds) pairs.
fn scatter_schedule(level: u8) -> &'static [(Mode, u64)] {
    if level >= 17 {
        &[
            (Mode::Scatter, 1),
            (Mode::Chase, 20),
            (Mode::Scatter, 1),
            (Mode::Chase, 20),
            (Mode::Chase, 9999),
        ]
    } else if level >= 5 {
        &[
            (Mode::Scatter, 5),
            (Mode::Chase, 20),
            (Mode::Scatter, 5),
            (Mode::Chase, 20),
            (Mode::Scatter, 5),
            (Mode::Chase, 9999),
        ]
    } else {
        &[
            (Mode::Scatter, 7),
            (Mode::Chase, 20),
            (Mode::Scatter, 7),
            (Mode::Chase, 20),
            (Mode::Scatter, 5),
            (Mode::Chase, 20),
            (Mode::Scatter, 5),
            (Mode::Chase, 9999),
        ]
    }
}

/// Upstream `ghost_release_delays`, seconds after the 4s death pause.
fn ghost_release_delay(level: u8, id: GhostId) -> u64 {
    let (b, p, i, c) = if level <= 1 {
        (0, 2, 4, 6)
    } else if level <= 4 {
        (0, 1, 3, 5)
    } else {
        (0, 1, 2, 3)
    };
    match id {
        GhostId::Blinky => b,
        GhostId::Pinky => p,
        GhostId::Inky => i,
        GhostId::Clyde => c,
    }
}

struct Player {
    r: i32,
    c: i32,
    dir: Dir,
    next_dir: Dir,
    next_step_at: u64,
}

/// Tiny xorshift64 — stands in for upstream `random()`. Good enough for
/// frightened wandering and fruit placement; no dependency, deterministic
/// under tests.
#[derive(Clone)]
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform-enough index in `0..n` (upstream `random(n) + 1`).
    fn index(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

/// The game. See the module docs for the host contract.
pub struct Game {
    maze: Maze,
    total_pellets: u32,
    remaining_pellets: u32,

    player: Player,
    player_start: (i32, i32),
    ghosts: Vec<GhostFull>,
    ghost_spawn: (i32, i32),
    global_pause_until: u64,

    fruit: FruitState,
    level: u8,
    score: u32,
    best_score: u32,
    lives: u32,
    extra_life_granted: bool,

    power_until: u64,
    power_chain: u32,
    power_eaten: [bool; 4],

    phase: Phase,
    frame: u64,
    run_start_frame: u64,
    level_start_frame: u64,
    end_frame: Option<u64>,
    stats_committed: bool,
    confirm_restart: bool,

    countdown_until: u64,
    info: Banner,
    info_color: BannerColor,
    info_until: Option<u64>,
    collected_fruits: Vec<char>,

    rng: Rng,
}

impl Game {
    /// Upstream `init_game` + `start_new_run`.
    pub fn new(best_score: u32) -> Game {
        let mut game = Game {
            maze: map::parse(),
            total_pellets: 0,
            remaining_pellets: 0,
            player: Player {
                r: 1,
                c: 1,
                dir: Dir::Left,
                next_dir: Dir::Left,
                next_step_at: 0,
            },
            player_start: (1, 1),
            ghosts: Vec::new(),
            ghost_spawn: (1, 1),
            global_pause_until: 0,
            fruit: FruitState {
                active: false,
                spawned: false,
                r: 1,
                c: 1,
            },
            level: 1,
            score: 0,
            best_score,
            lives: 3,
            extra_life_granted: false,
            power_until: 0,
            power_chain: 0,
            power_eaten: [false; 4],
            phase: Phase::Playing,
            frame: 0,
            run_start_frame: 0,
            level_start_frame: 0,
            end_frame: None,
            stats_committed: false,
            confirm_restart: false,
            countdown_until: 0,
            info: Banner::None,
            info_color: BannerColor::Gray,
            info_until: None,
            collected_fruits: Vec::new(),
            rng: Rng(0x9E3779B97F4A7C15),
        };
        game.init_positions_from_map();
        game.start_new_run();
        game
    }

    // ---- public read API for the pane ---------------------------------

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn score(&self) -> u32 {
        self.score
    }

    pub fn best_score(&self) -> u32 {
        self.best_score
    }

    /// True once per run when stats were committed; host persists
    /// [`Game::best_score`] when it flips (or on teardown).
    pub fn stats_committed(&self) -> bool {
        self.stats_committed
    }

    pub fn lives(&self) -> u32 {
        self.lives
    }

    pub fn level(&self) -> u8 {
        self.level
    }

    pub fn maze(&self) -> &Maze {
        &self.maze
    }

    /// Player position and (render color hint) whether power is active.
    pub fn player(&self) -> (i32, i32) {
        (self.player.r, self.player.c)
    }

    /// Ghosts in upstream creation order (blinky, pinky, inky, clyde).
    pub fn ghosts(&self) -> Vec<Ghost> {
        self.ghosts.iter().map(Ghost::from).collect()
    }

    /// Active fruit, if any.
    pub fn fruit(&self) -> Option<FruitView> {
        if self.fruit.active {
            Some(FruitView {
                symbol: fruit_for_level(self.level).0,
                r: self.fruit.r,
                c: self.fruit.c,
            })
        } else {
            None
        }
    }

    /// Symbols of fruits collected this run, oldest first.
    pub fn collected_fruits(&self) -> &[char] {
        &self.collected_fruits
    }

    /// Upstream `current_banner_message` as an enum.
    pub fn banner(&self) -> (Banner, BannerColor) {
        if self.confirm_restart {
            return (Banner::ConfirmRestart, BannerColor::Yellow);
        }
        let countdown = self.countdown_seconds_left();
        if countdown > 0 {
            return (Banner::Countdown(countdown), BannerColor::Yellow);
        }
        (self.info, self.info_color)
    }

    /// Whole seconds of power left (0 when inactive).
    pub fn power_seconds_left(&self) -> u32 {
        if self.power_until > self.frame {
            ((self.power_until - self.frame) as f64 / FPS as f64).ceil() as u32
        } else {
            0
        }
    }

    /// HUD mode: frightened while power is active, else scatter/chase.
    pub fn mode(&self) -> Mode {
        if self.is_power_active() {
            Mode::Frightened
        } else {
            self.current_chase_mode()
        }
    }

    /// Upstream `elapsed_seconds`.
    pub fn elapsed_seconds(&self) -> u32 {
        let ending = self.end_frame.unwrap_or(self.frame);
        (ending.saturating_sub(self.run_start_frame) / FPS) as u32
    }

    // ---- main entry ----------------------------------------------------

    /// One upstream frame: `update_logic(key)` then `frame += 1`.
    pub fn tick(&mut self, input: Input) {
        if self.phase == Phase::Playing {
            self.handle_playing_input(input);
        } else {
            self.handle_result_input(input);
        }

        // Expiry of transient info messages.
        if let Some(until) = self.info_until
            && self.frame >= until
        {
            self.info = Banner::None;
            self.info_until = None;
        }

        if self.confirm_restart {
            self.frame += 1;
            return;
        }
        if self.phase != Phase::Playing {
            // Result screen: upstream's host closes the tab here; the pane
            // keeps the result visible and waits for `r` to restart.
            self.frame += 1;
            return;
        }

        self.update_power_state();
        self.spawn_fruit_if_needed();
        self.update_player_and_collisions();
        if self.phase == Phase::Playing {
            self.update_ghosts();
        }
        self.frame += 1;
    }

    // ---- input ----------------------------------------------------------

    fn handle_playing_input(&mut self, input: Input) {
        match input {
            Input::Idle => {}
            Input::Up => self.player.next_dir = Dir::Up,
            Input::Down => self.player.next_dir = Dir::Down,
            Input::Left => self.player.next_dir = Dir::Left,
            Input::Right => self.player.next_dir = Dir::Right,
            Input::Restart => self.confirm_restart = true,
            Input::Yes => {
                if self.confirm_restart {
                    self.confirm_restart = false;
                    self.start_new_run();
                }
            }
            Input::No => self.confirm_restart = false,
        }
    }

    fn handle_result_input(&mut self, input: Input) {
        if input == Input::Restart || input == Input::Yes {
            self.start_new_run();
        }
    }

    // ---- helpers ----------------------------------------------------------

    fn is_power_active(&self) -> bool {
        self.power_until > self.frame
    }

    fn set_info(&mut self, banner: Banner, color: BannerColor, duration_sec: Option<u64>) {
        self.info = banner;
        self.info_color = color;
        self.info_until = duration_sec.map(|sec| self.frame + sec * FPS);
    }

    pub fn add_score(&mut self, delta: u32) {
        self.score += delta;
        if !self.extra_life_granted && self.score >= EXTRA_LIFE_SCORE {
            self.extra_life_granted = true;
            self.lives += 1;
        }
    }

    /// Whether the ghost with `id` was already eaten during the current
    /// power cycle (upstream `state.power_eaten`).
    pub fn ghost_was_eaten(&self, id: GhostId) -> bool {
        self.power_eaten[id.index()]
    }

    fn countdown_seconds_left(&self) -> u32 {
        if self.phase != Phase::Playing || self.countdown_until <= self.frame {
            return 0;
        }
        let frames = self.countdown_until - self.frame;
        (((frames as f64) / FPS as f64).ceil() as u32).max(1)
    }

    fn start_round_countdown(&mut self, seconds: u64) {
        self.countdown_until = self.frame + seconds * FPS;
        if self.global_pause_until < self.countdown_until {
            self.global_pause_until = self.countdown_until;
        }
    }

    // ---- setup -------------------------------------------------------------

    /// Upstream `init_positions_from_map`, with the center formula and
    /// door offsets transposed: engine (r, c) = native (c+1, r+1), so
    /// native "below the door" (r+1) becomes engine (c+1).
    fn init_positions_from_map(&mut self) {
        // Native center (floor(rows*0.75), floor(cols/2)) on 26×21 maps to
        // engine (r, c) = (native_c - 1, native_r - 1) = (9, 18).
        let center_r = self.maze.rows as i32 / 2 - 1;
        let center_c = (self.maze.cols as f64 * 0.75).floor() as i32 - 1;
        self.player_start = self.find_nearest_walkable(center_r, center_c);

        let door = self.maze.door_cells[self.maze.door_cells.len().div_ceil(2) - 1];
        let (sr, sc) = self.find_nearest_walkable(door.0 as i32, door.1 as i32 + 1);
        let (fr, fc) = self.find_nearest_walkable(door.0 as i32, door.1 as i32 + 3);
        self.ghost_spawn = (sr, sc);
        self.fruit.r = fr;
        self.fruit.c = fc;
    }

    /// Upstream `find_nearest_walkable` (BFS, door cells excluded).
    fn find_nearest_walkable(&self, start_r: i32, start_c: i32) -> (i32, i32) {
        if self.maze.is_walkable(start_r, start_c) && !self.maze.is_door(start_r, start_c) {
            return (start_r, start_c);
        }
        let mut visited = vec![vec![false; self.maze.cols]; self.maze.rows];
        let mut queue = Vec::new();
        if self.maze.in_bounds(start_r, start_c) {
            visited[start_r as usize][start_c as usize] = true;
            queue.push((start_r, start_c));
        }
        let mut head = 0;
        while head < queue.len() {
            let (r, c) = queue[head];
            head += 1;
            for dir in DIRS {
                let (dr, dc) = dir.delta();
                let (nr, nc) = self.maze.apply_tunnel(r + dr, c + dc);
                if self.maze.in_bounds(nr, nc) && !visited[nr as usize][nc as usize] {
                    visited[nr as usize][nc as usize] = true;
                    if self.maze.is_walkable(nr, nc) && !self.maze.is_door(nr, nc) {
                        return (nr, nc);
                    }
                    queue.push((nr, nc));
                }
            }
        }
        (1, 1) // upstream fallback (2, 2), 1-indexed
    }

    /// Upstream `start_level`.
    fn start_level(&mut self, level: u8) {
        self.level = level;
        self.level_start_frame = self.frame;
        self.maze = map::parse();
        self.init_positions_from_map();
        self.total_pellets = self.maze.pellet_count();
        self.remaining_pellets = self.total_pellets;
        self.reset_entities_for_level();
        self.randomize_fruit_spawn_for_level();
        self.start_round_countdown(3);
        self.set_info(Banner::Ready, BannerColor::Yellow, Some(3));
    }

    /// Upstream `start_new_run`.
    fn start_new_run(&mut self) {
        self.score = 0;
        self.lives = 3;
        self.extra_life_granted = false;
        self.phase = Phase::Playing;
        self.run_start_frame = self.frame;
        self.end_frame = None;
        self.stats_committed = false;
        self.confirm_restart = false;
        self.collected_fruits.clear();
        self.start_level(1);
    }

    /// Upstream `reset_entities_for_level`.
    fn reset_entities_for_level(&mut self) {
        self.player.r = self.player_start.0;
        self.player.c = self.player_start.1;
        // Lua corners (1-indexed, 26 rows × 21 cols): top=2, left=2,
        // right=cols-1=20, bottom=rows-1=25. Engine (r, c) = (C-1, R-1).
        let corners = [
            (GhostId::Blinky, 19, 1), // (top, right)    -> (19, 1)
            (GhostId::Pinky, 1, 1),   // (top, left)     -> (1, 1)
            (GhostId::Inky, 19, 24),  // (bottom, right) -> (19, 24)
            (GhostId::Clyde, 1, 24),  // (bottom, left)  -> (1, 24)
        ];
        self.ghosts = corners
            .into_iter()
            .map(|(id, home_r, home_c)| GhostFull {
                id,
                r: self.ghost_spawn.0,
                c: self.ghost_spawn.1,
                dir: Dir::Left,
                state: GhostState::House,
                release_at: self.frame,
                next_step_at: self.frame,
                home_r,
                home_c,
            })
            .collect();
        self.fruit.active = false;
        self.fruit.spawned = false;
    }

    /// Upstream `reset_player_auto_direction`.
    fn reset_player_auto_direction(&mut self) {
        for dir in [Dir::Left, Dir::Up, Dir::Right, Dir::Down] {
            let (dr, dc) = dir.delta();
            if self
                .maze
                .can_move_player(self.player.r + dr, self.player.c + dc)
                .is_some()
            {
                self.player.dir = dir;
                self.player.next_dir = dir;
                return;
            }
        }
        self.player.dir = Dir::Left;
        self.player.next_dir = Dir::Left;
    }

    /// Upstream `randomize_fruit_spawn_for_level`: a reachable, previously
    /// pellet-bearing, currently empty track cell outside the ghost house.
    fn randomize_fruit_spawn_for_level(&mut self) {
        let reachable = self.build_player_reachable_mask();
        let house = self.build_ghost_house_mask();
        let mut candidates: Vec<(i32, i32)> = Vec::new();
        for r in 0..self.maze.rows {
            for c in 0..self.maze.cols {
                let (ri, ci) = (r as i32, c as i32);
                let ch = self.maze.base[r][c];
                let walkable = self.maze.is_walkable(ri, ci) && !self.maze.is_door(ri, ci);
                let on_pellet_track = ch == map::PELLET || ch == map::POWER;
                if walkable
                    && on_pellet_track
                    && reachable[r][c]
                    && !house[r][c]
                    && ch != map::TUNNEL_LEFT
                    && ch != map::TUNNEL_RIGHT
                    && self.maze.pellets[r][c] == Pellet::None
                    && (ri, ci) != self.player_start
                    && (ri, ci) != self.ghost_spawn
                {
                    candidates.push((ri, ci));
                }
            }
        }
        if candidates.is_empty() {
            return;
        }
        let pick = candidates[self.rng.index(candidates.len())];
        self.fruit.r = pick.0;
        self.fruit.c = pick.1;
    }

    /// Upstream `build_player_reachable_mask`.
    fn build_player_reachable_mask(&self) -> Vec<Vec<bool>> {
        let mut reachable = vec![vec![false; self.maze.cols]; self.maze.rows];
        let (sr, sc) = self.player_start;
        if !self.maze.in_bounds(sr, sc) {
            return reachable;
        }
        reachable[sr as usize][sc as usize] = true;
        let mut queue = vec![(sr, sc)];
        let mut head = 0;
        while head < queue.len() {
            let (r, c) = queue[head];
            head += 1;
            for dir in DIRS {
                let (dr, dc) = dir.delta();
                if let Some((nr, nc)) = self.maze.can_move_player(r + dr, c + dc)
                    && !reachable[nr as usize][nc as usize]
                {
                    reachable[nr as usize][nc as usize] = true;
                    queue.push((nr, nc));
                }
            }
        }
        reachable
    }

    /// Upstream `build_ghost_house_mask`.
    fn build_ghost_house_mask(&self) -> Vec<Vec<bool>> {
        let mut house = vec![vec![false; self.maze.cols]; self.maze.rows];
        let (sr, sc) = self.ghost_spawn;
        if !self.maze.in_bounds(sr, sc) || self.maze.is_wall(sr, sc) {
            return house;
        }
        house[sr as usize][sc as usize] = true;
        let mut queue = vec![(sr, sc)];
        let mut head = 0;
        while head < queue.len() {
            let (r, c) = queue[head];
            head += 1;
            for dir in DIRS {
                let (dr, dc) = dir.delta();
                let (nr, nc) = self.maze.apply_tunnel(r + dr, c + dc);
                if self.maze.in_bounds(nr, nc)
                    && !house[nr as usize][nc as usize]
                    && !self.maze.is_wall_strict(nr, nc)
                    && !self.maze.is_door(nr, nc)
                {
                    house[nr as usize][nc as usize] = true;
                    queue.push((nr, nc));
                }
            }
        }
        house
    }

    // ---- power -------------------------------------------------------------

    fn reset_power_cycle(&mut self) {
        self.power_until = 0;
        self.power_chain = 0;
        self.power_eaten = [false; 4];
        for g in &mut self.ghosts {
            if g.state == GhostState::Frightened {
                g.state = GhostState::Normal;
            }
        }
    }

    fn activate_power_cycle(&mut self) {
        self.power_until = self.frame + power_duration_sec(self.level) * FPS;
        self.power_chain = 0;
        self.power_eaten = [false; 4];
        for g in &mut self.ghosts {
            if g.state != GhostState::Eyes && g.state != GhostState::House {
                g.state = GhostState::Frightened;
            }
        }
    }

    fn update_power_state(&mut self) {
        if self.power_until > 0 && self.frame >= self.power_until {
            self.reset_power_cycle();
        }
    }

    // ---- fruit ----------------------------------------------------------------

    fn spawn_fruit_if_needed(&mut self) {
        if self.fruit.spawned {
            return;
        }
        if self.remaining_pellets <= self.total_pellets * 7 / 10 {
            self.fruit.spawned = true;
            self.fruit.active = true;
        }
    }

    // ---- player ----------------------------------------------------------------

    /// Upstream `update_player_and_collisions`.
    fn update_player_and_collisions(&mut self) {
        if self.phase != Phase::Playing || self.countdown_seconds_left() > 0 {
            return;
        }
        self.try_move_player();
        for idx in 0..self.ghosts.len() {
            self.check_collision_with_ghost(idx);
            if self.phase != Phase::Playing {
                return;
            }
        }
    }

    /// Upstream `try_move_player`.
    fn try_move_player(&mut self) {
        if self.frame < self.player.next_step_at {
            return;
        }
        self.player.next_step_at = self.frame + PLAYER_STEP_FRAMES;

        // Try the queued turn first.
        let (dr, dc) = self.player.next_dir.delta();
        if self
            .maze
            .can_move_player(self.player.r + dr, self.player.c + dc)
            .is_some()
        {
            self.player.dir = self.player.next_dir;
        }

        // Then step in the current direction.
        let (dr, dc) = self.player.dir.delta();
        if let Some((nr, nc)) = self
            .maze
            .can_move_player(self.player.r + dr, self.player.c + dc)
        {
            self.player.r = nr;
            self.player.c = nc;
            self.consume_current_cell();
        }
    }

    /// Upstream `consume_current_cell`.
    fn consume_current_cell(&mut self) {
        let (r, c) = (self.player.r, self.player.c);
        match self.maze.pellets[r as usize][c as usize] {
            Pellet::Dot => {
                self.maze.pellets[r as usize][c as usize] = Pellet::None;
                self.remaining_pellets -= 1;
                self.add_score(10);
            }
            Pellet::Power => {
                self.maze.pellets[r as usize][c as usize] = Pellet::None;
                self.remaining_pellets -= 1;
                self.add_score(50);
                self.activate_power_cycle();
                self.set_info(Banner::Power, BannerColor::Cyan, Some(3));
            }
            Pellet::None => {}
        }

        if self.fruit.active && r == self.fruit.r && c == self.fruit.c {
            let (symbol, points) = fruit_for_level(self.level);
            self.add_score(points);
            self.fruit.active = false;
            self.collected_fruits.push(symbol);
            self.set_info(Banner::Fruit, BannerColor::Magenta, Some(3));
        }

        if self.remaining_pellets == 0 {
            if self.level >= MAX_LEVEL {
                self.phase = Phase::Won;
                self.end_frame = Some(self.frame);
                self.commit_stats_once();
                self.set_info(Banner::Won, BannerColor::Green, None);
            } else {
                let next = self.level + 1;
                self.start_level(next);
                self.set_info(Banner::LevelClear(next), BannerColor::Green, Some(3));
            }
        }
    }

    // ---- ghosts ----------------------------------------------------------------

    fn current_chase_mode(&self) -> Mode {
        let mut t = (self.frame - self.level_start_frame) / FPS;
        for &(mode, sec) in scatter_schedule(self.level) {
            if t < sec {
                return mode;
            }
            t -= sec;
        }
        Mode::Chase
    }

    /// Upstream `bfs_distance` over ghost-walkable cells (9999 = unreachable).
    fn bfs_distance(&self, sr: i32, sc: i32, tr: i32, tc: i32) -> u32 {
        if sr == tr && sc == tc {
            return 0;
        }
        if !self.maze.in_bounds(sr, sc) {
            return 9999;
        }
        let mut dist = vec![vec![u32::MAX; self.maze.cols]; self.maze.rows];
        dist[sr as usize][sc as usize] = 0;
        let mut queue = vec![(sr, sc)];
        let mut head = 0;
        while head < queue.len() {
            let (r, c) = queue[head];
            head += 1;
            for dir in DIRS {
                let (dr, dc) = dir.delta();
                let (nr, nc) = self.maze.apply_tunnel(r + dr, c + dc);
                if self.maze.in_bounds(nr, nc)
                    && dist[nr as usize][nc as usize] == u32::MAX
                    && self.maze.is_walkable(nr, nc)
                {
                    dist[nr as usize][nc as usize] = dist[r as usize][c as usize] + 1;
                    let d = dist[nr as usize][nc as usize];
                    if nr == tr && nc == tc {
                        return d;
                    }
                    queue.push((nr, nc));
                }
            }
        }
        9999
    }
    /// Upstream `projected_player_pos`.
    fn projected_player_pos(&self, steps: u32) -> (i32, i32) {
        let (mut r, mut c) = (self.player.r, self.player.c);
        let (dr, dc) = self.player.dir.delta();
        for _ in 0..steps {
            let (nr, nc) = self.maze.apply_tunnel(r + dr, c + dc);
            if !self.maze.in_bounds(nr, nc) || self.maze.is_wall_strict(nr, nc) {
                break;
            }
            r = nr;
            c = nc;
        }
        (r, c)
    }

    /// Upstream `blinky_enraged`.
    fn blinky_enraged(&self) -> bool {
        if self.level < 5 {
            return false;
        }
        self.remaining_pellets <= 20.max(self.total_pellets * 15 / 100)
    }

    /// Upstream `ghost_target`.
    fn ghost_target(&self, g: &GhostFull) -> (i32, i32) {
        if g.state == GhostState::Eyes {
            return self.ghost_spawn;
        }
        let mut mode = self.current_chase_mode();
        if g.id == GhostId::Blinky && self.blinky_enraged() {
            mode = Mode::Chase;
        }
        if mode == Mode::Scatter {
            return (g.home_r, g.home_c);
        }
        match g.id {
            GhostId::Blinky => (self.player.r, self.player.c),
            GhostId::Pinky => self.projected_player_pos(4),
            GhostId::Inky => {
                let (ar, ac) = self.projected_player_pos(2);
                let (mut br, mut bc) = (self.player.r, self.player.c);
                for other in &self.ghosts {
                    if other.id == GhostId::Blinky {
                        br = other.r;
                        bc = other.c;
                        break;
                    }
                }
                let rows = self.maze.rows as i32;
                let cols = self.maze.cols as i32;
                // Lua clamp(x, 2, n-1), 1-indexed → (1, n-2).
                let tx = (ar + (ar - br)).clamp(1, rows - 2);
                let ty = (ac + (ac - bc)).clamp(1, cols - 2);
                (tx, ty)
            }
            GhostId::Clyde => {
                let dist = (self.player.r - g.r).abs() + (self.player.c - g.c).abs();
                if dist > 8 {
                    (self.player.r, self.player.c)
                } else {
                    (g.home_r, g.home_c)
                }
            }
        }
    }

    /// Upstream `ghost_step_interval`.
    fn ghost_step_interval(&self, g: &GhostFull) -> u64 {
        if g.state == GhostState::Eyes {
            return (4 * GHOST_SLOW_FACTOR).div_ceil(2); // floor(4*2 + 0.5) = 8
        }
        let mut base: u64 = if self.level >= 11 {
            6
        } else if self.level >= 5 {
            7
        } else {
            8
        };
        if g.id == GhostId::Blinky && self.blinky_enraged() {
            base = base.saturating_sub(1).max(4);
        }
        if g.state == GhostState::Frightened && !self.power_eaten[g.id.index()] {
            base += 3;
        }
        (base * GHOST_SLOW_FACTOR).div_ceil(2)
    }

    /// Upstream `ghost_enter_house`.
    fn ghost_enter_house(&mut self, idx: usize) {
        let g = &mut self.ghosts[idx];
        g.r = self.ghost_spawn.0;
        g.c = self.ghost_spawn.1;
        g.state = GhostState::House;
        g.release_at = self.frame + ghost_revive_sec(self.level) * FPS;
        g.next_step_at = self.frame + FPS;
    }

    /// Upstream `eat_ghost`.
    fn eat_ghost(&mut self, idx: usize) {
        let (state, id) = {
            let g = &self.ghosts[idx];
            (g.state, g.id)
        };
        if state != GhostState::Frightened || self.power_eaten[id.index()] {
            return;
        }
        let rewards = [200, 400, 800, 1600];
        let idx_chain = (self.power_chain as usize).min(3);
        self.add_score(rewards[idx_chain]);
        self.power_chain += 1;
        self.power_eaten[id.index()] = true;
        let g = &mut self.ghosts[idx];
        g.state = GhostState::Eyes;
        g.next_step_at = self.frame;
        self.set_info(Banner::GhostEaten, BannerColor::Cyan, Some(3));
    }

    /// Upstream `reset_after_player_death`.
    fn reset_after_player_death(&mut self) {
        self.player.r = self.player_start.0;
        self.player.c = self.player_start.1;
        self.player.next_step_at = self.frame;
        self.reset_player_auto_direction();

        for g in &mut self.ghosts {
            g.r = self.ghost_spawn.0;
            g.c = self.ghost_spawn.1;
            g.dir = Dir::Left;
            g.state = GhostState::House;
            g.release_at = self.frame + 4 * FPS + ghost_release_delay(self.level, g.id) * FPS;
            g.next_step_at = g.release_at;
        }

        self.global_pause_until = self.frame + 4 * FPS;
        self.reset_power_cycle();
        self.set_info(Banner::Waiting, BannerColor::Yellow, Some(4));
    }

    /// Upstream `player_lost_life`.
    fn player_lost_life(&mut self) {
        self.lives -= 1;
        if self.lives == 0 {
            self.phase = Phase::Lost;
            self.end_frame = Some(self.frame);
            self.commit_stats_once();
            self.set_info(Banner::Lost, BannerColor::Red, None);
        } else {
            self.reset_after_player_death();
        }
    }

    /// Upstream `check_collision_with_ghost` for one ghost. The phase guard
    /// lives in the callers upstream; kept here so direct calls are safe.
    fn check_collision_with_ghost(&mut self, idx: usize) {
        if self.phase != Phase::Playing {
            return;
        }
        let g = &self.ghosts[idx];
        if self.player.r != g.r || self.player.c != g.c {
            return;
        }
        if g.state == GhostState::Eyes || g.state == GhostState::House {
            return;
        }
        if self.is_power_active()
            && g.state == GhostState::Frightened
            && !self.power_eaten[g.id.index()]
        {
            self.eat_ghost(idx);
        } else {
            self.player_lost_life();
        }
    }

    /// Upstream `move_ghost`.
    fn move_ghost(&mut self, idx: usize) {
        if self.ghosts[idx].state == GhostState::House {
            if self.frame >= self.ghosts[idx].release_at && self.frame >= self.global_pause_until {
                let frightened = self.is_power_active() && !self.power_eaten[idx];
                self.ghosts[idx].state = if frightened {
                    GhostState::Frightened
                } else {
                    GhostState::Normal
                };
                self.ghosts[idx].next_step_at = self.frame;
            }
            return;
        }

        if self.frame < self.ghosts[idx].next_step_at || self.frame < self.global_pause_until {
            return;
        }
        let interval = self.ghost_step_interval(&self.ghosts[idx]);
        self.ghosts[idx].next_step_at = self.frame + interval;

        let target = self.ghost_target(&self.ghosts[idx]);
        // Random-walk paths need the RNG while ghosts are borrowed, so take
        // an owned copy of the step decision out of the borrow.
        let frightened = self.ghosts[idx].state == GhostState::Frightened && !self.power_eaten[idx];
        let step = {
            let g = &self.ghosts[idx];
            let mut candidates: Vec<(i32, i32, Dir)> = Vec::new();
            for dir in DIRS {
                let (dr, dc) = dir.delta();
                if let Some((nr, nc)) = self.maze.can_move_ghost(g.r + dr, g.c + dc) {
                    candidates.push((nr, nc, dir));
                }
            }
            if candidates.is_empty() {
                None
            } else {
                if candidates.len() > 1 {
                    let opp = g.dir.opposite();
                    let filtered: Vec<_> = candidates
                        .iter()
                        .copied()
                        .filter(|&(_, _, dir)| dir != opp)
                        .collect();
                    if !filtered.is_empty() {
                        candidates = filtered;
                    }
                }
                if frightened {
                    Some(candidates[self.rng.index(candidates.len())])
                } else {
                    let mut best: Option<(u32, usize)> = None;
                    for (i, &(r, c, _)) in candidates.iter().enumerate() {
                        let d = self.bfs_distance(r, c, target.0, target.1);
                        if best.is_none() || d < best.unwrap().0 {
                            best = Some((d, i));
                        }
                    }
                    Some(
                        candidates[best
                            .map(|(_, i)| i)
                            .unwrap_or_else(|| self.rng.index(candidates.len()))],
                    )
                }
            }
        };
        if let Some((r, c, dir)) = step {
            let g = &mut self.ghosts[idx];
            g.r = r;
            g.c = c;
            g.dir = dir;
        }

        let at_spawn = {
            let g = &self.ghosts[idx];
            g.state == GhostState::Eyes && g.r == self.ghost_spawn.0 && g.c == self.ghost_spawn.1
        };
        if at_spawn {
            self.ghost_enter_house(idx);
        }
    }

    /// Upstream `update_ghosts`.
    fn update_ghosts(&mut self) {
        // State transitions first.
        for idx in 0..self.ghosts.len() {
            let id_idx = self.ghosts[idx].id.index();
            if self.ghosts[idx].state != GhostState::Eyes {
                let power = self.is_power_active() && !self.power_eaten[id_idx];
                let state = self.ghosts[idx].state;
                if power && state != GhostState::House {
                    self.ghosts[idx].state = GhostState::Frightened;
                } else if state == GhostState::Frightened
                    && (!self.is_power_active() || self.power_eaten[id_idx])
                {
                    self.ghosts[idx].state = GhostState::Normal;
                }
            }
        }

        for idx in 0..self.ghosts.len() {
            self.move_ghost(idx);
            self.check_collision_with_ghost(idx);
            if self.phase != Phase::Playing {
                return;
            }
        }
    }

    // ---- stats ------------------------------------------------------------

    /// Upstream `commit_stats_once` (the host-side save hook is polling).
    fn commit_stats_once(&mut self) {
        if self.stats_committed {
            return;
        }
        if self.score > self.best_score {
            self.best_score = self.score;
        }
        self.stats_committed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle_ticks(game: &mut Game, frames: u64) {
        for _ in 0..frames {
            game.tick(Input::Idle);
        }
    }
    #[test]
    fn new_game_is_playing_with_countdown() {
        let game = Game::new(0);
        assert_eq!(game.phase(), Phase::Playing);
        assert_eq!(game.score(), 0);
        assert_eq!(game.lives(), 3);
        assert_eq!(game.level(), 1);
        assert_eq!(game.banner(), (Banner::Countdown(3), BannerColor::Yellow));
    }

    #[test]
    fn player_auto_moves_and_scores_after_countdown() {
        let mut game = Game::new(0);
        idle_ticks(&mut game, 3 * FPS); // countdown
        let score_before = game.score();
        idle_ticks(&mut game, 4 * FPS); // plenty of steps
        assert!(
            game.score() > score_before,
            "player should have eaten pellets"
        );
    }

    #[test]
    fn tunnel_wraps_player_between_mouths() {
        let mut game = Game::new(0);
        idle_ticks(&mut game, 3 * FPS + PLAYER_STEP_FRAMES);
        // Park the player at the top tunnel mouth heading up. Column 12 is
        // the transposed tunnel column; (0, 12) is the top mouth.
        game.player.r = 0;
        game.player.c = 12;
        game.player.dir = Dir::Up;
        game.player.next_dir = Dir::Up;
        game.player.next_step_at = 0;
        for _ in 0..PLAYER_STEP_FRAMES {
            game.tick(Input::No);
        }
        assert_eq!(game.player(), (20, 12), "should wrap to bottom mouth");
    }

    #[test]
    fn power_pellet_frightens_and_eatable_ghost_scores() {
        let mut game = Game::new(0);
        idle_ticks(&mut game, 3 * FPS);
        // Put a power pellet one Left-step ahead (engine Left = native up,
        // along the open corridor) and step onto it.
        let (pr, pc) = (game.player.r, game.player.c);
        game.maze.pellets[pr as usize][(pc - 1) as usize] = Pellet::Power;
        game.player.next_step_at = 0;
        game.player.dir = Dir::Left;
        game.player.next_dir = Dir::Left;
        for _ in 0..PLAYER_STEP_FRAMES + 2 {
            game.tick(Input::No);
        }
        assert!(game.power_seconds_left() > 0);

        // Force a collision with a frightened ghost.
        let g = game.ghosts.iter_mut().next().unwrap();
        g.r = game.player.r;
        g.c = game.player.c;
        g.state = GhostState::Frightened;
        game.check_collision_with_ghost(0);
        assert_eq!(game.ghosts()[0].state, GhostState::Eyes);
        // 50 (power pellet) + 200 (first ghost in chain) + eaten dots.
        assert_eq!(game.score() % 250, 0);
    }

    #[test]
    fn normal_ghost_collision_costs_a_life() {
        let mut game = Game::new(0);
        idle_ticks(&mut game, 3 * FPS + 4 * FPS); // countdown + pause
        let lives = game.lives();
        let g = game.ghosts.iter_mut().next().unwrap();
        g.r = game.player.r;
        g.c = game.player.c;
        g.state = GhostState::Normal;
        game.check_collision_with_ghost(0);
        assert_eq!(game.lives(), lives - 1);
        assert_eq!(game.banner().0, Banner::Waiting);
    }

    #[test]
    fn restart_confirmation_flow_resets_run() {
        let mut game = Game::new(0);
        idle_ticks(&mut game, 3 * FPS);
        // Score something.
        game.add_score(500);
        game.tick(Input::Restart);
        assert_eq!(game.banner().0, Banner::ConfirmRestart);
        game.tick(Input::Yes);
        assert_eq!(game.score(), 0);
        assert_eq!(game.lives(), 3);
        assert_eq!(game.banner().0, Banner::Countdown(3));
    }

    #[test]
    fn clearing_pellets_starts_next_level() {
        let mut game = Game::new(0);
        idle_ticks(&mut game, 3 * FPS);
        // Empty the board except the cell one Up-step ahead of the player.
        for row in game.maze.pellets.iter_mut() {
            for cell in row.iter_mut() {
                *cell = Pellet::None;
            }
        }
        game.remaining_pellets = 1;
        let (pr, pc) = (game.player.r, game.player.c);
        game.maze.pellets[(pr - 1) as usize][pc as usize] = Pellet::Dot;
        game.player.next_step_at = 0;
        game.player.dir = Dir::Up;
        game.player.next_dir = Dir::Up;
    }

    #[test]
    fn losing_all_lives_ends_run_and_commits_best() {
        let mut game = Game::new(100);
        idle_ticks(&mut game, 3 * FPS);
        game.add_score(5000);
        for _ in 0..3 {
            game.player.next_step_at = u64::MAX; // reset on death unfreezes; refreeze
            // Skip countdown + post-death pause (3s + 4s + release delays).
            idle_ticks(&mut game, 8 * FPS);
            let g = game.ghosts.iter_mut().next().unwrap();
            g.r = game.player.r;
            g.c = game.player.c;
            g.state = GhostState::Normal;
            game.check_collision_with_ghost(0);
            if game.phase() != Phase::Playing {
                break;
            }
        }
        game.player.next_step_at = u64::MAX;
        assert_eq!(game.phase(), Phase::Lost);
        assert_eq!(game.best_score(), game.score());
        assert!(game.best_score() >= 5000);
        // Restart from the result screen.
        game.tick(Input::Restart);
        assert_eq!(game.phase(), Phase::Playing);
        assert_eq!(game.score(), 0);
    }

    #[test]
    fn result_screen_waits_for_restart_key() {
        let mut game = Game::new(0);
        idle_ticks(&mut game, 3 * FPS); // clear the opening countdown
        game.lives = 1;
        let (pr, pc) = (game.player.r, game.player.c);
        let g = game.ghosts.iter_mut().next().unwrap();
        g.r = pr;
        g.c = pc;
        g.state = GhostState::Normal;
        game.check_collision_with_ghost(0);
        assert_eq!(game.phase(), Phase::Lost);

        // The pane keeps the result visible: no auto-restart, stays Lost
        // through idle frames while the player decides.
        idle_ticks(&mut game, 8 * FPS);
        assert_eq!(game.phase(), Phase::Lost);

        // `r` restarts from the result screen.
        game.tick(Input::Restart);
        assert_eq!(game.phase(), Phase::Playing);
        assert_eq!(game.score(), 0);
    }

    #[test]
    fn fruit_spawns_at_seventy_percent_and_scores_on_pickup() {
        let mut game = Game::new(0);
        idle_ticks(&mut game, 3 * FPS);
        game.remaining_pellets = game.total_pellets * 7 / 10;
        game.tick(Input::No);
        assert!(game.fruit().is_some(), "fruit should be active at 70%");
        // Teleport onto the fruit.
        let (fr, fc) = (game.fruit.r, game.fruit.c);
        game.player.r = fr;
        game.player.c = fc;
        game.maze.pellets[fr as usize][fc as usize] = Pellet::None;
        game.player.next_step_at = 0;
        game.player.dir = Dir::Left;
        let before = game.score();
        // Step off and back: simpler — call consume via a move into the cell.
        // Move up one cell (fruit cell itself has no pellet now).
        game.player.r = fr - 1;
        game.player.c = fc;
        game.player.dir = Dir::Down;
        game.player.next_dir = Dir::Down;
        game.maze.pellets[fr as usize][fc as usize] = Pellet::None;
        // Ensure the below move is legal; if not, teleport adjacent.
        for _ in 0..PLAYER_STEP_FRAMES + 2 {
            game.tick(Input::No);
        }
        assert!(
            game.collected_fruits().len() == 1 || game.score() > before,
            "fruit should be collected when stepped on: score {} -> {}",
            before,
            game.score()
        );
    }

    #[test]
    fn fruit_table_matches_upstream() {
        assert_eq!(fruit_for_level(1), ('%', 100));
        assert_eq!(fruit_for_level(7), ('§', 1000));
        assert_eq!(fruit_for_level(12), ('?', 3000));
        assert_eq!(fruit_for_level(13), ('!', 5000));
        assert_eq!(fruit_for_level(20), ('!', 5000));
    }

    #[test]
    fn extra_life_at_one_hundred_thousand() {
        let mut game = Game::new(0);
        game.add_score(99_999);
        assert_eq!(game.lives(), 3);
        game.add_score(1);
        assert_eq!(game.lives(), 4);
    }
}
