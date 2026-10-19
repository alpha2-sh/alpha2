//! What a replacement project invalidates in the binding's media caches.

use std::collections::{HashMap, HashSet};

use alpha2_model::Project;

/// Sources whose uploaded audio and frames a replacement project invalidates.
///
/// A source keeps its media while its identity, its ID and locator, is
/// unchanged; clip edits and effects do not change that identity. Derived
/// presentation frames and rate-converted audio also depend on project settings
/// and are invalidated separately by the binding ([`stale_presentations`]). A
/// source that is gone, or whose locator changed, loses everything uploaded
/// for it. A host whose media changes under an unchanged locator must give it
/// a new ID.
pub(crate) fn stale_sources(previous: &Project, next: &Project) -> HashSet<String> {
    let kept = next
        .sources
        .iter()
        .map(|source| (&source.id, source.locator.as_str()))
        .collect::<HashSet<_>>();
    previous
        .sources
        .iter()
        .filter(|old| !kept.contains(&(&old.id, old.locator.as_str())))
        .map(|old| old.id.as_str().to_owned())
        .collect()
}

/// Which frames prepared for presentation a replacement project invalidates,
/// beyond those of [`stale_sources`].
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum StalePresentations {
    /// Every one: they were fitted to another output size, or converted under
    /// another color pipeline (`settings.color`: its HDR policy and peak).
    All,
    /// Those of these sources, whose declared color (`color_metadata`), which
    /// a host converts their frames with, changed.
    Sources(HashSet<String>),
}

/// The presentation frames `next` invalidates: a presentation frame is the
/// picture converted and fitted under the project at its upload, so a
/// different output size or color pipeline invalidates them all, and a
/// source's new declared color invalidates that source's.
pub(crate) fn stale_presentations(previous: &Project, next: &Project) -> StalePresentations {
    let (before, after) = (&previous.settings, &next.settings);
    if (before.width, before.height, before.color) != (after.width, after.height, after.color) {
        return StalePresentations::All;
    }
    let declared = next
        .sources
        .iter()
        .map(|source| (&source.id, source.color_metadata))
        .collect::<HashMap<_, _>>();
    StalePresentations::Sources(
        previous
            .sources
            .iter()
            .filter(|old| {
                declared
                    .get(&old.id)
                    .is_some_and(|color| *color != old.color_metadata)
            })
            .map(|old| old.id.as_str().to_owned())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::{StalePresentations, stale_presentations, stale_sources};
    use alpha2_model::Project;
    use serde_json::json;
    use std::collections::HashSet;

    fn project(sources: &[(&str, &str)], clip_start: i64) -> Project {
        let sources = sources
            .iter()
            .map(|(id, locator)| json!({"id": id, "locator": locator, "kind": "audio"}))
            .collect::<Vec<_>>();
        let clips = [json!({
            "id": "clip",
            "source_id": "a",
            "timeline_range": {"start": clip_start, "end": clip_start + 1_000},
            "source_range": {"start": 0, "end": 1_000}
        })];
        Project::from_json(
            &json!({
                "schema_version": 1,
                "id": "p",
                "name": "p",
                "settings": {
                    "width": 16, "height": 16,
                    "frame_rate": {"num": 30, "den": 1},
                    "sample_rate": 48_000
                },
                "sources": sources,
                "timeline": {"tracks": [
                    {"id": "t", "name": "t", "kind": "audio", "clips": clips}
                ]}
            })
            .to_string(),
        )
        .expect("valid project")
    }

    #[test]
    fn stale_sources_are_found_in_time_linear_in_the_sources() {
        let with_sources = |locator: &str, count: usize| {
            let mut project = project(&[("a", "file://a")], 0);
            project.sources.extend((0..count).map(|index| {
                alpha2_model::MediaSource::new(
                    alpha2_media::SourceId::new(format!("s{index}")).expect("id"),
                    format!("{locator}/{index}"),
                    alpha2_model::MediaKind::Audio,
                )
            }));
            project
        };
        let count = alpha2_model::MAX_CLIPS;
        let (before, after) = (
            with_sources("file://old", count),
            with_sources("file://new", count),
        );
        assert_eq!(stale_sources(&before, &after).len(), count);
        alpha2_scaling_support::assert_linear(
            "stale sources",
            count / alpha2_scaling_support::GROWTH,
            |count| {
                (
                    with_sources("file://old", count),
                    with_sources("file://new", count),
                )
            },
            |(before, after)| {
                std::hint::black_box(stale_sources(before, after));
            },
        );
    }

    #[test]
    fn an_edit_that_keeps_every_source_invalidates_nothing() {
        let before = project(&[("a", "file://a"), ("b", "file://b")], 0);
        let moved = project(&[("a", "file://a"), ("b", "file://b")], 5_000);
        assert!(stale_sources(&before, &moved).is_empty());
    }

    #[test]
    fn a_removed_source_is_invalidated() {
        let before = project(&[("a", "file://a"), ("b", "file://b")], 0);
        let after = project(&[("a", "file://a")], 0);
        assert_eq!(stale_sources(&before, &after), ["b".to_owned()].into());
    }

    #[test]
    fn presentations_follow_the_size_the_color_pipeline_and_each_declared_color() {
        let before = project(&[("a", "file://a"), ("b", "file://b")], 0);
        let mut after = project(&[("a", "file://a"), ("b", "file://b")], 5_000);
        assert_eq!(
            stale_presentations(&before, &after),
            StalePresentations::Sources(HashSet::new())
        );
        after.settings.color.hdr_peak_nits = 4_000;
        assert_eq!(
            stale_presentations(&before, &after),
            StalePresentations::All
        );
        let mut after = project(&[("a", "file://a"), ("b", "file://b")], 0);
        after.settings.color.hdr = alpha2_model::HdrSources::Reject;
        assert_eq!(
            stale_presentations(&before, &after),
            StalePresentations::All
        );
        let mut after = project(&[("a", "file://a"), ("b", "file://b")], 0);
        after.sources[1].color_metadata = Some(alpha2_media::ColorMetadata::EXPORT);
        assert_eq!(
            stale_presentations(&before, &after),
            StalePresentations::Sources(["b".to_owned()].into())
        );
    }

    #[test]
    fn a_new_locator_invalidates_only_that_source() {
        let before = project(&[("a", "file://a"), ("b", "file://b")], 0);
        let after = project(&[("a", "file://a"), ("b", "file://b-v2")], 0);
        assert_eq!(stale_sources(&before, &after), ["b".to_owned()].into());
    }
}
