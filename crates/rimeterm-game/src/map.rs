//! Maze template + parsing, ported from tui-game's `pacman.lua`.
//!
//! The template below is the upstream maze verbatim (26 rows × 21 cols).
//! [`parse`] transposes it into the engine orientation (21 rows × 26 cols)
//! so the board fits rimeterm's wide, short left-bottom pane; transposition
//! preserves the maze graph, so gameplay is unchanged.

/// Ordinary pellet character.
pub const PELLET: char = '·';
/// Power pellet character.
pub const POWER: char = '*';
/// Ghost-house door character.
pub const DOOR: char = '-';
/// Left tunnel mouth marker in the template.
pub const TUNNEL_LEFT: char = '<';
/// Right tunnel mouth marker in the template.
pub const TUNNEL_RIGHT: char = '>';

/// Characters that count as walls (upstream `WALL_SET`).
pub const WALL_CHARS: [char; 10] = ['╔', '╗', '╚', '╝', '═', '║', '╦', '╩', '╠', '╣'];

/// Native upstream maze, 26 rows × 21 cols.
const NATIVE_TEMPLATE: [&str; 26] = [
    "╔═════════╦═════════╗",
    "║·········║·········║",
    "║·╔═╗·╔═╗·║·╔═╗·╔═╗·║",
    "║*║ ║·║ ║·║·║ ║·║ ║*║",
    "║·╚═╝·╚═╝·║·╚═╝·╚═╝·║",
    "║···················║",
    "║·╔═╗·║·╔═══╗·║·╔═╗·║",
    "║·╚═╝·║·╚═╦═╝·║·╚═╝·║",
    "║·····║···║···║·····║",
    "╚═══╗·╠══ ║ ══╣·╔═══╝",
    "    ║·║       ║·║    ",
    "════╝·║ ╔═-═╗ ║·╚════",
    "<    ·  ║   ║  ·    >",
    "════╗·║ ╚═══╝ ║·╔════",
    "    ║·║       ║·║    ",
    "    ║·║ ╔═══╗ ║·║    ",
    "╔═══╝·║ ╚═╦═╝ ║·╚═══╗",
    "║·········║·········║",
    "║·══╗·═══·║·═══·╔══·║",
    "║*··║···········║··*║",
    "╠═╗·║·║·╔═══╗·║·║·╔═╣",
    "╠═╝·║·║·╚═╦═╝·║·║·╚═╣",
    "║·····║···║···║·····║",
    "║·════╩══·║·══╩════·║",
    "║···················║",
    "╚═══════════════════╝",
];

/// Pellet state of one cell, kept separate from the base maze like the
/// upstream `state.pellets` matrix.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pellet {
    None,
    Dot,
    Power,
}

/// Parsed maze in engine orientation (transposed: 21 rows × 26 cols).
#[derive(Clone, Debug)]
pub struct Maze {
    pub rows: usize,
    pub cols: usize,
    /// Base characters: walls, door, tunnel mouths, pellet-track marks.
    pub base: Vec<Vec<char>>,
    /// Pellet state per cell (both track kinds live here, not in `base`).
    pub pellets: Vec<Vec<Pellet>>,
    /// Ghost-house door cells.
    pub door_cells: Vec<(usize, usize)>,
    /// Both tunnel mouths; the maze always has exactly two.
    pub tunnel_left: (usize, usize),
    pub tunnel_right: (usize, usize),
}

/// Parse [`NATIVE_TEMPLATE`] into the transposed [`Maze`].
///
/// Mirrors upstream `parse_map`: pellet-track characters stay visible in
/// `base`, pellets are seeded from them, door and tunnel cells recorded.
pub fn parse() -> Maze {
    let native: Vec<Vec<char>> = NATIVE_TEMPLATE
        .iter()
        .map(|row| row.chars().collect::<Vec<_>>())
        .collect();
    // engine[r][c] = native[c][r]; short native rows pad with spaces.
    let engine = |r: usize, c: usize| -> char {
        native
            .get(c)
            .and_then(|row| row.get(r))
            .copied()
            .unwrap_or(' ')
    };

    let rows = NATIVE_TEMPLATE
        .iter()
        .map(|row| row.chars().count())
        .max()
        .unwrap_or(0);
    let cols = native.len();

    let mut maze = Maze {
        rows,
        cols,
        base: vec![vec![' '; cols]; rows],
        pellets: vec![vec![Pellet::None; cols]; rows],
        door_cells: Vec::new(),
        tunnel_left: (0, 0),
        tunnel_right: (0, 0),
    };

    for r in 0..rows {
        for c in 0..cols {
            let ch = engine(r, c);
            maze.base[r][c] = ch;
            maze.pellets[r][c] = match ch {
                PELLET => Pellet::Dot,
                POWER => Pellet::Power,
                _ => Pellet::None,
            };
            match ch {
                DOOR => maze.door_cells.push((r, c)),
                TUNNEL_LEFT => maze.tunnel_left = (r, c),
                TUNNEL_RIGHT => maze.tunnel_right = (r, c),
                _ => {}
            }
        }
    }
    maze
}

