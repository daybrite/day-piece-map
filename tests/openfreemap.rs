// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! The real thing, recorded: OpenFreeMap's Liberty style over central London, served from
//! `tests/data/openfreemap` by a strict `Simulation` on a manual clock, so the piece draws a
//! production style and production tiles with no network.

#[path = "support/openfreemap.rs"]
mod recording;

use day_mock::{MockProbe, MockToolkit};
use day_part_http::Client;
use day_part_http::simulation::{ManualClock, Simulation};
use day_piece_map::{LngLat, MapStatus, MapView, map_with_client};
use day_pieces::prelude::*;
use day_reactive::flush_sync;
use day_spec::WindowOptions;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Fixture {
    _probe: MockProbe,
    clock: ManualClock,
    sim: Simulation,
    view: MapView,
    /// Requests no recorded route answered; strict, so each one failed.
    unanswered: Arc<Mutex<Vec<String>>>,
    /// Frame requests the piece made; the mock toolkit delivers none, so the test does.
    frames: Rc<RefCell<Vec<day_spec::FrameCallback>>>,
}

fn boot() -> Fixture {
    day_core::uninstall_tree();
    let clock = ManualClock::default();
    let sim = Simulation::with_clock(Arc::new(clock.clone()));
    recording::routes(&sim);
    // Strict already (the default); this last rule only writes down what fell through, so a
    // request the recording lacks names itself in the failure instead of going out.
    let unanswered = Arc::new(Mutex::new(Vec::new()));
    {
        let unanswered = unanswered.clone();
        sim.handle(
            |_, _| true,
            move |request| {
                if let Ok(mut list) = unanswered.lock() {
                    list.push(request.url.clone());
                }
                Err(day_part_http::HttpError::Io(format!(
                    "not recorded: {}",
                    request.url
                )))
            },
        );
    }
    let client = Client::builder().transport(sim.clone()).build();
    // The demo's London home: Charing Cross at zoom 15.
    let view = MapView::new(LngLat::new(-0.1281, 51.5080), 15.0);
    let frames: Rc<RefCell<Vec<day_spec::FrameCallback>>> = Rc::new(RefCell::new(Vec::new()));
    let (mock, probe) = MockToolkit::new();
    day_core::launch_with(
        mock,
        WindowOptions::default()
            .title("OpenFreeMap fixture")
            .size(1280.0, 800.0),
        move || map_with_client(recording::STYLE_URL, view, client).any(),
    );
    // After launch, which installs the toolkit's own requester: the mock delivers no frames,
    // so the test keeps the callbacks and delivers them itself.
    {
        let frames = frames.clone();
        day_core::frame::install_frame_requester(move |_, cb| {
            frames.borrow_mut().push(cb);
            Box::new(|| {})
        });
    }
    Fixture {
        _probe: probe,
        clock,
        sim,
        view,
        unanswered,
        frames,
    }
}

impl Fixture {
    /// Let the simulated network answer, the tiles decode (on worker threads, in real time)
    /// and the frame re-record, until every tile is in and what the frame drew has held still
    /// for a while, or a minute has passed.
    fn settle(&self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut last = self.view.frame.get_untracked();
        let mut quiet = 0;
        while Instant::now() < deadline {
            flush_sync();
            self.clock.advance(Duration::from_millis(50));
            self.tick();
            let frame = self.view.frame.get_untracked();
            let loaded = self.view.status.get_untracked() != MapStatus::Loading
                && self.view.pending.get_untracked() == 0;
            quiet = if loaded && frame == last {
                quiet + 1
            } else {
                0
            };
            last = frame;
            // Rasters and label placements finish after the tiles land, a few frames later.
            if quiet >= 16 {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Deliver the display frames the piece asked for.
    fn tick(&self) {
        let pending: Vec<day_spec::FrameCallback> = self.frames.borrow_mut().drain(..).collect();
        for cb in pending {
            cb(day_spec::FrameStamp::new(0.016));
        }
        flush_sync();
    }
}

#[test]
fn the_recorded_liberty_style_draws_london_offline() {
    let f = boot();
    f.settle();
    let requests: Vec<String> = f.sim.requests().into_iter().map(|r| r.url).collect();
    assert_eq!(
        f.view.status.get_untracked(),
        MapStatus::Ready,
        "{requests:#?}"
    );
    assert!(
        f.unanswered.lock().map(|l| l.is_empty()).unwrap_or(false),
        "requests the recording does not hold: {:?}",
        f.unanswered.lock().map(|l| l.clone())
    );
    // The view at zoom 15 fetches zoom 14, the source's deepest: the four recorded tiles.
    for tile in [
        "14/8185/5447",
        "14/8186/5447",
        "14/8185/5448",
        "14/8186/5448",
    ] {
        assert!(
            requests
                .iter()
                .any(|u| u.ends_with(&format!("/{tile}.pbf"))),
            "{tile} was fetched: {requests:#?}"
        );
    }
    assert_eq!(f.view.pending.get_untracked(), 0);
    let frame = f.view.frame.get_untracked();
    assert!(
        frame.bitmaps >= 4,
        "each tile's raster is blitted: {frame:?}"
    );
    // Central London: streets, stations, squares, the river, each with a name.
    assert!(
        frame.labels >= 20,
        "real London is full of names: {frame:?}"
    );
    assert_eq!(
        f.view.attribution.get_untracked(),
        "OpenFreeMap © OpenMapTiles Data from OpenStreetMap"
    );
    drop(f);
    day_core::uninstall_tree();
}
