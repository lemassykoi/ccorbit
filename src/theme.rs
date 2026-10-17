//! Palettes. Plain colors use the 256-color range so they work in any modern terminal;
//! gradients and the skyline use 24-bit color when the terminal says it supports it
//! (`COLORTERM=truecolor`) and the nearest 256-color cube entry otherwise.

use std::sync::OnceLock;

use ratatui::style::Color;

use crate::model::Kind;

pub type Rgb = (u8, u8, u8);

#[derive(Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    pub bg: Color,
    pub surface: Color,
    pub shadow: Color,
    pub fg: Color,
    pub hi: Color,
    pub dim: Color,
    pub faint: Color,
    pub accent: Color,
    pub sel: Color,
    pub ok: Color,
    pub warn: Color,
    pub err: Color,
    pub agent: Color,
    pub workflow: Color,
    pub skill: Color,
    pub mcp: Color,
    pub web: Color,
    pub file: Color,
    pub shell: Color,
    pub comms: Color,
    pub prompt: Color,
    pub g0: Rgb,
    pub g1: Rgb,
    pub towers: [Rgb; 5],
}

const fn c(i: u8) -> Color {
    Color::Indexed(i)
}

pub const THEMES: [Theme; 3] = [
    Theme {
        name: "phosphor",
        bg: c(233), surface: c(235), shadow: c(16),
        fg: c(77), hi: c(120), dim: c(65), faint: c(240), accent: c(214), sel: c(22),
        ok: c(46), warn: c(220), err: c(203),
        agent: c(213), workflow: c(87), skill: c(220), mcp: c(45), web: c(117), file: c(114), shell: c(150), comms: c(183), prompt: c(229),
        g0: (0, 255, 135), g1: (0, 175, 255),
        towers: [(0, 215, 135), (0, 175, 255), (175, 135, 255), (255, 175, 0), (255, 95, 135)],
    },
    Theme {
        name: "amber",
        bg: c(233), surface: c(235), shadow: c(16),
        fg: c(214), hi: c(222), dim: c(136), faint: c(240), accent: c(230), sel: c(94),
        ok: c(184), warn: c(208), err: c(196),
        agent: c(209), workflow: c(223), skill: c(226), mcp: c(180), web: c(186), file: c(215), shell: c(179), comms: c(217), prompt: c(230),
        g0: (255, 200, 0), g1: (255, 95, 95),
        towers: [(255, 175, 0), (255, 135, 95), (215, 175, 95), (255, 215, 135), (215, 95, 95)],
    },
    Theme {
        name: "neon",
        bg: c(233), surface: c(235), shadow: c(16),
        fg: c(51), hi: c(231), dim: c(61), faint: c(241), accent: c(201), sel: c(53),
        ok: c(48), warn: c(227), err: c(197),
        agent: c(207), workflow: c(141), skill: c(227), mcp: c(87), web: c(123), file: c(117), shell: c(159), comms: c(219), prompt: c(231),
        g0: (255, 0, 175), g1: (0, 215, 255),
        towers: [(255, 0, 175), (0, 215, 255), (175, 95, 255), (255, 255, 95), (0, 255, 175)],
    },
];

pub fn index(name: &str) -> Option<usize> {
    THEMES.iter().position(|t| t.name == name)
}

pub fn truecolor() -> bool {
    static TC: OnceLock<bool> = OnceLock::new();
    *TC.get_or_init(|| matches!(std::env::var("COLORTERM").as_deref(), Ok("truecolor") | Ok("24bit")))
}

pub fn rgb((r, g, b): Rgb) -> Color {
    if truecolor() {
        return Color::Rgb(r, g, b);
    }
    const LEVELS: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let lvl = |v: u8| (0..6).min_by_key(|&i| (LEVELS[i] - v as i32).abs()).unwrap_or(0) as u8;
    Color::Indexed(16 + 36 * lvl(r) + 6 * lvl(g) + lvl(b))
}

pub fn mix(a: Rgb, b: Rgb, f: f64) -> Rgb {
    let f = f.clamp(0.0, 1.0);
    let m = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * f).round() as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

/// The RGB value of any color, resolving the xterm 256-color palette.
pub fn to_rgb(c: Color) -> Rgb {
    const BASE: [Rgb; 16] = [
        (0, 0, 0), (205, 49, 49), (13, 188, 121), (229, 229, 16), (36, 114, 200), (188, 63, 188), (17, 168, 205), (229, 229, 229),
        (102, 102, 102), (241, 76, 76), (35, 209, 139), (245, 245, 67), (59, 142, 234), (214, 112, 214), (41, 184, 219), (255, 255, 255),
    ];
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(i) if i < 16 => BASE[i as usize],
        Color::Indexed(i) if i < 232 => {
            let i = i - 16;
            let l = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (l(i / 36), l((i / 6) % 6), l(i % 6))
        }
        Color::Indexed(i) => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
        Color::Black => BASE[0],
        Color::Red => BASE[1],
        Color::Green => BASE[2],
        Color::Yellow => BASE[3],
        Color::Blue => BASE[4],
        Color::Magenta => BASE[5],
        Color::Cyan => BASE[6],
        Color::Gray => BASE[7],
        Color::DarkGray => BASE[8],
        Color::LightRed => BASE[9],
        Color::LightGreen => BASE[10],
        Color::LightYellow => BASE[11],
        Color::LightBlue => BASE[12],
        Color::LightMagenta => BASE[13],
        Color::LightCyan => BASE[14],
        Color::White | Color::Reset => BASE[15],
    }
}

/// `c` faded toward `bg` by `f` (0 keeps it, 1 is the background): distance in the HUD.
pub fn fog(c: Color, bg: Color, f: f64) -> Color {
    if f <= 0.02 {
        return c;
    }
    rgb(mix(to_rgb(c), to_rgb(bg), f))
}

/// Darker below 1, lighter above.
pub fn shade((r, g, b): Rgb, f: f64) -> Rgb {
    let m = |x: u8| (x as f64 * f).round().clamp(0.0, 255.0) as u8;
    (m(r), m(g), m(b))
}

impl Theme {
    pub fn kind(&self, k: Kind) -> Color {
        match k {
            Kind::Agent => self.agent,
            Kind::Workflow => self.workflow,
            Kind::Skill => self.skill,
            Kind::Mcp => self.mcp,
            Kind::Web => self.web,
            Kind::File => self.file,
            Kind::Shell => self.shell,
            Kind::Comms => self.comms,
            Kind::Tool => self.dim,
        }
    }

    /// The identity color of the session at position `k` on the wall: the same on its card, its
    /// skyline tower, its feed lines and its HUD node.
    pub fn session(&self, k: usize) -> Color {
        rgb(self.towers[k % self.towers.len()])
    }

    /// The theme gradient at `f` in 0..=1.
    pub fn grad(&self, f: f64) -> Color {
        rgb(mix(self.g0, self.g1, f))
    }
}
