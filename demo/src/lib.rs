// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! Map Demo: the demo and on-device test app for `day-piece-map`.
//!
//! One page: a map that pans and zooms, a picker for where its style comes from, zoom buttons,
//! and a readout of what the piece reports (status, position, what the last frame drew). Every
//! control carries an id so `dayscript/demo.yaml` can drive it and read it back.
//!
//! The picker offers OpenFreeMap's five styles and VersaTiles' two, all OpenStreetMap data for
//! the whole planet served without a key, and the built-in sample: a small island drawn by hand
//! in the piece's `sample` module and served through the HTTP part's interception, so the walkthrough draws the
//! same map on every target without a network, which is what the CI screenshots need.

use day::prelude::*;
use day_piece_map::{LngLat, MapStatus, MapView, Marker, map, sample};

pub mod offline;

// Typed constants for everything under resource/: https://daybrite.dev/docs/how-to/resources
day::resources!();
// The mobile and web entry point; desktop enters through src/main.rs.
day::day_start!(options: res::window_options(), root);

/// Where the map's style comes from: OpenFreeMap's five styles and VersaTiles' two, all
/// OpenStreetMap data served without a key, and the built-in sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Liberty,
    Bright,
    Positron,
    Fiord,
    Dark,
    /// VersaTiles Colorful: the Shortbread schema, so a different set of layers and sources.
    Colorful,
    ColorfulDark,
    /// The hand-drawn island served in-process.
    Sample,
    /// Liberty from the recording in the piece's test data, served in-process: the real style
    /// around the London home with no network, for the walkthrough.
    LibertyOffline,
}

impl Source {
    const ALL: [Source; 9] = [
        Source::Liberty,
        Source::Bright,
        Source::Positron,
        Source::Fiord,
        Source::Dark,
        Source::Colorful,
        Source::ColorfulDark,
        Source::Sample,
        Source::LibertyOffline,
    ];

    fn style_url(self) -> String {
        match self {
            Source::Liberty => "https://tiles.openfreemap.org/styles/liberty".to_string(),
            Source::Bright => "https://tiles.openfreemap.org/styles/bright".to_string(),
            Source::Positron => "https://tiles.openfreemap.org/styles/positron".to_string(),
            Source::Fiord => "https://tiles.openfreemap.org/styles/fiord".to_string(),
            Source::Dark => "https://tiles.openfreemap.org/styles/dark".to_string(),
            Source::Colorful => {
                "https://tiles.versatiles.org/assets/styles/colorful/style.json".to_string()
            }
            Source::ColorfulDark => {
                "https://tiles.versatiles.org/assets/styles/colorful-dark/style.json".to_string()
            }
            Source::Sample => sample::STYLE_URL.to_string(),
            Source::LibertyOffline => offline::STYLE_URL.to_string(),
        }
    }

    fn title(self) -> String {
        match self {
            Source::Liberty => res::str::source_liberty(),
            Source::Bright => res::str::source_bright(),
            Source::Positron => res::str::source_positron(),
            Source::Fiord => res::str::source_fiord(),
            Source::Dark => res::str::source_dark(),
            Source::Colorful => res::str::source_colorful(),
            Source::ColorfulDark => res::str::source_colorful_dark(),
            Source::Sample => res::str::source_sample(),
            Source::LibertyOffline => res::str::source_liberty_offline(),
        }
        .format()
    }

    /// Where to look first: Trafalgar Square, close enough to read street names, or the
    /// middle of the sample island. The recording holds the tiles around the square at zoom
    /// 15.
    fn home(self) -> (LngLat, f64) {
        match self {
            Source::Sample => (sample::HOME, 9.0),
            Source::LibertyOffline => (TRAFALGAR, 15.0),
            _ => (TRAFALGAR, 14.0),
        }
    }

