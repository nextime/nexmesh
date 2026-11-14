//! Two looks, and the same tokens behind both.
//!
//! The console is a thing you stare at for hours, so it gets the same choice
//! the dashboard has: a flight deck that stays out of the way, and LCARS for
//! when you would rather be on one. The LCARS palette is the trans flag --
//! which is both the point and, as it happens, very close to canon pastels.
//!
//! Everything is truecolor. A terminal that cannot do 24-bit still renders
//! these: crossterm degrades them to the nearest it has.

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Theme {
    /// Quiet, modern, a little sci-fi at the edges.
    Deck,
    /// Black, blocks, elbows, capitals.
    Lcars,
}

impl Theme {
    pub fn name(self) -> &'static str {
        match self {
            Theme::Deck => "deck",
            Theme::Lcars => "lcars",
        }
    }

    pub fn from_name(s: &str) -> Theme {
        match s.trim() {
            "lcars" => Theme::Lcars,
            _ => Theme::Deck,
        }
    }

    pub fn next(self) -> Theme {
        match self {
            Theme::Deck => Theme::Lcars,
            Theme::Lcars => Theme::Deck,
        }
    }

    pub fn bg(self) -> Color {
        match self {
            Theme::Deck => Color::Rgb(11, 15, 20),
            Theme::Lcars => Color::Rgb(0, 0, 0),
        }
    }

    pub fn panel(self) -> Color {
        match self {
            Theme::Deck => Color::Rgb(18, 24, 32),
            Theme::Lcars => Color::Rgb(11, 11, 16),
        }
    }

    pub fn fg(self) -> Color {
        match self {
            Theme::Deck => Color::Rgb(230, 237, 243),
            Theme::Lcars => Color::Rgb(255, 255, 255),
        }
    }

    pub fn dim(self) -> Color {
        match self {
            Theme::Deck => Color::Rgb(139, 156, 176),
            Theme::Lcars => Color::Rgb(207, 216, 227),
        }
    }

    pub fn faint(self) -> Color {
        match self {
            Theme::Deck => Color::Rgb(93, 107, 125),
            Theme::Lcars => Color::Rgb(142, 154, 168),
        }
    }

    pub fn line(self) -> Color {
        match self {
            Theme::Deck => Color::Rgb(36, 48, 65),
            Theme::Lcars => Color::Rgb(42, 42, 56),
        }
    }

    /// The primary: light blue in both, which is lucky.
    pub fn accent(self) -> Color {
        match self {
            Theme::Deck => Color::Rgb(167, 139, 250),
            Theme::Lcars => Color::Rgb(91, 206, 250),
        }
    }

    /// The secondary. Pink in LCARS, and used for anything that wants
    /// attention without alarming anybody.
    pub fn second(self) -> Color {
        match self {
            Theme::Deck => Color::Rgb(147, 197, 253),
            Theme::Lcars => Color::Rgb(245, 169, 184),
        }
    }

    pub fn ok(self) -> Color {
        match self {
            Theme::Deck => Color::Rgb(110, 231, 183),
            Theme::Lcars => Color::Rgb(91, 206, 250),
        }
    }

    pub fn warn(self) -> Color {
        match self {
            Theme::Deck => Color::Rgb(252, 211, 77),
            Theme::Lcars => Color::Rgb(245, 169, 184),
        }
    }

    pub fn bad(self) -> Color {
        match self {
            Theme::Deck => Color::Rgb(252, 165, 165),
            Theme::Lcars => Color::Rgb(245, 169, 184),
        }
    }

    /// Eight stable colours for names, so the same agent is the same colour
    /// in every pane and on every run. It is what makes a busy log skimmable.
    pub fn nick(self, name: &str) -> Color {
        let mut h: u32 = 0;
        for b in name.bytes() {
            h = h.wrapping_mul(31).wrapping_add(b as u32);
        }
        let deck = [
            Color::Rgb(125, 211, 252), Color::Rgb(252, 165, 165),
            Color::Rgb(134, 239, 172), Color::Rgb(252, 211, 77),
            Color::Rgb(196, 181, 253), Color::Rgb(249, 168, 212),
            Color::Rgb(103, 232, 249), Color::Rgb(253, 186, 116),
        ];
        let lcars = [
            Color::Rgb(91, 206, 250), Color::Rgb(245, 169, 184),
            Color::Rgb(255, 255, 255), Color::Rgb(154, 217, 240),
            Color::Rgb(255, 201, 212), Color::Rgb(127, 227, 255),
            Color::Rgb(230, 179, 192), Color::Rgb(191, 233, 255),
        ];
        let set = match self {
            Theme::Deck => deck,
            Theme::Lcars => lcars,
        };
        set[(h % 8) as usize]
    }

    pub fn title(self) -> Style {
        match self {
            Theme::Deck => Style::default().fg(self.accent()).add_modifier(Modifier::BOLD),
            Theme::Lcars => Style::default().fg(self.second()).add_modifier(Modifier::BOLD),
        }
    }

    pub fn header(self) -> Style {
        Style::default().fg(self.second()).add_modifier(Modifier::BOLD)
    }

    pub fn selected(self) -> Style {
        match self {
            Theme::Deck => Style::default()
                .fg(self.fg())
                .bg(Color::Rgb(31, 41, 55))
                .add_modifier(Modifier::BOLD),
            // LCARS selects by filling the block, as its buttons do.
            Theme::Lcars => Style::default()
                .fg(Color::Rgb(0, 0, 0))
                .bg(self.accent())
                .add_modifier(Modifier::BOLD),
        }
    }

    /// How a tile reads: filled and black-on-colour in LCARS, outlined in
    /// deck. Returns (number style, label style).
    pub fn tile(self, alert: bool) -> (Style, Style) {
        match self {
            Theme::Deck => (
                Style::default()
                    .fg(if alert { self.warn() } else { self.fg() })
                    .add_modifier(Modifier::BOLD),
                Style::default().fg(self.faint()),
            ),
            Theme::Lcars => {
                let bg = if alert { self.second() } else { self.accent() };
                (
                    Style::default().fg(Color::Rgb(0, 0, 0)).bg(bg).add_modifier(Modifier::BOLD),
                    Style::default().fg(Color::Rgb(0, 0, 0)).bg(bg),
                )
            }
        }
    }

    /// LCARS writes its labels in capitals. Deck does not shout.
    pub fn label(self, s: &str) -> String {
        match self {
            Theme::Deck => s.to_string(),
            Theme::Lcars => s.to_uppercase(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_always_gets_the_same_colour() {
        // The whole value of colouring names is that it is stable: the same
        // agent in the same colour in every pane and after every restart.
        for t in [Theme::Deck, Theme::Lcars] {
            assert_eq!(t.nick("Max"), t.nick("Max"));
            assert_ne!(t.nick("Max"), t.nick("Iris"));
        }
    }

    #[test]
    fn lcars_is_painted_in_the_trans_flag() {
        // Stated as a test because it is a deliberate choice, not a palette
        // somebody will later "fix" to something more usual.
        assert_eq!(Theme::Lcars.accent(), Color::Rgb(0x5b, 0xce, 0xfa));
        assert_eq!(Theme::Lcars.second(), Color::Rgb(0xf5, 0xa9, 0xb8));
        assert_eq!(Theme::Lcars.fg(), Color::Rgb(255, 255, 255));
        assert_eq!(Theme::Lcars.bg(), Color::Rgb(0, 0, 0));
    }

    #[test]
    fn an_unknown_theme_name_is_the_quiet_one() {
        // A console that opens in LCARS because a config file was corrupted
        // is a surprise; a console that opens quiet never is.
        assert_eq!(Theme::from_name("nonsense"), Theme::Deck);
        assert_eq!(Theme::from_name(""), Theme::Deck);
        assert_eq!(Theme::from_name(" lcars "), Theme::Lcars);
        assert_eq!(Theme::Deck.next(), Theme::Lcars);
        assert_eq!(Theme::Lcars.next(), Theme::Deck);
    }

    #[test]
    fn only_lcars_shouts() {
        assert_eq!(Theme::Deck.label("agents"), "agents");
        assert_eq!(Theme::Lcars.label("agents"), "AGENTS");
    }
}
