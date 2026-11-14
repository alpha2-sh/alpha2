//! What a replacement project invalidates in the binding's media caches.

use std::collections::{HashMap, HashSet};

use alpha2_model::Project;

/// Sources whose uploaded audio and frames a replacement project invalidates.
///
/// A source keeps its media while its ID, locator, kind, provider and fingerprint are unchanged;
/// clip edits and effects do not matter. Presentation frames also depend on project settings and
/// are handled by [`stale_presentations`]. A host whose media changes in a way the document does
/// not show must give the source a new ID.
pub(crate) fn stale_sources(previous: &Project, next: &Project) -> HashSet<String> {
    let kept = next
        .sources
        .iter()
        .map(|source| (&source.id, source))
        .collect::<HashMap<_, _>>();
    previous
        .sources
        .iter()
        .filter(|old| {
            !kept.get(&old.id).is_some_and(|new| {
                new.locator == old.locator
                    && new.kind == old.kind
                    && new.provider == old.provider
                    && new.fingerprint == old.fingerprint
            })
        })
        .map(|old| old.id.as_str().to_owned())
        .collect()
}

/// Presentation frames a replacement project invalidates beyond [`stale_sources`].
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum StalePresentations {
    /// All of them: the output size or color pipeline (`settings.color`) changed.
    All,
    /// Those of sources whose declared color (`color_metadata`) changed.
    Sources(HashSet<String>),
}

/// The presentation frames `next` invalidates. They were converted and fitted under the project
/// at upload time.
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
        alpha2_scaling_support::assert_linear_shared(
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
    fn a_changed_kind_provider_or_fingerprint_invalidates_that_source() {
        let lottie = |animation: Option<&str>| {
            serde_json::from_value::<alpha2_model::ProviderSource>(
                json!({"format": "lottie", "animation": animation}),
            )
            .expect("provider")
        };
        let mut before = project(&[("a", "file://a"), ("b", "file://b")], 0);
        before.sources[1].kind = alpha2_model::MediaKind::Video;
        before.sources[1].provider = Some(lottie(Some("intro")));
        let unchanged = before.clone();
        assert!(stale_sources(&before, &unchanged).is_empty());

        let mut animation = before.clone();
        animation.sources[1].provider = Some(lottie(Some("outro")));
        assert_eq!(stale_sources(&before, &animation), ["b".to_owned()].into());

        let mut fingerprint = before.clone();
        fingerprint.sources[0].fingerprint = Some(alpha2_model::MediaFingerprint::default());
        assert_eq!(
            stale_sources(&before, &fingerprint),
            ["a".to_owned()].into()
        );

        let mut kind = before.clone();
        kind.sources[0].kind = alpha2_model::MediaKind::Video;
        assert_eq!(stale_sources(&before, &kind), ["a".to_owned()].into());
    }

    #[test]
    fn a_new_locator_invalidates_only_that_source() {
        let before = project(&[("a", "file://a"), ("b", "file://b")], 0);
        let after = project(&[("a", "file://a"), ("b", "file://b-v2")], 0);
        assert_eq!(stale_sources(&before, &after), ["b".to_owned()].into());
    }
}
