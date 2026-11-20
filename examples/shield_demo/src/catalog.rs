//! Game data used by the demo: hulls, guns, weapons and shield colours (from the 0.5.7 client and
//! the in-game wiki, as in the browser prototype). The mob colours are proposals.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HullKind {
    Ship,
    Alien,
    Dormant,
}

pub struct HullInfo {
    pub file: &'static str,
    pub fr: &'static str,
    pub en: &'static str,
    pub kind: HullKind,
    /// Mob shield colour (HDR); ships use the pilot's skin.
    pub color: [f32; 3],
    /// Shots have no effect on it.
    pub immune: bool,
}

const fn ship(file: &'static str) -> HullInfo {
    HullInfo {
        file,
        fr: file,
        en: file,
        kind: HullKind::Ship,
        color: [0.12, 0.55, 2.8],
        immune: false,
    }
}

const fn mob(file: &'static str, label: &'static str, color: [f32; 3], immune: bool) -> HullInfo {
    HullInfo {
        file,
        fr: label,
        en: label,
        kind: HullKind::Alien,
        color,
        immune,
    }
}

pub const HULLS: [HullInfo; 21] = [
    ship("Protos"),
    ship("Kitefin"),
    ship("Ostirion"),
    ship("Nomad"),
    ship("Paragon"),
    ship("Miyuki"),
    ship("Storm"),
    ship("Wraith"),
    ship("Ironclad"),
    mob("seeker", "Seeker", [2.8, 1.2, 0.12], false),
    mob("goombah", "Goombah", [0.3, 2.8, 0.25], false),
    mob("facet", "Facet", [0.1, 2.3, 2.0], false),
    mob("void", "Void", [1.3, 0.12, 2.8], false),
    mob("phantasm", "Phantasm", [1.8, 2.2, 2.8], false),
    mob("bulwark", "Bulwark", [2.8, 0.55, 0.08], false),
    mob("crystalys", "Crystalys", [2.8, 0.25, 1.5], false),
    mob("tesseract", "Tesseract", [2.6, 2.2, 0.35], false),
    mob("inert-mass", "Inert Mass", [1.0, 1.25, 1.4], false),
    mob("unwakened", "Unwakened", [2.8, 0.1, 0.1], true),
    HullInfo {
        file: "turret",
        fr: "Tourelle dormante",
        en: "Dormant Turret",
        kind: HullKind::Dormant,
        color: [2.8, 0.8, 0.1],
        immune: true,
    },
    HullInfo {
        file: "excavator",
        fr: "Excavatrice géante",
        en: "Giant Excavator",
        kind: HullKind::Dormant,
        color: [2.2, 1.5, 0.4],
        immune: true,
    },
];

pub fn hull_index(file: &str) -> Option<usize> {
    HULLS.iter().position(|h| h.file.eq_ignore_ascii_case(file))
}

/// Gun positions of a ship, read from the 0.5.7 client: a centre gun and two pairs (± x), in
/// client units (model × 110, +Z forward, +Y up). `l` is the ship's largest dimension.
pub struct Guns {
    pub label: &'static str,
    pub l: f32,
    pub c: [f32; 3],
    pub a: [f32; 3],
    pub b: [f32; 3],
}

