//! Two-colour gradients, interpolated the way Niri does it for borders and focus rings, so a
//! gradient copied out of the Niri config comes out looking the same. The same mixing is used
//! for fading the focus pill between colours.

use serde::Deserialize;
use waybar_cffi::gtk::gdk::RGBA;

/// The gradient as written in the taskbar configuration, mirroring Niri's `active-gradient`.
#[derive(Debug, Deserialize)]
pub struct Config {
    from: String,
    to: String,
    /// The colour space to interpolate in, in Niri's syntax: `srgb`, `srgb-linear`, `oklab`, or
    /// `oklch` followed optionally by `shorter hue`, `longer hue`, `increasing hue` or
    /// `decreasing hue`.
    #[serde(default, rename = "in")]
    space: Option<String>,
    /// How long one trip along the gradient takes while it cycles.
    #[serde(default = "default_cycle_ms")]
    cycle_ms: u32,
    /// How long the gradient takes to spread out when a drag starts, and to shrink away after.
    #[serde(default = "default_fade_ms")]
    fade_ms: u32,
}

fn default_cycle_ms() -> u32 {
    2000
}

fn default_fade_ms() -> u32 {
    250
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gradient {
    from: [f64; 4],
    to: [f64; 4],
    space: Space,
    pub cycle_ms: u32,
    pub fade_ms: u32,
}

/// The colour space colours are mixed in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Space {
    #[default]
    Srgb,
    SrgbLinear,
    Oklab,
    Oklch(Hue),
}

/// Which way round the hue wheel an OKLCH gradient goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hue {
    Shorter,
    Longer,
    Increasing,
    Decreasing,
}

impl Gradient {
    pub fn parse(config: &Config) -> Result<Self, String> {
        let colour = |s: &str| {
            s.parse::<RGBA>()
                .map(|c| [c.red(), c.green(), c.blue(), c.alpha()])
                .map_err(|_| format!("cannot parse colour {s:?}"))
        };

        Ok(Self {
            from: colour(&config.from)?,
            to: colour(&config.to)?,
            space: Space::parse(config.space.as_deref().unwrap_or("srgb"))?,
            cycle_ms: config.cycle_ms,
            fade_ms: config.fade_ms,
        })
    }

    /// Returns the colour `t` of the way from one end to the other, as sRGB with alpha.
    pub fn at(&self, t: f64) -> [f64; 4] {
        self.space.mix(self.from, self.to, t)
    }
}

impl Space {
    /// Parses a colour space in Niri's syntax: `srgb`, `srgb-linear`, `oklab`, or `oklch`
    /// followed optionally by `shorter hue`, `longer hue`, `increasing hue` or `decreasing hue`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let words: Vec<_> = s.split_whitespace().collect();
        Ok(match words.as_slice() {
            ["srgb"] => Self::Srgb,
            ["srgb-linear"] => Self::SrgbLinear,
            ["oklab"] => Self::Oklab,
            ["oklch"] | ["oklch", "shorter", "hue"] => Self::Oklch(Hue::Shorter),
            ["oklch", "longer", "hue"] => Self::Oklch(Hue::Longer),
            ["oklch", "increasing", "hue"] => Self::Oklch(Hue::Increasing),
            ["oklch", "decreasing", "hue"] => Self::Oklch(Hue::Decreasing),
            _ => return Err(format!("unknown colour space {s:?}")),
        })
    }

    /// Mixes two sRGB colours with alpha, `t` of the way from `from` to `to`.
    pub fn mix(self, from: [f64; 4], to: [f64; 4], t: f64) -> [f64; 4] {
        let mix = |a: f64, b: f64| a + (b - a) * t;
        let alpha = mix(from[3], to[3]);

        let [r, g, b] = match self {
            Space::Srgb => [0, 1, 2].map(|i| mix(from[i], to[i])),
            Space::SrgbLinear => {
                let (a, b) = (linear(from), linear(to));
                [0, 1, 2].map(|i| to_srgb(mix(a[i], b[i])))
            }
            Space::Oklab => {
                let (a, b) = (oklab(linear(from)), oklab(linear(to)));
                from_oklab([0, 1, 2].map(|i| mix(a[i], b[i])))
            }
            Space::Oklch(hue) => {
                let (a, b) = (oklch(oklab(linear(from))), oklch(oklab(linear(to))));
                let (h1, h2) = hue.fix(a, b);
                let (l, c, h) = (mix(a[0], b[0]), mix(a[1], b[1]), mix(h1, h2).to_radians());
                from_oklab([l, c * h.cos(), c * h.sin()])
            }
        };

        [
            r.clamp(0.0, 1.0),
            g.clamp(0.0, 1.0),
            b.clamp(0.0, 1.0),
            alpha,
        ]
    }
}