    /// Two places worth a marker and a flight between them, and the box that holds both.
    fn sights(self) -> [(LngLat, &'static str); 2] {
        match self {
            Source::Sample => [
                (sample::HOME, "Island center"),
                (sample::EAST_COAST, "East coast"),
            ],
            _ => [
                (TRAFALGAR, "Trafalgar Square"),
                (BIG_BEN, "Elizabeth Tower"),
            ],
        }
    }

    /// Route this source's requests: the island always answers in-process, the recording only
    /// while it is the source, so the live styles keep reaching the network otherwise. The
    /// session holds one simulation, so the two route sets go on one.
    fn install_simulation(self) {
        let sim = day_part_http::simulation::Simulation::new();
        sample::routes(&sim);
        if self == Source::LibertyOffline {
            offline::routes(&sim);
        } else {
            sim.set_strict(false);
        }
        day_part_http::Session::global().set_simulation(Some(sim));
    }
}

const TRAFALGAR: LngLat = LngLat::new(-0.1281, 51.5080);
const BIG_BEN: LngLat = LngLat::new(-0.1246, 51.5007);

/// The languages OpenMapTiles carries names in (`name:<code>` on places, roads, water and
/// points of interest), by code and English name. A feature without a name in the chosen
/// language shows the style's own choice.
const LANGUAGES: [(&str, &str); 65] = [
    ("am", "Amharic"),
    ("ar", "Arabic"),
    ("az", "Azerbaijani"),
    ("be", "Belarusian"),
    ("bg", "Bulgarian"),
    ("br", "Breton"),
    ("bs", "Bosnian"),
    ("ca", "Catalan"),
    ("co", "Corsican"),
    ("cs", "Czech"),
    ("cy", "Welsh"),
    ("da", "Danish"),
    ("de", "German"),
    ("el", "Greek"),
    ("en", "English"),
    ("eo", "Esperanto"),
    ("es", "Spanish"),
    ("et", "Estonian"),
    ("eu", "Basque"),
    ("fi", "Finnish"),
    ("fr", "French"),
    ("fy", "Western Frisian"),
    ("ga", "Irish"),
    ("gd", "Scottish Gaelic"),
    ("he", "Hebrew"),
    ("hi", "Hindi"),
    ("hr", "Croatian"),
    ("hu", "Hungarian"),
    ("hy", "Armenian"),
    ("id", "Indonesian"),
    ("is", "Icelandic"),
    ("it", "Italian"),
    ("ja", "Japanese"),
    ("ka", "Georgian"),
    ("kk", "Kazakh"),
    ("kn", "Kannada"),
    ("ko", "Korean"),
    ("ku", "Kurdish"),
    ("la", "Latin"),
    ("lb", "Luxembourgish"),
    ("lt", "Lithuanian"),
    ("lv", "Latvian"),
    ("mk", "Macedonian"),
    ("ml", "Malayalam"),
    ("mt", "Maltese"),
    ("nl", "Dutch"),
    ("no", "Norwegian"),
    ("oc", "Occitan"),
    ("pl", "Polish"),
    ("pt", "Portuguese"),
    ("rm", "Romansh"),
    ("ro", "Romanian"),
    ("ru", "Russian"),
    ("sk", "Slovak"),
    ("sl", "Slovenian"),
    ("sq", "Albanian"),
    ("sr", "Serbian"),
    ("sv", "Swedish"),
    ("ta", "Tamil"),
    ("te", "Telugu"),
    ("th", "Thai"),
    ("tr", "Turkish"),
    ("uk", "Ukrainian"),
    ("vi", "Vietnamese"),
    ("zh", "Chinese"),
];

/// The language the device runs in, when the data has it: `fr-FR` picks French.
fn device_language() -> Option<String> {
    let locale = day::locale().get_untracked();
    let code = locale
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    LANGUAGES
        .iter()
        .find(|(c, _)| *c == code)
        .map(|(c, _)| (*c).to_string())
}

/// The page.
pub fn root() -> impl Piece {
    info!("Map Demo starting");
    let source = Signal::new(Source::Liberty);
    let (center, zoom) = Source::Liberty.home();
    let view = MapView::new(center, zoom);
    // The style follows the source, once its routes are in place: the piece reloads on the
    // signal, which this effect writes after installing the simulation the load goes through.
    let style = Signal::new(String::new());
    // Choosing a source also goes to its home, so the island is in view when it is picked,
    // and puts the source's two markers down.
    Effect::new(move || {
        let src = source.get();
        src.install_simulation();
        let (center, zoom) = src.home();
        view.jump_to(center, zoom);
        view.rotate_to(0.0);
        view.clear_markers();
        for (at, title) in src.sights() {
            view.add_marker(Marker::new(at).title(title));
        }
        view.tap.set(None);
        style.set(src.style_url());
    });

    // The title and the zoom buttons on one row, the source picker on its own, so a phone's
    // width holds them.
    let header = row((
        label(res::str::app_title())
            .font(Font::Headline)
            .id(id!(map_title)),
        spacer(),
        button(res::str::zoom_out())
            .action(move || view.zoom_by(-1.0))
            .id(id!(map_zoom_out)),
        button(res::str::zoom_in())
            .action(move || view.zoom_by(1.0))
            .id(id!(map_zoom_in)),
    ))
    .spacing(Space::S);
    // The language starts as the device's, where the data speaks it, and follows the picker.
    let language = Signal::new(device_language());
    view.set_language(language.get_untracked().as_deref());
    Effect::new(move || view.set_language(language.get().as_deref()));
    let mut languages: Vec<(Option<String>, String)> =
        vec![(None, res::str::language_default().format())];
    languages.extend(
        LANGUAGES
            .iter()
            .map(|(code, name)| (Some((*code).to_string()), format!("{name} ({code})"))),
    );
    let source_row = row((
        picker(
            source,
            Source::ALL
                .iter()
                .map(|s| (*s, s.title()))
                .collect::<Vec<_>>(),
        )
        .menu()
        .id(id!(map_source)),
        picker(language, languages).menu().id(id!(map_language)),
    ))
    .spacing(Space::S);
    // The camera API: fly between the two sights, fit both, turn and come back north.
    let flights = Signal::new(0usize);
    let camera_row = row((
        button(res::str::fly())
            .action(move || {
                let n = flights.get_untracked() + 1;
                flights.set(n);
                let (at, _) = source.get_untracked().sights()[n % 2];
                view.fly_to(at, 16.0);
            })
            .id(id!(map_fly)),
        button(res::str::fit())
            .action(move || {
                let [(a, _), (b, _)] = source.get_untracked().sights();
                let sw = LngLat::new(a.lng.min(b.lng), a.lat.min(b.lat));
                let ne = LngLat::new(a.lng.max(b.lng), a.lat.max(b.lat));
                view.fit_bounds(sw, ne, 60.0);
            })
            .id(id!(map_fit)),
        button(res::str::rotate())
            .action(move || view.rotate_to(view.bearing.get_untracked() + 45.0))
            .id(id!(map_rotate)),
        button(res::str::north())
            .action(move || view.reset_north())
            .id(id!(map_north)),
        label(move || {
            let b = view.bearing.get();
            if b.abs() < 0.5 {
                res::str::bearing_north().format()
            } else {
                res::str::bearing(format!("{:.0}", (b + 360.0) % 360.0)).format()
            }
        })
        .id(id!(map_bearing)),
    ))
    .spacing(Space::S);
    // What the last tap found: a marker, the topmost feature with a name, or nothing.
    let tap = label(move || match view.tap.get() {
        None => res::str::tap_prompt().format(),
        Some(t) => {
            if let Some(id) = t.marker {
                let title = view.marker(id).map(|m| m.title).unwrap_or_default();
                return res::str::tap_marker(title).format();
            }
            match t.features.first() {
                None => res::str::tap_nothing().format(),
                Some(f) => {
                    let name = f
                        .properties
                        .get("name")
                        .or_else(|| f.properties.get("class"))
                        .map(|v| match v {
                            day_piece_map::Value::String(s) => s.clone(),
                            other => format!("{other:?}"),
                        })
                        .unwrap_or_default();
                    res::str::tap_feature(res::str::TapFeature {
                        layer: f.layer.clone(),
                        name,
                        count: i64::try_from(t.features.len()).unwrap_or(0),
                    })
                    .format()
                }
            }
        }
    })
    .id(id!(map_tap));

    let status = label(move || match view.status.get() {
        MapStatus::Loading => res::str::status_loading().format(),
        MapStatus::Ready => res::str::status_ready().format(),
        MapStatus::Failed(why) => res::str::status_failed(why).format(),
    })
    .id(id!(map_status));
    let position = label(move || {
        let c = view.center.get();
        res::str::position(res::str::Position {
            lng: format!("{:.4}", c.lng),
            lat: format!("{:.4}", c.lat),
            zoom: format!("{:.2}", view.zoom.get()),
        })
        .format()
    })
    .id(id!(map_position));
    let frame = label(move || {
        let f = view.frame.get();
        res::str::frame(res::str::Frame {
            tiles: i64::try_from(f.bitmaps).unwrap_or(0),
            labels: i64::try_from(f.labels).unwrap_or(0),
            pending: i64::try_from(view.pending.get()).unwrap_or(0),
        })
        .format()
    })
    .id(id!(map_frame));
    column((
        header,
        source_row,
        camera_row,
        map(style, view).id(id!(map_canvas)).grow(Dimension::Both),
        row((status, position)).spacing(Space::M),
        row((frame, tap)).spacing(Space::M),
    ))
    .spacing(Space::S)
    .padding(Space::M)
}