impl Maze {
    /// Upstream `in_bounds` on the engine grid.
    pub fn in_bounds(&self, r: i32, c: i32) -> bool {
        r >= 0 && c >= 0 && (r as usize) < self.rows && (c as usize) < self.cols
    }

    /// Upstream `is_wall`: out of bounds counts as wall.
    pub fn is_wall(&self, r: i32, c: i32) -> bool {
        !self.in_bounds(r, c) || self.is_wall_strict(r, c)
    }

    /// Upstream `is_door`.
    pub fn is_door(&self, r: i32, c: i32) -> bool {
        self.in_bounds(r, c) && self.base[r as usize][c as usize] == DOOR
    }

    /// Upstream `is_walkable` (non-wall, in bounds).
    pub fn is_walkable(&self, r: i32, c: i32) -> bool {
        self.in_bounds(r, c) && !self.is_wall_strict(r, c)
    }

    /// Wall test without the out-of-bounds-as-wall collapse. Caller must
    /// keep coordinates in bounds.
    pub fn is_wall_strict(&self, r: i32, c: i32) -> bool {
        WALL_CHARS.contains(&self.base[r as usize][c as usize])
    }

    /// Upstream `apply_tunnel`, transposed: the tunnel mouths sit in one
    /// column, so wrapping triggers when the row leaves the grid.
    pub fn apply_tunnel(&self, r: i32, c: i32) -> (i32, i32) {
        let (lr, lc) = self.tunnel_left;
        let (rr, rc) = self.tunnel_right;
        if c == lc as i32 && r < 0 {
            return (rr as i32, rc as i32);
        }
        if c == rc as i32 && r >= self.rows as i32 {
            return (lr as i32, lc as i32);
        }
        (r, c)
    }

    /// Upstream `can_move_player`: walkable and not the house door.
    pub fn can_move_player(&self, r: i32, c: i32) -> Option<(i32, i32)> {
        let (r, c) = self.apply_tunnel(r, c);
        if !self.in_bounds(r, c) || self.is_wall_strict(r, c) || self.is_door(r, c) {
            None
        } else {
            Some((r, c))
        }
    }

    /// Upstream `can_move_ghost`: walkable; doors allowed.
    pub fn can_move_ghost(&self, r: i32, c: i32) -> Option<(i32, i32)> {
        let (r, c) = self.apply_tunnel(r, c);
        if !self.in_bounds(r, c) || self.is_wall_strict(r, c) {
            None
        } else {
            Some((r, c))
        }
    }

    /// Count dots + power pellets currently on the board.
    pub fn pellet_count(&self) -> u32 {
        self.pellets
            .iter()
            .flat_map(|row| row.iter())
            .filter(|p| !matches!(p, Pellet::None))
            .count() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_transposed_dimensions() {
        let maze = parse();
        assert_eq!(maze.rows, 21);
        assert_eq!(maze.cols, 26);
    }

    #[test]
    fn seeds_one_hundred_ninety_one_pellets() {
        // 187 dots + 4 power pellets counted from the upstream template.
        assert_eq!(parse().pellet_count(), 191);
    }

    #[test]
    fn records_door_and_tunnels() {
        let maze = parse();
        assert_eq!(maze.door_cells, vec![(10, 11)]);
        assert_eq!(maze.tunnel_left, (0, 12));
        assert_eq!(maze.tunnel_right, (20, 12));
    }

    #[test]
    fn tunnel_wraps_both_directions() {
        let maze = parse();
        assert_eq!(maze.apply_tunnel(-1, 12), (20, 12));
        assert_eq!(maze.apply_tunnel(21, 12), (0, 12));
        assert_eq!(maze.apply_tunnel(5, -1), (5, -1));
    }

    #[test]
    fn players_cannot_cross_doors_but_ghosts_can() {
        let maze = parse();
        // The door cell itself: ghosts pass, players don't.
        assert!(maze.can_move_ghost(10, 11).is_some());
        assert!(maze.can_move_player(10, 11).is_none());
    }
}