impl Hue {
    /// Adjusts the two hues so that going straight from one to the other goes the right way
    /// round, following CSS Color 4.
    fn fix(self, a: [f64; 3], b: [f64; 3]) -> (f64, f64) {
        // A grey has no hue to speak of, so borrow the other end's rather than swinging round.
        let (mut h1, mut h2) = match (a[1] < 1e-4, b[1] < 1e-4) {
            (true, false) => (b[2], b[2]),
            (false, true) => (a[2], a[2]),
            _ => (a[2], b[2]),
        };

        let delta = h2 - h1;
        match self {
            Self::Shorter if delta > 180.0 => h1 += 360.0,
            Self::Shorter if delta < -180.0 => h2 += 360.0,
            Self::Longer if 0.0 < delta && delta < 180.0 => h1 += 360.0,
            Self::Longer if -180.0 < delta && delta <= 0.0 => h2 += 360.0,
            Self::Increasing if h2 < h1 => h2 += 360.0,
            Self::Decreasing if h1 < h2 => h1 += 360.0,
            _ => {}
        }

        (h1, h2)
    }
}

fn linear(c: [f64; 4]) -> [f64; 3] {
    [0, 1, 2].map(|i| {
        let v = c[i];
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    })
}

fn to_srgb(v: f64) -> f64 {
    let v = v.clamp(0.0, 1.0);
    if v <= 0.0031308 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

fn oklab([r, g, b]: [f64; 3]) -> [f64; 3] {
    let l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
    let m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
    let s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();

    [
        0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s,
        1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s,
        0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s,
    ]
}

fn from_oklab([l, a, b]: [f64; 3]) -> [f64; 3] {
    let l_ = (l + 0.3963377774 * a + 0.2158037573 * b).powi(3);
    let m_ = (l - 0.1055613458 * a - 0.0638541728 * b).powi(3);
    let s_ = (l - 0.0894841775 * a - 1.2914855480 * b).powi(3);

    [
        4.0767416621 * l_ - 3.3077115913 * m_ + 0.2309699292 * s_,
        -1.2684380046 * l_ + 2.6097574011 * m_ - 0.3413193965 * s_,
        -0.0041960863 * l_ - 0.7034186147 * m_ + 1.7076147010 * s_,
    ]
    .map(to_srgb)
}

fn oklch([l, a, b]: [f64; 3]) -> [f64; 3] {
    [l, a.hypot(b), b.atan2(a).to_degrees().rem_euclid(360.0)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(space: &str) -> Gradient {
        Gradient::parse(&Config {
            from: "#a0a".into(),
            to: "#0ec".into(),
            space: Some(space.into()),
            cycle_ms: 2000,
            fade_ms: 250,
        })
        .unwrap()
    }

    fn close(a: [f64; 4], b: [f64; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-3)
    }

    #[test]
    fn ends_match_the_colours() {
        for space in ["srgb", "srgb-linear", "oklab", "oklch decreasing hue"] {
            let g = gradient(space);
            assert!(
                close(g.at(0.0), [2.0 / 3.0, 0.0, 2.0 / 3.0, 1.0]),
                "{space}"
            );
            assert!(close(g.at(1.0), [0.0, 14.0 / 15.0, 0.8, 1.0]), "{space}");
        }
    }

    #[test]
    fn hue_direction() {
        // Magenta to teal the decreasing way passes through blue, the increasing way through
        // yellow and green.
        let [r, _, b, _] = gradient("oklch decreasing hue").at(0.5);
        assert!(b > r, "expected blue in the middle");
        let [r, g, b, _] = gradient("oklch increasing hue").at(0.5);
        assert!(r > b && g > b, "expected yellow in the middle");
    }

    #[test]
    fn cyan_to_magenta_the_short_way_passes_through_blue() {
        let cyan = [0.0, 240.0 / 255.0, 240.0 / 255.0, 1.0];
        let magenta = [2.0 / 3.0, 0.0, 2.0 / 3.0, 1.0];
        let [r, g, b, _] = Space::parse("oklch").unwrap().mix(cyan, magenta, 0.5);
        assert!(b > r && b > g, "expected blue in the middle");
    }

    #[test]
    fn rejects_unknown_space() {
        assert!(Space::parse("hsl").is_err());
    }
}
