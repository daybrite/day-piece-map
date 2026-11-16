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
use day_piece_map::{LngLat, MapStatus, MapView, Marker};
// Keep provider dependencies available for Day's automatic backend feature discovery.
use day_piece_map_apple as _;
use day_piece_map_canvas::{CanvasMapView, map, sample};
use day_piece_map_google as _;

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
    fn sights(self) -> [(LngLat, String); 2] {
        match self {
            Source::Sample => [
                (sample::HOME, res::str::sight_island_center().format()),
                (sample::EAST_COAST, res::str::sight_east_coast().format()),
            ],
            _ => [
                (TRAFALGAR, res::str::sight_trafalgar_square().format()),
                (BIG_BEN, res::str::sight_elizabeth_tower().format()),
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
const LANGUAGES: [&str; 65] = [
    "am", "ar", "az", "be", "bg", "br", "bs", "ca", "co", "cs", "cy", "da", "de", "el", "en", "eo",
    "es", "et", "eu", "fi", "fr", "fy", "ga", "gd", "he", "hi", "hr", "hu", "hy", "id", "is", "it",
    "ja", "ka", "kk", "kn", "ko", "ku", "la", "lb", "lt", "lv", "mk", "ml", "mt", "nl", "no", "oc",
    "pl", "pt", "rm", "ro", "ru", "sk", "sl", "sq", "sr", "sv", "ta", "te", "th", "tr", "uk", "vi",
    "zh",
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
        .find(|c| **c == code)
        .map(|c| (*c).to_string())
}

/// The page.
pub fn root() -> impl Piece {
    info!("Map Demo starting");
    let source = Signal::new(Source::Liberty);
    let (center, zoom) = Source::Liberty.home();
    let view = MapView::new(center, zoom);
    let canvas_view = CanvasMapView::with_view(view);
    let provider = Signal::new(Provider::Canvas);
    let flights = Signal::new(0usize);
    // The style follows the source, once its routes are in place: the piece reloads on the
    // signal, which this effect writes after installing the simulation the load goes through.
    let style = Signal::new(String::new());
    // Choosing a source also goes to its home, so the island is in view when it is picked,
    // and puts the source's two markers down.
    Effect::new(move || {
        let src = source.get();
        flights.set(0);
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
    canvas_view.set_language(language.get_untracked().as_deref());
    Effect::new(move || canvas_view.set_language(language.get().as_deref()));
    let mut languages: Vec<(Option<String>, String)> =
        vec![(None, res::str::language_default().format())];
    languages.extend(LANGUAGES.iter().map(|code| {
        (
            Some((*code).to_string()),
            res::str::language_name(*code).format(),
        )
    }));
    let source_row = move || {
        row((
            picker(
                source,
                Source::ALL
                    .iter()
                    .map(|s| (*s, s.title()))
                    .collect::<Vec<_>>(),
            )
            .menu()
            .id(id!(map_source)),
            picker(language, languages.clone())
                .menu()
                .id(id!(map_language)),
        ))
        .spacing(Space::S)
        .fit(RowFit::Wrap { run_spacing: 8.0 })
    };
    // The camera API: fly between the two sights, fit both, turn and come back north.
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
    .spacing(Space::S)
    .fit(RowFit::Wrap { run_spacing: 8.0 });
    // What the last tap found: a marker, the topmost feature with a name, or nothing.
    let tap = label(move || match view.tap.get() {
        None => res::str::tap_prompt().format(),
        Some(t) => {
            if let Some(id) = t.marker {
                let title = view.marker(id).map_or_default(|m| m.title);
                return res::str::tap_marker(title).format();
            }
            let features = if provider.get() == Provider::Canvas {
                canvas_view.feature_hits.get()
            } else {
                Vec::new()
            };
            match features.first() {
                None => res::str::tap_nothing().format(),
                Some(f) => {
                    let name = f
                        .properties
                        .get("name")
                        .or_else(|| f.properties.get("class"))
                        .map_or_default(|v| match v {
                            day_piece_map_canvas::Value::String(s) => s.clone(),
                            other => format!("{other:?}"),
                        });
                    res::str::tap_feature(res::str::TapFeature {
                        layer: f.layer.clone(),
                        name,
                        count: i64::try_from(features.len()).unwrap_or(0),
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
        MapStatus::Failed(why) => res::str::status_failed(map_failure(&why)).format(),
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
        if provider.get() != Provider::Canvas {
            return String::new();
        }
        let f = canvas_view.frame.get();
        res::str::frame(res::str::Frame {
            tiles: i64::try_from(f.bitmaps).unwrap_or(0),
            labels: i64::try_from(f.labels).unwrap_or(0),
            pending: i64::try_from(canvas_view.pending.get()).unwrap_or(0),
        })
        .format()
    })
    .id(id!(map_frame));
    column((
        header,
        provider_picker(provider),
        when(move || provider.get() == Provider::Canvas, source_row),
        camera_row,
        switch(
            move || provider.get(),
            move |selected| {
                view.tap.set(None);
                match selected {
                    Provider::Canvas => map(style, canvas_view).id(id!(map_canvas)).any(),
                    #[cfg(any(
                        all(feature = "appkit", target_os = "macos"),
                        all(feature = "uikit", target_os = "ios")
                    ))]
                    Provider::Apple => day_piece_map_apple::map(view).id(id!(map_canvas)).any(),
                    #[cfg(all(feature = "material", target_os = "android"))]
                    Provider::Google => day_piece_map_google::map(view).id(id!(map_canvas)).any(),
                }
            },
        )
        .grow(Dimension::Both),
        row((status, position))
            .spacing(Space::M)
            .fit(RowFit::Wrap { run_spacing: 4.0 }),
        row((frame, tap))
            .spacing(Space::M)
            .fit(RowFit::Wrap { run_spacing: 4.0 }),
    ))
    .spacing(Space::S)
    .padding(Space::M)
}

/// Available providers follow the compiled toolkit; the shared view survives a switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Provider {
    Canvas,
    #[cfg(any(
        all(feature = "appkit", target_os = "macos"),
        all(feature = "uikit", target_os = "ios")
    ))]
    Apple,
    #[cfg(all(feature = "material", target_os = "android"))]
    Google,
}
fn provider_picker(provider: Signal<Provider>) -> impl Piece {
    let choices = [
        (Provider::Canvas, res::str::map_provider_osm()),
        #[cfg(any(
            all(feature = "appkit", target_os = "macos"),
            all(feature = "uikit", target_os = "ios")
        ))]
        (Provider::Apple, res::str::map_provider_apple()),
        #[cfg(all(feature = "material", target_os = "android"))]
        (Provider::Google, res::str::map_provider_google()),
    ];
    let multiple = choices.len() > 1;
    when(
        move || multiple,
        move || {
            picker(provider, choices.clone())
                .segmented()
                .id(day::id!(map_provider))
        },
    )
}
fn map_failure(code: &str) -> String {
    match code {
        "missing-api-key" => res::str::map_missing_api_key(),
        "play-services-unavailable" => res::str::map_play_services_unavailable(),
        _ => res::str::map_load_failed(),
    }
    .format()
}