pub const GUNS: [Guns; 12] = [
    Guns {
        label: "Protos",
        l: 50.0,
        c: [0.0, -2.6, 7.4],
        a: [7.05, -6.75, 7.5],
        b: [27.5, -1.9, -5.0],
    },
    Guns {
        label: "Kitefin",
        l: 72.6,
        c: [0.0, -3.9, 39.9],
        a: [15.4, -3.5, 21.6],
        b: [28.1, -1.8, 0.3],
    },
    Guns {
        label: "Ostirion",
        l: 86.1,
        c: [0.0, -0.9, 28.5],
        a: [5.7, -5.6, 50.7],
        b: [13.3, 6.4, 44.3],
    },
    Guns {
        label: "Nomad",
        l: 86.5,
        c: [0.0, -5.0, 37.0],
        a: [25.3, -4.0, 41.5],
        b: [15.2, -4.0, 45.0],
    },
    Guns {
        label: "Paragon",
        l: 90.3,
        c: [0.0, -4.7, 55.2],
        a: [16.75, -2.2, 9.6],
        b: [25.5, 8.7, 36.8],
    },
    Guns {
        label: "Miyuki",
        l: 99.2,
        c: [0.0, -3.2, 53.1],
        a: [20.1, -5.0, 25.9],
        b: [10.3, -6.7, 38.2],
    },
    Guns {
        label: "Storm",
        l: 116.0,
        c: [0.0, -1.5, 62.0],
        a: [35.3, -13.8, 50.5],
        b: [54.6, -14.8, 8.0],
    },
    Guns {
        label: "Storm NOTSUM",
        l: 112.4,
        c: [0.0, -1.5, 62.0],
        a: [35.3, -13.8, 50.5],
        b: [49.2, -14.8, 5.0],
    },
    Guns {
        label: "Storm RECON",
        l: 116.0,
        c: [0.0, -1.5, 62.0],
        a: [8.6, -1.7, 32.1],
        b: [54.6, -14.8, 4.8],
    },
    Guns {
        label: "Wraith",
        l: 130.6,
        c: [0.0, 6.5, 53.0],
        a: [20.8, 2.0, 37.0],
        b: [29.7, -4.6, 14.8],
    },
    Guns {
        label: "Ironclad",
        l: 106.0,
        c: [0.0, -9.3, 57.0],
        a: [41.8, -18.1, 50.8],
        b: [44.7, -6.9, 32.8],
    },
    Guns {
        label: "Ironclad DUMA",
        l: 100.0,
        c: [0.0, 0.0, 51.0],
        a: [41.8, -18.1, 36.0],
        b: [44.7, -6.9, 32.8],
    },
];

/// Bolts per volley seen in game for the pilot who fires: 2 on a Protos, 4 on the other ships.
pub fn observed_shots(guns: &Guns) -> usize {
    if guns.label == "Protos" { 2 } else { 4 }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WeaponKind {
    Laser,
    Rocket,
    Blast,
    Siphon,
}

pub struct Weapon {
    pub key: &'static str,
    pub fr: &'static str,
    pub en: &'static str,
    pub kind: WeaponKind,
    /// Penetration (%); `None`: the "custom" slider.
    pub pen: Option<f32>,
    pub sound: &'static str,
}

const fn laser(key: &'static str, fr: &'static str, en: &'static str, pen: Option<f32>, sound: &'static str) -> Weapon {
    Weapon {
        key,
        fr,
        en,
        kind: WeaponKind::Laser,
        pen,
        sound,
    }
}

pub const WEAPONS: [Weapon; 10] = [
    laser("x1", "Laser x1/x2 · 0 %", "Laser x1/x2 · 0%", Some(0.0), "laser_x1"),
    laser("x3", "Laser x3 · 5 %", "Laser x3 · 5%", Some(5.0), "laser_x3"),
    laser("x4", "Laser x4 · 10 %", "Laser x4 · 10%", Some(10.0), "laser_x4"),
    laser("am", "Antimatter · 20 %", "Antimatter · 20%", Some(20.0), "laser_x3"),
    laser("gem", "x4 + Gemini · 25 %", "x4 + Gemini · 25%", Some(25.0), "laser_x4"),
    laser(
        "best",
        "Meilleur laser · 85 %",
        "Best laser · 85%",
        Some(85.0),
        "laser_x4",
    ),
    Weapon {
        key: "lancet",
        fr: "Lancet III · 35 %",
        en: "Lancet III · 35%",
        kind: WeaponKind::Rocket,
        pen: Some(35.0),
        sound: "rocket_launch",
    },
    Weapon {
        key: "ember",
        fr: "Roquette à explosion",
        en: "Blast rocket",
        kind: WeaponKind::Blast,
        pen: Some(0.0),
        sound: "rocket_launch",
    },
    Weapon {
        key: "siphon",
        fr: "Siphon (bouclier seul)",
        en: "Siphon (shield only)",
        kind: WeaponKind::Siphon,
        pen: Some(0.0),
        sound: "laser_siphon",
    },
    laser("custom", "Pénétration au choix", "Custom penetration", None, "laser_x2"),
];

pub const PALETTES: [(&str, &str, [f32; 3]); 6] = [
    ("Standard (bleu)", "Standard (blue)", [0.12, 0.55, 2.8]),
    ("Rouge plasma", "Plasma red", [2.8, 0.35, 0.08]),
    ("Or", "Gold", [2.8, 1.6, 0.25]),
    ("Vert toxique", "Toxic green", [0.2, 2.8, 0.35]),
    ("Violet", "Purple", [1.6, 0.25, 2.8]),
    ("Cyan", "Cyan", [0.12, 2.2, 2.8]),
];

/// Every alien's absorbance (wiki).
pub const ALIEN_ABSORB: f32 = 80.0;
