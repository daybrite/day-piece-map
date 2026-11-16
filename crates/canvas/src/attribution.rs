// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! Source-supplied legal credits, kept visible and actionable without rendering arbitrary HTML.

use day_pieces::prelude::*;
use day_reactive::Signal;

const OSM_COPYRIGHT: &str = "https://www.openstreetmap.org/copyright";
// A publication's legal credit, not app-owned UI copy.
const OSM_CREDIT: &str = "© OpenStreetMap contributors";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Credit {
    pub text: String,
    pub url: Option<String>,
}

/// Keep every source's credits, including unknown providers. Only the conventional OSM
/// notice is shortened; other providers can impose their own wording and link requirements.
pub(crate) fn parse(html: &str) -> Vec<Credit> {
    let doc = day_spec::html_to_styled(&html.replace("&copy;", "©"), day_spec::DocStyle::default());
    let mut credits = Vec::new();
    let mut end = 0;
    for run in doc.runs.iter().filter(|run| run.link.is_some()) {
        if run.range.start < end {
            continue;
        }
        push(&mut credits, &doc.text[end..run.range.start], None);
        push(&mut credits, &doc.text[run.range.clone()], run.link.clone());
        end = run.range.end;
    }
    push(&mut credits, &doc.text[end..], None);
    // Keep the primary data credit at the lower-left even when a tile provider lists itself first.
    credits.sort_by_key(|credit| credit.url.as_deref() != Some(OSM_COPYRIGHT));
    credits.dedup();
    credits
}

fn push(credits: &mut Vec<Credit>, text: &str, url: Option<String>) {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.trim_matches([' ', '|', '·']);
    if text.is_empty() {
        return;
    }
    let osm = matches!(
        text,
        "OpenStreetMap" | "© OpenStreetMap" | "OpenStreetMap contributors" | OSM_CREDIT
    );
    if osm {
        if credits.last().is_some_and(|credit| {
            credit.url.is_none()
                && matches!(credit.text.as_str(), "©" | "Data from" | "© Data from")
        }) {
            credits.pop();
        }
        credits.push(Credit {
            text: OSM_CREDIT.into(),
            url: Some(OSM_COPYRIGHT.into()),
        });
    } else if !(text == "contributors"
        && credits
            .last()
            .is_some_and(|credit| credit.text == OSM_CREDIT))
    {
        credits.push(Credit {
            text: text.into(),
            // Attribution HTML is remote content: never activate app routes or custom schemes.
            url: url
                .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
                .map(|url| day_spec::html_to_styled(&url, day_spec::DocStyle::default()).text),
        });
    }
}

pub(crate) fn strip(credits: Signal<Vec<Credit>>) -> impl Piece {
    keyed(
        move || {
            let credits = credits.get();
            (!credits.is_empty()).then_some(credits)
        },
        |credits| {
            let color = Color::rgb(0.2, 0.2, 0.2);
            row(credits
                .iter()
                .map(|credit| match &credit.url {
                    Some(url) => link(credit.text.clone(), url.clone())
                        .font(Font::System(10.0))
                        .color(color)
                        .any(),
                    None => label(credit.text.clone())
                        .font(Font::System(10.0))
                        .color(color)
                        .any(),
                })
                .collect::<Vec<_>>())
            .spacing(6.0)
            .fit(day_pieces::RowFit::Wrap { run_spacing: 0.0 })
            .padding(Insets::symmetric(6.0, 3.0))
            .background(Color::rgba(1.0, 1.0, 1.0, 0.75))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_osm_notice_preserves_other_required_credits_and_links() {
        let credits = parse(
            r#"<a href="https://openfreemap.org">OpenFreeMap</a> <a href="https://openmaptiles.org/">&copy; OpenMapTiles</a> Data from <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a>"#,
        );
        assert_eq!(
            credits,
            vec![
                Credit {
                    text: OSM_CREDIT.into(),
                    url: Some(OSM_COPYRIGHT.into())
                },
                Credit {
                    text: "OpenFreeMap".into(),
                    url: Some("https://openfreemap.org".into())
                },
                Credit {
                    text: "© OpenMapTiles".into(),
                    url: Some("https://openmaptiles.org/".into())
                },
            ]
        );
        for html in [
            OSM_CREDIT,
            "© <a href='https://openstreetmap.org'>OpenStreetMap</a> contributors",
        ] {
            assert_eq!(parse(html), vec![credits[0].clone()]);
        }
    }

    #[test]
    fn unknown_credits_survive_and_only_web_links_are_actionable() {
        let credits = parse(
            "Survey © 2026 <a href='https://survey.test/license?a=1&amp;b=2'><b>A &amp; B</b></a> <a href='javascript:alert(1)'>More</a>",
        );
        assert_eq!(credits[0].text, "Survey © 2026");
        assert_eq!(credits[1].text, "A & B");
        assert_eq!(
            credits[1].url.as_deref(),
            Some("https://survey.test/license?a=1&b=2")
        );
        assert_eq!(
            credits[2],
            Credit {
                text: "More".into(),
                url: None
            }
        );
        assert_eq!(parse("  "), [] as [Credit; 0]);
    }
}
