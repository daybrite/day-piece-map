// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! OpenFreeMap's Liberty style without the network: the style, its TileJSON, its sprite and the
//! tiles around the London home, recorded from the live server and answered from inside the app
//! through `day-part-http`'s interception, so a walkthrough draws the real style the same way on
//! every target, every run.
//!
//! The recording and its routes live with the piece's tests (`tests/support/openfreemap.rs`,
//! `tests/data/openfreemap/README.md`), which share them; the demo is never published, so it
//! reaches them by path.

#[path = "../../tests/support/openfreemap.rs"]
mod recording;

use day_part_http::Session;
use day_part_http::simulation::Simulation;

pub use recording::{STYLE_URL, routes};

/// Serve the recording on the default HTTP session, strictly: every URL it does not hold fails,
/// so nothing the map draws can have come from the network.
///
/// The session holds one simulation, so this replaces any installed before it (the sample
/// island's included). To serve both, put both route sets on one simulation and install that.
pub fn install() {
    let sim = Simulation::new();
    routes(&sim);
    Session::global().set_simulation(Some(sim));
}
